//! The spend report (`archon cost`). It sums `spend_daily`, the rows the
//! daily budget sums for one day, and over every day they add up to the
//! `spend` rows the total budget sums: a report counts money as they do.

use archon_core::cost::{CostAmounts, CostGroup, CostReport, CostRow, CostSplit, DayRange};
use archon_core::error::ArchonError;
use chrono::NaiveDate;
use rusqlite::{Connection, params};

use super::codec::{SqlContext, end_read_tx, read_tx};

/// Rows and total are summed separately, so they may differ by rounding.
const ROUNDING_USD: f64 = 1e-6;

/// Walk once per spending delegate. The unique parent link and the stop at
/// the first stage give at most one match. UNION also terminates stale cycles.
const STAGE_OWNERS: &str = "
    WITH RECURSIVE chain(spender, candidate) AS (
        SELECT DISTINCT d.agent, d.agent FROM spend_daily d
        JOIN delegated_agents a ON a.agent = d.agent
        WHERE d.day BETWEEN ?1 AND ?2
        UNION
        SELECT c.spender, a.parent FROM chain c
        JOIN delegated_agents a ON a.agent = c.candidate
        LEFT JOIN stage_runs st ON st.aether_agent = c.candidate
        WHERE st.aether_agent IS NULL
    ),
    stage_owners AS (
        SELECT c.spender, c.candidate FROM chain c
        JOIN stage_runs st ON st.aether_agent = c.candidate
    )";

const FROM_OWNED: &str = "FROM spend_daily d
    JOIN spend s ON s.agent = d.agent
    LEFT JOIN delegated_agents a ON a.agent = d.agent
    LEFT JOIN stage_owners owner ON owner.spender = d.agent
    LEFT JOIN stage_runs st ON st.aether_agent = COALESCE(owner.candidate, d.agent)
    LEFT JOIN runs run ON run.id = st.run
    LEFT JOIN items i ON i.id = COALESCE(run.item, s.item)
    LEFT JOIN repos r ON r.id = i.repo
    WHERE d.day BETWEEN ?1 AND ?2";

// The ledger keeps orchestrator spend and older, unlinked helpers without items.
const ORCHESTRATOR_LEAD: &str = "st.aether_agent IS NULL AND a.agent IS NULL AND s.item IS NULL";

/// Ledger days carry the provenance each charge was observed or settled with.
const FROM_DAYS: &str = "FROM spend_daily d
    WHERE d.day BETWEEN ?1 AND ?2";

const SUMS_SQL: &str = "COALESCE(SUM(d.cost_usd), 0),
    COALESCE(SUM(d.estimated_cost_usd), 0),
    COALESCE(SUM(d.billed_cost_usd), 0),
    COALESCE(SUM(d.list_price_cost_usd), 0),
    COALESCE(SUM(d.unclassified_cost_usd), 0)";

/// The SQL expression of a group's key. Fixed text, never caller input.
fn key_sql(by: CostGroup) -> String {
    let model = "json_extract(st.data, '$.choice.model.provider') || '/' || \
                 json_extract(st.data, '$.choice.model.model')";
    let provider = "json_extract(st.data, '$.choice.model.provider')";
    match by {
        CostGroup::Day => "d.day",
        CostGroup::Repo => "COALESCE(r.name, i.repo)",
        CostGroup::Pipeline => "i.pipeline",
        CostGroup::Item => "COALESCE(run.item, s.item)",
        CostGroup::Stage => {
            return "CASE WHEN st.stage_id IS NOT NULL THEN st.stage_id
            WHEN s.item IS NULL THEN '(orchestrator)' END"
                .into();
        }
        CostGroup::Model => {
            return format!(
                "CASE WHEN {ORCHESTRATOR_LEAD} THEN '(orchestrator)'
            WHEN a.agent IS NOT NULL THEN a.model ELSE {model} END"
            );
        }
        CostGroup::Provider => {
            return format!(
                "CASE WHEN {ORCHESTRATOR_LEAD} THEN '(orchestrator)'
            WHEN a.agent IS NOT NULL THEN substr(a.model, 1, instr(a.model, '/') - 1)
            ELSE {provider} END"
            );
        }
        CostGroup::Effort => {
            return format!(
                "CASE WHEN {ORCHESTRATOR_LEAD} THEN '(orchestrator)'
            WHEN a.agent IS NOT NULL THEN a.effort
            ELSE replace(json_extract(st.data, '$.choice.effort'), 'xhigh', 'x_high') END"
            );
        }
    }
    .into()
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
        "{STAGE_OWNERS},
         entries AS (
             SELECT d.*, {} AS key,
                 CASE WHEN a.agent IS NOT NULL THEN 'delegated'
                      WHEN st.aether_agent IS NOT NULL OR ({ORCHESTRATOR_LEAD}) THEN 'lead' END AS role,
                 d.cost_usd + CASE WHEN ?3 THEN d.estimated_cost_usd ELSE 0 END AS counted
             {FROM_OWNED}
         )
         SELECT key, {SUMS_SQL},
             COALESCE(SUM(counted) FILTER (WHERE role = 'lead'), 0),
             COALESCE(SUM(counted) FILTER (WHERE role = 'delegated'), 0)
         FROM entries d GROUP BY key ORDER BY key",
        key_sql(by)
    );
    let mut stmt = conn.prepare_cached(&sql).ctx("prepare the spend report")?;
    let rows = stmt
        .query_map(
            params![day_text(range.since), day_text(range.until), estimates],
            |r| {
                let amounts = amounts(
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    estimates,
                );
                let split = CostSplit {
                    lead_usd: r.get(6)?,
                    delegated_usd: r.get(7)?,
                };
                Ok((r.get::<_, Option<String>>(0)?, amounts, split))
            },
        )
        .ctx("group spend")?;
    let mut out = Vec::new();
    for row in rows {
        let (key, amounts, split) = row.ctx("read grouped spend")?;
        assert!(
            by != CostGroup::Day || key.is_some(),
            "every ledger day has a date"
        );
        let amounts = amounts?;
        check_split(split, amounts.counted_usd)?;
        out.push(CostRow {
            key,
            amounts,
            split: Some(split),
        });
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

fn check_split(split: CostSplit, counted: f64) -> Result<(), ArchonError> {
    let parts = [split.lead_usd, split.delegated_usd];
    if parts.iter().any(|v| !v.is_finite() || *v < 0.0)
        || parts.iter().sum::<f64>() - counted > ROUNDING_USD * counted.max(1.0)
    {
        return Err(ArchonError::Store(format!(
            "lead and delegated spend {parts:?} exceed or are invalid for the counted amount {counted}"
        )));
    }
    Ok(())
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
    let mut split = CostSplit::default();
    for row in &rows {
        let part = row.split.expect("the query provides a split for every row");
        split.lead_usd += part.lead_usd;
        split.delegated_usd += part.delegated_usd;
    }
    end_read_tx(tx, "end the spend report")?;
    check_sums(&rows, &total)?;
    check_split(split, total.counted_usd)?;
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
        split: Some(split),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, counted: f64) -> CostRow {
        CostRow {
            key: Some(key.into()),
            amounts: amounts(counted, 0.0, counted, 0.0, 0.0, true).unwrap(),
            split: None,
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
