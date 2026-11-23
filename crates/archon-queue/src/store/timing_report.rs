use archon_core::Timestamp;
use archon_core::cost::DayRange;
use archon_core::error::ArchonError;
use archon_core::timing_report::TimingReport;
use rusqlite::{Connection, params};

use super::Store;
use super::codec::{SqlContext, end_read_tx, read_tx, ts};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Interval {
    start: Timestamp,
    end: Timestamp,
}

impl Interval {
    fn intersect(self, start: Timestamp, end: Timestamp) -> Option<Self> {
        let start = self.start.max(start);
        let end = self.end.min(end);
        (start < end).then_some(Self { start, end })
    }

    fn millis(self) -> u64 {
        assert!(self.start < self.end, "a measured interval is positive");
        (self.end - self.start).num_milliseconds() as u64
    }
}

impl Store {
    /// Observed timing in the supplied inclusive UTC dates.
    pub fn timing_report(&self, range: &DayRange) -> Result<TimingReport, ArchonError> {
        self.timing_report_at(range, archon_core::now())
    }

    fn timing_report_at(
        &self,
        range: &DayRange,
        now: Timestamp,
    ) -> Result<TimingReport, ArchonError> {
        if range.since > range.until {
            return Err(ArchonError::Invalid(
                "timing range starts after it ends".into(),
            ));
        }
        let midnight = |day: chrono::NaiveDate| day.and_time(chrono::NaiveTime::MIN).and_utc();
        let end =
            range.until.succ_opt().map(midnight).ok_or_else(|| {
                ArchonError::Invalid("timing range end has no following day".into())
            })?;
        let window = Interval {
            start: midnight(range.since),
            end: end.min(now),
        };
        let mut report = TimingReport {
            range: *range,
            queue_ms: [
                "dependency",
                "cap",
                "budget",
                "pause",
                "quota",
                "ready",
                "other",
            ]
            .into_iter()
            .map(|reason| (reason.to_owned(), 0))
            .collect(),
            stage_ms: 0,
            gap_ms: 0,
            retry_ms: 0,
            merge_wait_ms: 0,
            ci_wait_ms: 0,
        };
        if window.start < window.end {
            let tx = read_tx(&self.conn)?;
            read_queue(&self.conn, window, &mut report)?;
            report.gap_ms += dispatch_gaps(&self.conn, window)?;
            read_runs(&self.conn, window, &mut report)?;
            read_pull_requests(&self.conn, window, &mut report)?;
            end_read_tx(tx, "finish timing report snapshot")?;
        }
        Ok(report)
    }
}

fn dispatch_gaps(conn: &Connection, window: Interval) -> Result<u64, ArchonError> {
    assert!(
        window.start < window.end,
        "the dispatch query has a non-empty window"
    );
    let end = ts(&window.end);
    let mut statement = conn
        .prepare(
            "WITH dispatch AS (
             SELECT t.item, t.at, coalesce(
                 (SELECT n.at FROM item_transitions n
                  WHERE n.item = t.item AND n.seq > t.seq ORDER BY n.seq LIMIT 1), ?2
             ) AS next_at
             FROM item_transitions t WHERE t.to_state = 'dispatching' AND t.at < ?2
         )
         SELECT at, coalesce(
             (SELECT min(r.started_at) FROM runs r
              WHERE r.item = d.item AND r.started_at >= d.at AND r.started_at < d.next_at),
             next_at)
         FROM dispatch d WHERE next_at > ?1",
        )
        .ctx("prepare dispatch timing")?;
    let mut rows = statement
        .query(params![ts(&window.start), end])
        .ctx("query dispatch timing")?;
    let mut total = 0;
    while let Some(row) = rows.next().ctx("read dispatch timing")? {
        total += row_interval(row, 0, window)?.map_or(0, Interval::millis);
    }
    Ok(total)
}

fn timestamp(raw: &str) -> Result<Timestamp, ArchonError> {
    raw.parse()
        .map_err(|error| ArchonError::Corrupt(format!("timing timestamp {raw:?}: {error}")))
}

fn read_queue(
    conn: &Connection,
    window: Interval,
    report: &mut TimingReport,
) -> Result<(), ArchonError> {
    assert!(
        window.start < window.end,
        "the queue query has a non-empty window"
    );
    let mut statement = conn
        .prepare(
            "SELECT reason, started_at, ended_at FROM queue_reason_intervals
         WHERE started_at < ?2 AND (ended_at > ?1 OR ended_at IS NULL)",
        )
        .ctx("prepare queue timing")?;
    let mut rows = statement
        .query(params![ts(&window.start), ts(&window.end)])
        .ctx("query queue timing")?;
    while let Some(row) = rows.next().ctx("read queue timing")? {
        let reason: String = row.get(0).ctx("read queue timing reason")?;
        let start: String = row.get(1).ctx("read queue timing start")?;
        let end: Option<String> = row.get(2).ctx("read queue timing end")?;
        let start = timestamp(&start)?;
        let end = end
            .as_deref()
            .map(timestamp)
            .transpose()?
            .unwrap_or(window.end);
        if end < start {
            return Err(ArchonError::Corrupt(format!(
                "queue timing ends before it starts ({start} to {end})"
            )));
        }
        if let Some(interval) = window.intersect(start, end) {
            *report.queue_ms.entry(reason).or_default() += interval.millis();
        }
    }
    Ok(())
}

