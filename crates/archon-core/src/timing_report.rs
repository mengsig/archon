//! Observed durations across inclusive UTC calendar days.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::cost::{CostQuery, DayBound, DayRange};
use crate::error::ArchonError;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct TimingQuery {
    pub since: Option<DayBound>,
    pub until: Option<DayBound>,
}

impl TimingQuery {
    pub fn range(&self, today: NaiveDate) -> Result<DayRange, ArchonError> {
        CostQuery {
            since: self.since,
            until: self.until,
            ..CostQuery::default()
        }
        .range(today)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TimingReport {
    pub range: DayRange,
    /// Recorded queue intervals by the reason observed at dispatch.
    pub queue_ms: BTreeMap<String, u64>,
    /// Stage wall time with parallel slots counted once per run.
    pub stage_ms: u64,
    /// Dispatch provisioning and time inside a run outside its stages.
    pub gap_ms: u64,
    /// The part of stage time spent on repeated attempts or repeated runs.
    pub retry_ms: u64,
    /// Time from creation to merge for each PR with both timestamps recorded.
    pub merge_wait_ms: u64,
    /// Time from first observed check start to last observed complete snapshot.
    pub ci_wait_ms: u64,
}

impl TimingReport {
    pub fn text(&self) -> String {
        let mut text = format!(
            "Observed timing (UTC {} through {}, milliseconds)\n",
            self.range.since, self.range.until
        );
        for (reason, duration) in &self.queue_ms {
            writeln!(text, "queue {reason}: {duration}").expect("write to String");
        }
        writeln!(
            text,
            "stage: {}\ngap: {}\nretry: {}\nmerge wait: {}\nCI wait: {}",
            self.stage_ms, self.gap_ms, self.retry_ms, self.merge_wait_ms, self.ci_wait_ms
        )
        .expect("write to String");
        text.push_str(concat!(
            "Totals use recorded intervals. Gaps cover dispatch provisioning and time within runs outside stages.\n",
            "Queue reasons follow dispatch observations. Other includes unclassified queue time.\n",
            "Retry time is included in stage time. Merge wait covers PR creation through merge.\n",
            "CI wait covers the first observed start through the last complete checks snapshot, including gaps between reruns.\n",
            "CI and merge waits may overlap each other and run time.\n",
        ));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_uses_inclusive_utc_days_and_rejects_reversed_bounds() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let query = TimingQuery {
            since: Some(DayBound::parse("7d").unwrap()),
            until: Some(DayBound::parse("1d").unwrap()),
        };
        let range = query.range(today).unwrap();
        assert_eq!(range.since.to_string(), "2026-09-04");
        assert_eq!(range.until.to_string(), "2026-09-09");
        assert!(
            TimingQuery {
                since: Some(DayBound::parse("2026-09-10").unwrap()),
                until: Some(DayBound::parse("2026-09-09").unwrap()),
            }
            .range(today)
            .is_err()
        );
    }
}
