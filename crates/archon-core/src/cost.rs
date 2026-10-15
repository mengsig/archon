//! The spend report behind `archon cost` and the `cost_report` tool
//! (ARCHITECTURE §5.3). The ledger keeps spend by local calendar day, the
//! day the daily budget counts, so a report covers whole days.

use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::age::Age;
use crate::error::ArchonError;

/// Provenance of Aether's cumulative dollar amount. Missing legacy provenance
/// is unknown; mixed totals cannot be split into billed and estimated dollars.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    #[default]
    Unknown,
    ListPrice,
    ProviderReported,
    Mixed,
}

impl CostBasis {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ListPrice => "list_price",
            Self::ProviderReported => "provider_reported",
            Self::Mixed => "mixed",
        }
    }
}

/// Dollars for operator-facing budget messages. Positive sub-cent amounts
/// stay visible; the numeric values in records and JSON retain full precision.
pub fn format_usd(amount: f64) -> String {
    if amount > 0.0 && amount < 0.01 {
        "<$0.01".into()
    } else {
        format!("${:.2}", if amount == 0.0 { 0.0 } else { amount })
    }
}

/// The `estimates` argument of spend reads that learn from or report what work
/// cost: the API-equivalent total, so unbilled subscription usage is never free.
pub const API_EQUIVALENT: bool = true;

/// Days a report covers when it names no start: the last week, today included.
pub const DEFAULT_DAYS: u32 = 7;

const SECS_PER_DAY: u64 = 86_400;

/// What a report sums spend by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostGroup {
    #[default]
    Day,
    Repo,
    Pipeline,
    Model,
    Item,
    Stage,
}

impl CostGroup {
    pub const ALL: [Self; 6] = [
        Self::Day,
        Self::Repo,
        Self::Pipeline,
        Self::Model,
        Self::Item,
        Self::Stage,
    ];

    /// The wire name, e.g. `pipeline`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Repo => "repo",
            Self::Pipeline => "pipeline",
            Self::Model => "model",
            Self::Item => "item",
            Self::Stage => "stage",
        }
    }
}

/// One end of a report's range: a local calendar date, or a number of
/// whole days back from today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DayBound {
    Date(NaiveDate),
    /// As a start, the last `n` days with today; as an end, up to the day
    /// before those, so `since 7d` and `until 7d` split time without overlap.
    Days(u32),
}

impl DayBound {
    /// `YYYY-MM-DD`, or whole days as an age: `7d`, `2w`.
    pub fn parse(text: &str) -> Result<Self, ArchonError> {
        let text = text.trim();
        if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            return Ok(Self::Date(date));
        }
        let age = Age::parse(text).map_err(|_| {
            ArchonError::Invalid(format!(
                "{text:?} is neither a date like 2026-09-01 nor a number of days like 7d or 2w"
            ))
        })?;
        if !age.secs().is_multiple_of(SECS_PER_DAY) {
            return Err(ArchonError::Invalid(format!(
                "{text:?} is not a whole number of days: spend is kept by calendar day, so \
                 use days like 7d or 2w, or a date"
            )));
        }
        let days = u32::try_from(age.secs() / SECS_PER_DAY)
            .map_err(|_| ArchonError::Invalid(format!("{text:?} is too long")))?;
        Ok(Self::Days(days))
    }

    fn first_day(self, today: NaiveDate) -> Result<NaiveDate, ArchonError> {
        match self {
            Self::Date(date) => Ok(date),
            Self::Days(0) => Err(ArchonError::Invalid(
                "a start of 0 days back covers no day; use 1d for today".into(),
            )),
            Self::Days(n) => days_before(today, n - 1),
        }
    }

    fn last_day(self, today: NaiveDate) -> Result<NaiveDate, ArchonError> {
        match self {
            Self::Date(date) => Ok(date),
            Self::Days(n) => days_before(today, n),
        }
    }
}

fn days_before(today: NaiveDate, n: u32) -> Result<NaiveDate, ArchonError> {
    today
        .checked_sub_days(Days::new(u64::from(n)))
        .ok_or_else(|| ArchonError::Invalid(format!("{n} days before {today} is out of range")))
}

