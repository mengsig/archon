//! The spend report (`archon cost`). It sums `spend_daily`, the rows the
//! daily budget sums for one day, and over every day they add up to the
//! `spend` rows the total budget sums: a report counts money as they do.

use archon_core::cost::{CostAmounts, CostGroup, CostReport, CostRow, DayRange};
use archon_core::error::ArchonError;
use chrono::NaiveDate;
use rusqlite::{Connection, params};

use super::codec::{SqlContext, end_read_tx, read_tx};

/// Rows and total are summed separately, so they may differ by rounding.
const ROUNDING_USD: f64 = 1e-6;

/// Each agent's day joined to what owns the agent. Every join matches at
/// most one row (`stage_runs.aether_agent` is unique), so none repeats spend.
const FROM_OWNED: &str = "FROM spend_daily d
    JOIN spend s ON s.agent = d.agent
    LEFT JOIN items i ON i.id = s.item
    LEFT JOIN repos r ON r.id = i.repo
    LEFT JOIN stage_runs st ON st.aether_agent = d.agent
    WHERE d.day BETWEEN ?1 AND ?2";

/// Ledger days carry the provenance each charge was observed or settled with.
const FROM_DAYS: &str = "FROM spend_daily d
    WHERE d.day BETWEEN ?1 AND ?2";

const SUMS_SQL: &str = "COALESCE(SUM(d.cost_usd), 0),
    COALESCE(SUM(d.estimated_cost_usd), 0),
    COALESCE(SUM(d.billed_cost_usd), 0),
    COALESCE(SUM(d.list_price_cost_usd), 0),
    COALESCE(SUM(d.unclassified_cost_usd), 0)";

/// The rows a group's key reads from: only a key that names an owner needs
/// the joins.
fn from_sql(by: CostGroup) -> &'static str {
    match by {
        CostGroup::Day => FROM_DAYS,
        _ => FROM_OWNED,
    }
}

/// The SQL expression of a group's key. Fixed text, never caller input.
fn key_sql(by: CostGroup) -> &'static str {
    match by {
        CostGroup::Day => "d.day",
        CostGroup::Repo => "COALESCE(r.name, i.repo)",
        CostGroup::Pipeline => "i.pipeline",
        CostGroup::Model => {
            "json_extract(st.data, '$.choice.model.provider') || '/' || \
             json_extract(st.data, '$.choice.model.model')"
        }
        CostGroup::Provider => "json_extract(st.data, '$.choice.model.provider')",
        CostGroup::Item => "s.item",
        CostGroup::Stage => "st.stage_id",
    }
}

