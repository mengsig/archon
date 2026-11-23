use archon_core::Timestamp;
use archon_core::error::ArchonError;
use archon_core::model::Effort;
use rusqlite::{Connection, params};

use super::codec::SqlContext;

const MIN_SAMPLES: usize = 10;
const MAX_SAMPLES: usize = 100;

pub(super) fn p90(
    conn: &Connection,
    pipeline: &str,
    stage: &str,
    effort: Effort,
) -> Result<Option<u64>, ArchonError> {
    assert!(!pipeline.is_empty());
    assert!(!stage.is_empty());
    let sql = "SELECT st.started_at, json_extract(st.data, '$.finished_at')
         FROM stage_runs st JOIN runs r ON r.id = st.run
         WHERE r.pipeline = ?1 AND st.stage_id = ?2 AND st.state = 'completed'
           AND COALESCE(json_extract(st.data, '$.infrastructure_failure'), 0) = 0
           AND replace(json_extract(st.data, '$.choice.effort'), 'xhigh', 'x_high') = ?3
           AND NOT EXISTS (
               SELECT 1 FROM json_each(st.data, '$.model_segments') AS segment
               WHERE replace(json_extract(segment.value, '$.choice.effort'), 'xhigh', 'x_high') != ?3
           )
         ORDER BY st.started_at DESC, st.id DESC LIMIT ?4";
    let mut statement = conn.prepare_cached(sql).ctx("prepare stage timing")?;
    let mut rows = statement
        .query(params![
            pipeline,
            stage,
            effort.to_string(),
            MAX_SAMPLES as i64
        ])
        .ctx("query stage timing")?;
    let mut samples = Vec::new();
    while let Some(row) = rows.next().ctx("read stage timing")? {
        let started = row.get::<_, String>(0).ctx("read stage start")?;
        let finished = row.get::<_, String>(1).ctx("read stage finish")?;
        samples.push(seconds(&started, &finished)?);
    }
    if samples.len() < MIN_SAMPLES {
        return Ok(None);
    }
    assert!(samples.len() <= MAX_SAMPLES, "the query limits samples");
    samples.sort_unstable();
    let rank = (samples.len() * 9).div_ceil(10);
    Ok(Some(samples[rank - 1]))
}

fn seconds(started: &str, finished: &str) -> Result<u64, ArchonError> {
    let parse = |raw: &str| {
        raw.parse::<Timestamp>()
            .map_err(|error| ArchonError::Corrupt(format!("stage timing timestamp {raw}: {error}")))
    };
    let elapsed = parse(finished)? - parse(started)?;
    let duration = elapsed
        .to_std()
        .map_err(|_| ArchonError::Corrupt("stage timing finishes before it starts".into()))?;
    Ok((duration.as_secs() + u64::from(duration.subsec_nanos() > 0)).max(1))
}