/// `Request::CostReport`: spend over a range of days, grouped by `by`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostQuery {
    /// `None`: the last [`DEFAULT_DAYS`] days.
    pub since: Option<DayBound>,
    /// `None`: today.
    pub until: Option<DayBound>,
    #[serde(default)]
    pub by: CostGroup,
}

impl CostQuery {
    /// The inclusive range of local days the query covers, given `today`.
    pub fn range(&self, today: NaiveDate) -> Result<DayRange, ArchonError> {
        let since = self
            .since
            .unwrap_or(DayBound::Days(DEFAULT_DAYS))
            .first_day(today)?;
        let until = match self.until {
            Some(bound) => bound.last_day(today)?,
            None => today,
        };
        if since > until {
            return Err(ArchonError::Invalid(format!(
                "the range starts on {since}, after it ends on {until}"
            )));
        }
        let range = DayRange { since, until };
        debug_assert!(range.since <= range.until);
        Ok(range)
    }
}

/// Local calendar days, both ends included; never empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayRange {
    pub since: NaiveDate,
    pub until: NaiveDate,
}

/// Money in one group. Classification does not change budget accounting:
/// `counted_usd` includes raw Aether `cost_usd` regardless of its basis, plus
/// raw `estimated_cost_usd` when `limits.count_estimated_cost` is on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CostAmounts {
    pub counted_usd: f64,
    /// Provider-reported billing only.
    pub cost_usd: f64,
    /// List-price cost plus the separate API-equivalent estimate.
    pub estimated_cost_usd: f64,
    /// Legacy/unknown and mixed-basis cost whose split is unavailable.
    #[serde(default)]
    pub unclassified_cost_usd: f64,
}

impl CostAmounts {
    /// Every dollar Aether reported or estimated: what the work would cost at
    /// API prices, whether or not budgets count the estimates.
    pub fn api_equivalent_usd(&self) -> f64 {
        let parts = [
            self.cost_usd,
            self.estimated_cost_usd,
            self.unclassified_cost_usd,
        ];
        assert!(parts.iter().all(|v| v.is_finite() && *v >= 0.0));
        let total = parts.iter().sum::<f64>();
        debug_assert!(total + 1e-9 >= self.counted_usd, "counted is a part");
        total
    }