fn day_text(day: NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

fn amounts(
    cost: f64,
    estimate: f64,
    billed: f64,
    list_price: f64,
    unclassified: f64,
    estimates: bool,
) -> Result<CostAmounts, ArchonError> {
    if [cost, estimate, billed, list_price, unclassified]
        .iter()
        .any(|v| !v.is_finite() || *v < 0.0)
    {
        return Err(ArchonError::Store(format!(
            "spend sums to {cost} / {estimate} (billed {billed}, list-price {list_price}, \
             unclassified {unclassified}), not non-negative finite amounts"
        )));
    }
    let counted = if estimates { cost + estimate } else { cost };
    let estimated = estimate + list_price;
    if !counted.is_finite() || !estimated.is_finite() {
        return Err(ArchonError::Store("spend total overflowed".into()));
    }
    Ok(CostAmounts {
        counted_usd: counted,
        cost_usd: billed,
        estimated_cost_usd: estimated,
        unclassified_cost_usd: unclassified,
    })
}

fn grouped(
    conn: &Connection,
    range: &DayRange,
    by: CostGroup,
    estimates: bool,
) -> Result<Vec<CostRow>, ArchonError> {
    let sql = format!(
        "SELECT {} AS key, {SUMS_SQL} {}
         GROUP BY key ORDER BY key",
        key_sql(by),
        from_sql(by)
    );
    let mut stmt = conn.prepare_cached(&sql).ctx("prepare the spend report")?;
    let rows = stmt
        .query_map(params![day_text(range.since), day_text(range.until)], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .ctx("group spend")?;
    let mut out = Vec::new();
    for row in rows {
        let (key, cost, estimate, billed, list_price, unclassified): (
            Option<String>,
            f64,
            f64,
            f64,
            f64,
            f64,
        ) = row.ctx("read grouped spend")?;
        assert!(
            by != CostGroup::Day || key.is_some(),
            "every ledger day has a date"
        );
        let amounts = amounts(cost, estimate, billed, list_price, unclassified, estimates)?;
        out.push(CostRow { key, amounts });
    }
    if by != CostGroup::Day {
        // Most counted first; the ownerless group last among equals.
        out.sort_by(|a, b| {
            let counted = b.amounts.counted_usd.total_cmp(&a.amounts.counted_usd);
            counted.then_with(|| a.key.is_none().cmp(&b.key.is_none()))
        });
    }
    Ok(out)
}

fn total(conn: &Connection, range: &DayRange, estimates: bool) -> Result<CostAmounts, ArchonError> {
    let sql = format!("SELECT {SUMS_SQL} {FROM_DAYS}");
    let (cost, estimate, billed, list_price, unclassified) = conn
        .query_row(
            &sql,
            params![day_text(range.since), day_text(range.until)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ctx("sum spend over the range")?;
    amounts(cost, estimate, billed, list_price, unclassified, estimates)
}

/// Refuses a report whose rows do not add up to its total: a join that
/// dropped or repeated ledger rows would misstate spend.
fn check_sums(rows: &[CostRow], total: &CostAmounts) -> Result<(), ArchonError> {
    let mut sum = CostAmounts::default();
    for row in rows {
        sum.counted_usd += row.amounts.counted_usd;
        sum.cost_usd += row.amounts.cost_usd;
        sum.estimated_cost_usd += row.amounts.estimated_cost_usd;
        sum.unclassified_cost_usd += row.amounts.unclassified_cost_usd;
    }
    let parts = [
        (
            "unclassified",
            sum.unclassified_cost_usd,
            total.unclassified_cost_usd,
        ),
        ("counted", sum.counted_usd, total.counted_usd),
        ("cost", sum.cost_usd, total.cost_usd),
        (
            "estimated",
            sum.estimated_cost_usd,
            total.estimated_cost_usd,
        ),
    ];
    for (name, rows_sum, expected) in parts {
        if (rows_sum - expected).abs() > ROUNDING_USD * expected.max(1.0) {
            return Err(ArchonError::Store(format!(
                "the spend report's {name} rows add up to {rows_sum}, not its total {expected}"
            )));
        }
    }
    Ok(())
}

/// Spend over `range` grouped `by`; `estimates` is whether they count
/// toward the budgets. Reads rows and total in one snapshot.
pub(super) fn report(
    conn: &Connection,
    range: &DayRange,
    by: CostGroup,
    estimates: bool,
) -> Result<CostReport, ArchonError> {
    if range.since > range.until {
        return Err(ArchonError::Invalid(format!(
            "the range starts on {}, after it ends on {}",
            range.since, range.until
        )));
    }
    let tx = read_tx(conn)?;
    let rows = grouped(conn, range, by, estimates)?;
    let total = total(conn, range, estimates)?;
    end_read_tx(tx, "end the spend report")?;
    check_sums(&rows, &total)?;
    debug_assert!(
        by != CostGroup::Day || rows.windows(2).all(|w| w[0].key < w[1].key),
        "days come once each, in order"
    );
    Ok(CostReport {
        range: *range,
        by,
        estimates_counted: estimates,
        rows,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, counted: f64) -> CostRow {
        CostRow {
            key: Some(key.into()),
            amounts: amounts(counted, 0.0, counted, 0.0, 0.0, true).unwrap(),
        }
    }

    #[test]
    fn rows_that_miss_the_total_are_refused_beyond_rounding() {
        let rows = [row("a", 0.1), row("b", 0.2)];
        let total = amounts(0.3, 0.0, 0.3, 0.0, 0.0, true).unwrap();
        check_sums(&rows, &total).expect("0.1 + 0.2 is 0.3 up to rounding");
        let short = amounts(0.4, 0.0, 0.4, 0.0, 0.0, true).unwrap();
        let refused = check_sums(&rows, &short).unwrap_err().to_string();
        assert!(refused.contains("add up to"), "{refused}");
        assert!(amounts(f64::NAN, 0.0, 0.0, 0.0, 0.0, true).is_err());
        assert!(amounts(-1.0, 0.0, 0.0, 0.0, 0.0, true).is_err());
    }
}