fn read_runs(
    conn: &Connection,
    window: Interval,
    report: &mut TimingReport,
) -> Result<(), ArchonError> {
    assert!(
        window.start < window.end,
        "the run query has a non-empty window"
    );
    let mut runs = conn
        .prepare(
            "SELECT id, started_at, json_extract(data, '$.finished_at'),
                EXISTS (SELECT 1 FROM runs prior WHERE prior.item = r.item AND
                        (prior.started_at, prior.rowid) < (r.started_at, r.rowid))
         FROM runs r WHERE started_at < ?2 AND (terminal = 0 OR
            substr(json_extract(data, '$.finished_at'), 1, 10) >= ?1)",
        )
        .ctx("prepare run timing")?;
    let start_day = window.start.date_naive().to_string();
    let mut rows = runs
        .query(params![start_day, ts(&window.end)])
        .ctx("query run timing")?;
    let mut stages = conn
        .prepare(
            "SELECT started_at, json_extract(data, '$.finished_at'), attempt
         FROM stage_runs WHERE run = ?1 AND started_at < ?2",
        )
        .ctx("prepare stage timing")?;
    while let Some(row) = rows.next().ctx("read run timing")? {
        let id: String = row.get(0).ctx("read timed run id")?;
        let repeated_run: bool = row.get(3).ctx("read run repetition")?;
        let Some(run) = row_interval(row, 1, window)? else {
            continue;
        };
        let (stage, retry) = stage_durations(&mut stages, &id, run, repeated_run)?;
        assert!(
            retry <= stage && stage <= run.millis(),
            "run timing splits fit their envelopes"
        );
        report.stage_ms += stage;
        report.retry_ms += retry;
        report.gap_ms += run.millis() - stage;
    }
    Ok(())
}

fn row_interval(
    row: &rusqlite::Row<'_>,
    column: usize,
    bounds: Interval,
) -> Result<Option<Interval>, ArchonError> {
    let start: String = row.get(column).ctx("read execution start")?;
    let end: Option<String> = row.get(column + 1).ctx("read execution end")?;
    let end = end
        .as_deref()
        .map(timestamp)
        .transpose()?
        .unwrap_or(bounds.end);
    Ok(bounds.intersect(timestamp(&start)?, end))
}

fn stage_durations(
    statement: &mut rusqlite::Statement<'_>,
    run: &str,
    bounds: Interval,
    repeated_run: bool,
) -> Result<(u64, u64), ArchonError> {
    assert!(
        bounds.start < bounds.end,
        "a stage is clipped to an existing run"
    );
    let mut rows = statement
        .query(params![run, ts(&bounds.end)])
        .ctx("query stage timing")?;
    let mut all = Vec::new();
    let mut retries = Vec::new();
    while let Some(row) = rows.next().ctx("read stage timing")? {
        if let Some(interval) = row_interval(row, 0, bounds)? {
            let attempt: u32 = row.get(2).ctx("read timed stage attempt")?;
            all.push(interval);
            if !repeated_run && attempt > 1 {
                retries.push(interval);
            }
        }
    }
    let stage = union_millis(&mut all);
    let retry = if repeated_run {
        stage
    } else {
        union_millis(&mut retries)
    };
    assert!(retry <= stage, "retried stages are a subset of all stages");
    Ok((stage, retry))
}

fn union_millis(intervals: &mut [Interval]) -> u64 {
    intervals.sort_unstable();
    let mut total = 0;
    let Some((&first, rest)) = intervals.split_first() else {
        return total;
    };
    let mut merged = first;
    for &interval in rest {
        assert!(interval.start >= merged.start && interval.start < interval.end);
        if interval.start <= merged.end {
            merged.end = merged.end.max(interval.end);
        } else {
            total += merged.millis();
            merged = interval;
        }
    }
    total + merged.millis()
}

fn read_pull_requests(
    conn: &Connection,
    window: Interval,
    report: &mut TimingReport,
) -> Result<(), ArchonError> {
    assert!(
        window.start < window.end,
        "the PR query has a non-empty window"
    );
    let mut statement = conn.prepare(
        "SELECT min(created_at), min(merged_at), min(checks_started_at), max(checks_finished_at)
         FROM run_pull_requests
         GROUP BY host, repository, number
         HAVING min(merged_at) > ?1 OR max(checks_finished_at) > ?1",
    ).ctx("prepare pull request timing")?;
    let mut rows = statement
        .query([ts(&window.start)])
        .ctx("query pull request timing")?;
    while let Some(row) = rows.next().ctx("read pull request timing")? {
        report.merge_wait_ms += known_duration(row, 0, window)?;
        report.ci_wait_ms += known_duration(row, 2, window)?;
    }
    Ok(())
}

fn known_duration(
    row: &rusqlite::Row<'_>,
    column: usize,
    bounds: Interval,
) -> Result<u64, ArchonError> {
    let start: Option<String> = row.get(column).ctx("read pull request timing start")?;
    let end: Option<String> = row.get(column + 1).ctx("read pull request timing end")?;
    let (Some(start), Some(end)) = (start, end) else {
        return Ok(0);
    };
    let (start, end) = (timestamp(&start)?, timestamp(&end)?);
    Ok(bounds.intersect(start, end).map_or(0, Interval::millis))
}