    /// The API-equivalent total when the counted amount leaves out at least
    /// half a cent of estimates, as budgets with `count_estimated_cost` off do.
    pub fn uncounted_api_equivalent_usd(&self) -> Option<f64> {
        let total = self.api_equivalent_usd();
        (total - self.counted_usd >= 0.005).then_some(total)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRow {
    /// The day, repository name, pipeline, `provider/model`, item id or
    /// stage id. `None` gathers spend with no such owner: the
    /// orchestrator's, and agents outside a stage for model and stage.
    pub key: Option<String>,
    #[serde(flatten)]
    pub amounts: CostAmounts,
}

/// `Reply::CostReport`. Days in order for `by = day`; otherwise the most
/// counted first. The rows add up to `total`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostReport {
    pub range: DayRange,
    pub by: CostGroup,
    /// Whether the separate API-equivalent estimates count toward budgets.
    pub estimates_counted: bool,
    pub rows: Vec<CostRow>,
    pub total: CostAmounts,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn range(since: Option<&str>, until: Option<&str>) -> Result<DayRange, ArchonError> {
        let query = CostQuery {
            since: since.map(|s| DayBound::parse(s).unwrap()),
            until: until.map(|s| DayBound::parse(s).unwrap()),
            by: CostGroup::Day,
        };
        query.range(date("2026-09-29"))
    }

    #[test]
    fn older_cost_reports_can_be_read_without_an_unclassified_field() {
        let amounts: CostAmounts = serde_json::from_value(serde_json::json!({
            "counted_usd": 2.5, "cost_usd": 2.0, "estimated_cost_usd": 0.5,
        }))
        .unwrap();
        assert_eq!(amounts.unclassified_cost_usd, 0.0);
        assert_eq!(amounts.counted_usd, 2.5);
        for basis in [
            CostBasis::Unknown,
            CostBasis::ListPrice,
            CostBasis::ProviderReported,
            CostBasis::Mixed,
        ] {
            assert_eq!(serde_json::to_value(basis).unwrap(), basis.name());
        }
    }

    #[test]
    fn only_uncounted_estimates_add_an_api_equivalent_total() {
        // A Claude lead billed $2 and its Codex reviewer was estimated at $0.75.
        let mut amounts = CostAmounts {
            counted_usd: 2.75,
            cost_usd: 2.0,
            estimated_cost_usd: 0.75,
            unclassified_cost_usd: 0.0,
        };
        assert_eq!(amounts.api_equivalent_usd(), 2.75);
        assert_eq!(amounts.uncounted_api_equivalent_usd(), None);
        amounts.counted_usd = 2.0;
        assert_eq!(amounts.uncounted_api_equivalent_usd(), Some(2.75));
        amounts.counted_usd = 2.75 - 1e-12;
        assert_eq!(amounts.uncounted_api_equivalent_usd(), None, "rounding");
    }

    #[test]
    fn bounds_are_dates_or_whole_days() {
        assert_eq!(
            DayBound::parse(" 2026-09-01 ").unwrap(),
            DayBound::Date(date("2026-09-01"))
        );
        assert_eq!(DayBound::parse("7d").unwrap(), DayBound::Days(7));
        assert_eq!(DayBound::parse("2w").unwrap(), DayBound::Days(14));
        assert_eq!(DayBound::parse("0d").unwrap(), DayBound::Days(0));
        for bad in ["12h", "36h", "yesterday", "2026-13-01", "7", ""] {
            assert!(DayBound::parse(bad).is_err(), "{bad}");
        }
        let hours = DayBound::parse("12h").unwrap_err().to_string();
        assert!(hours.contains("calendar day"), "{hours}");
    }

    #[test]
    fn the_default_is_the_last_seven_days_with_today() {
        let week = range(None, None).unwrap();
        assert_eq!(week.since, date("2026-09-23"));
        assert_eq!(week.until, date("2026-09-29"));
        let today = range(Some("1d"), None).unwrap();
        assert_eq!((today.since, today.until), (week.until, week.until));
    }

    #[test]
    fn a_duration_splits_time_the_same_way_at_either_end() {
        let before = range(Some("2026-09-01"), Some("7d")).unwrap();
        assert_eq!(before.until, date("2026-09-22"));
        assert_eq!(range(None, None).unwrap().since, date("2026-09-23"));
        let yesterday = range(Some("2d"), Some("1d")).unwrap();
        assert_eq!(yesterday.since, yesterday.until);
        assert_eq!(yesterday.until, date("2026-09-28"));
    }

    #[test]
    fn a_backwards_or_empty_range_is_refused() {
        let backwards = range(Some("2026-09-10"), Some("2026-09-01")).unwrap_err();
        assert!(matches!(backwards, ArchonError::Invalid(_)), "{backwards}");
        assert!(range(Some("7d"), Some("7d")).is_err());
        assert!(range(Some("0d"), None).is_err());
        assert!(range(Some("4000000000d"), None).is_err());
    }

    #[test]
    fn a_query_round_trips_on_the_wire() {
        let query = CostQuery {
            since: Some(DayBound::Date(date("2026-09-01"))),
            until: Some(DayBound::Days(1)),
            by: CostGroup::Model,
        };
        let wire = serde_json::to_value(query).unwrap();
        assert_eq!(wire["by"], "model");
        assert_eq!(wire["since"]["date"], "2026-09-01");
        assert_eq!(serde_json::from_value::<CostQuery>(wire).unwrap(), query);
        let bare: CostQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(bare, CostQuery::default());
        for group in CostGroup::ALL {
            assert_eq!(serde_json::to_value(group).unwrap(), group.name());
        }
    }
}

#[cfg(test)]
mod amount_tests {
    use super::format_usd;

    #[test]
    fn budget_amounts_keep_positive_fractions_visible() {
        for amount in [f64::MIN_POSITIVE, 0.000001, 0.001, 0.009999] {
            assert_eq!(format_usd(amount), "<$0.01");
        }
        for (amount, expected) in [
            (0.0, "$0.00"),
            (-0.0, "$0.00"),
            (0.01, "$0.01"),
            (1.23, "$1.23"),
            (100.0, "$100.00"),
        ] {
            assert_eq!(format_usd(amount), expected);
        }
    }
}
