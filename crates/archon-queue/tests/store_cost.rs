//! The spend report: each grouping, the day bounds, rows that add up to
//! the total, and totals that match what the budgets count.

mod common;

use archon_core::aether_ids::AetherAgentId;
use archon_core::cost::{CostBasis, CostGroup, CostReport, DayRange};
use archon_core::error::ArchonError;
use archon_core::model::ModelChoice;
use archon_core::queue::QueueItem;
use archon_queue::Store;
use archon_queue::store::SpendRow;
use chrono::NaiveDate;
use common::{agent, item, repo, running_stage, start, store, t};

const DAY: i64 = 86_400;

/// The local calendar day of `t(secs)`, as the ledger records it.
fn day(secs: i64) -> NaiveDate {
    t(secs).with_timezone(&chrono::Local).date_naive()
}

fn range(from: i64, to: i64) -> DayRange {
    DayRange {
        since: day(from),
        until: day(to),
    }
}

fn charge(
    store: &mut Store,
    agent: &AetherAgentId,
    item: Option<&QueueItem>,
    at: i64,
    usd: (f64, f64),
) {
    let row = SpendRow {
        agent: agent.clone(),
        item: item.map(|i| i.id.clone()),
        cost_usd: usd.0,
        cost_basis: archon_core::cost::CostBasis::ProviderReported,
        cost_complete: None,
        estimated_cost_usd: usd.1,
        input_tokens: 1,
        output_tokens: 1,
        at: t(at),
    };
    store.record_spend(&row).unwrap();
}

/// Gives `item` a live run whose `stage` is run by agent `n` on `model`.
fn staged(store: &mut Store, item: &QueueItem, stage: &str, model: &str, n: u32) {
    let run = start(store, &item.id);
    let mut stage_run = running_stage(&run, 0, n);
    stage_run.stage_id = stage.into();
    stage_run.choice = ModelChoice {
        model: model.parse().unwrap(),
        ..stage_run.choice
    };
    store.upsert_stage_run(&stage_run).unwrap();
}

/// Three days of spend. Totals: cost $3.50, estimates $5.25.
/// - agent 1: item A (alpha, direct), stage implement on claude; $1 on
///   day 0, then $2 more on day 1.
/// - agent 2: item B (alpha, ask), stage review on codex; $4 estimated on day 1.
/// - agent 3: item C (no repo, ask) outside any stage; $0.50 + $0.25 on day 2.
/// - agent 4: the orchestrator, no item; $1 estimated on day 2.
fn seeded() -> (Store, [QueueItem; 3]) {
    let mut store = store();
    let alpha = repo("alpha");
    store.insert_repo(&alpha).unwrap();
    let a = store.insert_item(&item(Some(&alpha), "a"), None).unwrap();
    let b = QueueItem {
        pipeline: "ask".into(),
        ..item(Some(&alpha), "b")
    };
    let b = store.insert_item(&b, None).unwrap();
    let c = QueueItem {
        pipeline: "ask".into(),
        ..item(None, "c")
    };
    let c = store.insert_item(&c, None).unwrap();
    staged(
        &mut store,
        &a,
        "implement",
        "claude-code/claude-opus-5-5",
        1,
    );
    staged(&mut store, &b, "review", "codex/astra", 2);

    charge(&mut store, &agent(1), Some(&a), 0, (1.0, 0.0));
    charge(&mut store, &agent(1), Some(&a), DAY, (3.0, 0.0));
    charge(&mut store, &agent(2), Some(&b), DAY, (0.0, 4.0));
    charge(&mut store, &agent(3), Some(&c), 2 * DAY, (0.5, 0.25));
    charge(&mut store, &agent(4), None, 2 * DAY, (0.0, 1.0));
    (store, [a, b, c])
}

/// `(key, counted)` per row, after checking the rows add up to the total.
fn rows(report: &CostReport) -> Vec<(Option<String>, f64)> {
    let sum: f64 = report.rows.iter().map(|r| r.amounts.counted_usd).sum();
    assert_eq!(sum, report.total.counted_usd, "rows add up to the total");
    report
        .rows
        .iter()
        .map(|r| (r.key.clone(), r.amounts.counted_usd))
        .collect()
}

fn key(text: &str) -> Option<String> {
    Some(text.to_owned())
}

#[test]
fn each_grouping_splits_the_same_total() {
    let (store, [a, b, c]) = seeded();
    let all = range(0, 2 * DAY);
    let report = |by| store.cost_report(&all, by, true).unwrap();
    let days: Vec<_> = [0, DAY, 2 * DAY].map(|s| key(&day(s).to_string())).into();
    assert_eq!(
        rows(&report(CostGroup::Day)),
        vec![
            (days[0].clone(), 1.0),
            (days[1].clone(), 6.0),
            (days[2].clone(), 1.75)
        ]
    );
    assert_eq!(
        rows(&report(CostGroup::Repo)),
        vec![(key("alpha"), 7.0), (None, 1.75)]
    );
    assert_eq!(
        rows(&report(CostGroup::Pipeline)),
        vec![(key("ask"), 4.75), (key("direct"), 3.0), (None, 1.0)]
    );
    assert_eq!(
        rows(&report(CostGroup::Model)),
        vec![
            (key("codex/astra"), 4.0),
            (key("claude-code/claude-opus-5-5"), 3.0),
            (None, 1.75)
        ]
    );
    assert_eq!(
        rows(&report(CostGroup::Provider)),
        vec![(key("codex"), 4.0), (key("claude-code"), 3.0), (None, 1.75)],
        "the model rows summed by provider"
    );
    let ids = [a, b, c].map(|i| key(i.id.as_str()));
    assert_eq!(
        rows(&report(CostGroup::Item)),
        vec![
            (ids[1].clone(), 4.0),
            (ids[0].clone(), 3.0),
            (None, 1.0),
            (ids[2].clone(), 0.75)
        ]
    );
    assert_eq!(
        rows(&report(CostGroup::Stage)),
        vec![(key("review"), 4.0), (key("implement"), 3.0), (None, 1.75)]
    );
    for by in CostGroup::ALL {
        let report = report(by);
        assert_eq!(report.by, by);
        assert_eq!(report.total.counted_usd, 8.75, "{by:?}");
        assert_eq!(report.total.cost_usd, 3.5, "{by:?}");
        assert_eq!(report.total.estimated_cost_usd, 5.25, "{by:?}");
    }
}

#[test]
fn the_day_bounds_keep_only_spend_on_those_days() {
    let (store, [a, b, _]) = seeded();
    let day1 = store
        .cost_report(&range(DAY, DAY), CostGroup::Item, true)
        .unwrap();
    assert_eq!(
        rows(&day1),
        vec![(key(b.id.as_str()), 4.0), (key(a.id.as_str()), 2.0)],
        "agent 1's day-0 dollar stays on day 0"
    );
    let later = store
        .cost_report(&range(DAY, 2 * DAY), CostGroup::Repo, true)
        .unwrap();
    assert_eq!(rows(&later), vec![(key("alpha"), 6.0), (None, 1.75)]);
    assert_eq!(later.range, range(DAY, 2 * DAY));
}

#[test]
fn totals_count_what_the_daily_and_total_budgets_count() {
    let (store, _) = seeded();
    let now = t(2 * DAY);
    for estimates in [true, false] {
        let (today, total) = store.spend_totals(now, estimates).unwrap();
        let on_today = store
            .cost_report(&range(2 * DAY, 2 * DAY), CostGroup::Model, estimates)
            .unwrap();
        assert_eq!(on_today.total.counted_usd, today, "estimates {estimates}");
        let every_day = store
            .cost_report(&range(0, 2 * DAY), CostGroup::Stage, estimates)
            .unwrap();
        assert_eq!(every_day.total.counted_usd, total, "estimates {estimates}");
        assert_eq!(every_day.estimates_counted, estimates);
        rows(&every_day);
    }
    let billed = store
        .cost_report(&range(0, 2 * DAY), CostGroup::Day, false)
        .unwrap();
    let counted: Vec<f64> = billed.rows.iter().map(|r| r.amounts.counted_usd).collect();
    assert_eq!(counted, vec![1.0, 2.0, 0.5], "without estimates, only cost");
    assert_eq!(billed.total.estimated_cost_usd, 5.25, "still shown");
}

#[test]
fn a_range_without_spend_is_empty_and_a_backwards_one_refused() {
    let (seeded, _) = seeded();
    let later = range(10 * DAY, 20 * DAY);
    for by in CostGroup::ALL {
        let report = seeded.cost_report(&later, by, true).unwrap();
        assert!(report.rows.is_empty(), "{by:?}: {report:?}");
        assert_eq!(report.total.counted_usd, 0.0);
        assert_eq!(report.total.estimated_cost_usd, 0.0);
    }
    let report = store()
        .cost_report(&range(0, DAY), CostGroup::Day, true)
        .unwrap();
    assert!(report.rows.is_empty());
    let backwards = seeded
        .cost_report(&range(DAY, 0), CostGroup::Day, true)
        .unwrap_err();
    assert!(matches!(backwards, ArchonError::Invalid(_)), "{backwards}");
}

#[test]
fn provenance_classifies_reports_but_never_changes_charges() {
    use archon_core::cost::CostBasis;
    let mut store = store();
    let owner = store.insert_item(&item(None, "cost basis"), None).unwrap();
    for (n, basis) in [
        (1, CostBasis::ListPrice),
        (2, CostBasis::ProviderReported),
        (3, CostBasis::Unknown),
        (4, CostBasis::Mixed),
    ] {
        let row = SpendRow {
            agent: agent(n),
            item: Some(owner.id.clone()),
            cost_usd: 2.0,
            cost_basis: basis,
            cost_complete: None,
            estimated_cost_usd: 0.5,
            input_tokens: 1,
            output_tokens: 1,
            at: t(DAY),
        };
        store.record_spend(&row).unwrap();
        store.record_spend(&row).unwrap();
        // Older conflicting provenance cannot relabel a charged total.
        store
            .record_spend(&SpendRow {
                at: t(0),
                cost_basis: CostBasis::ProviderReported,
                ..row
            })
            .unwrap();
    }
    for estimates in [true, false] {
        let expected = if estimates { 10.0 } else { 8.0 };
        for by in CostGroup::ALL {
            let report = store.cost_report(&range(0, DAY), by, estimates).unwrap();
            assert_eq!(report.total.counted_usd, expected);
            assert_eq!(report.total.cost_usd, 2.0);
            assert_eq!(report.total.estimated_cost_usd, 4.0);
            assert_eq!(report.total.unclassified_cost_usd, 4.0);
            rows(&report);
        }
        assert_eq!(
            store.spend_for_item(&owner.id, estimates).unwrap(),
            expected
        );
        assert_eq!(
            store.spend_totals(t(DAY), estimates).unwrap(),
            (expected, expected)
        );
        assert_eq!(
            store.spend_for_agent(&agent(1), estimates).unwrap(),
            if estimates { 2.5 } else { 2.0 }
        );
    }
}

#[test]
fn cumulative_basis_changes_preserve_history_without_recharging_it() {
    use archon_core::cost::CostBasis;
    let mut store = store();
    let row = SpendRow {
        agent: agent(1),
        item: None,
        cost_usd: 2.0,
        cost_basis: CostBasis::ListPrice,
        cost_complete: None,
        estimated_cost_usd: 0.0,
        input_tokens: 1,
        output_tokens: 1,
        at: t(0),
    };
    store.record_spend(&row).unwrap();
    store
        .record_spend(&SpendRow {
            cost_usd: 3.0,
            cost_basis: CostBasis::Mixed,
            at: t(DAY),
            ..row
        })
        .unwrap();
    let first = store
        .cost_report(&range(0, 0), CostGroup::Day, false)
        .unwrap();
    assert_eq!(first.total.counted_usd, 2.0);
    assert_eq!(first.total.unclassified_cost_usd, 0.0);
    assert_eq!(first.total.cost_usd, 0.0);
    assert_eq!(first.total.estimated_cost_usd, 2.0);
    let second = store
        .cost_report(&range(DAY, DAY), CostGroup::Day, false)
        .unwrap();
    assert_eq!(second.total.counted_usd, 1.0);
    assert_eq!(second.total.unclassified_cost_usd, 1.0);
}

#[test]
fn a_lower_correction_cannot_relabel_retained_list_price_as_billing() {
    use archon_core::cost::CostBasis;
    let mut store = store();
    let row = SpendRow {
        agent: agent(1),
        item: None,
        cost_usd: 2.0,
        cost_basis: CostBasis::ListPrice,
        cost_complete: None,
        estimated_cost_usd: 0.0,
        input_tokens: 1,
        output_tokens: 1,
        at: t(0),
    };
    store.record_spend(&row).unwrap();
    store
        .record_spend(&SpendRow {
            cost_usd: 1.0,
            cost_basis: CostBasis::ProviderReported,
            at: t(DAY),
            ..row
        })
        .unwrap();
    let report = store
        .cost_report(&range(0, DAY), CostGroup::Day, false)
        .unwrap();
    assert_eq!(report.total.counted_usd, 2.0);
    assert_eq!(report.total.cost_usd, 0.0);
    assert_eq!(report.total.estimated_cost_usd, 2.0);
    assert_eq!(report.total.unclassified_cost_usd, 0.0);
    assert_eq!(store.spend_totals(t(DAY), false).unwrap(), (0.0, 2.0));
}

#[test]
fn basis_changes_within_one_day_preserve_each_charged_component() {
    use archon_core::cost::{CostAmounts, CostBasis};
    let mut store = store();
    let mut row = SpendRow {
        agent: agent(1),
        item: None,
        cost_usd: 0.0,
        cost_basis: CostBasis::Unknown,
        cost_complete: None,
        estimated_cost_usd: 0.5,
        input_tokens: 1,
        output_tokens: 1,
        at: t(0),
    };
    for basis in [
        CostBasis::Unknown,
        CostBasis::ListPrice,
        CostBasis::ProviderReported,
        CostBasis::Mixed,
    ] {
        row.cost_usd += 1.0;
        row.cost_basis = basis;
        // Equal timestamps can carry growth; repeated totals must not charge twice.
        store.record_spend(&row).unwrap();
        store.record_spend(&row).unwrap();
    }
    let expected = CostAmounts {
        counted_usd: 4.5,
        cost_usd: 1.0,
        estimated_cost_usd: 1.5,
        unclassified_cost_usd: 2.0,
    };
    assert_eq!(
        store
            .cost_report(&range(0, 0), CostGroup::Day, true)
            .unwrap()
            .total,
        expected
    );
    row.cost_basis = CostBasis::ProviderReported;
    row.cost_usd = 2.0;
    store.record_spend(&row).unwrap();
    row.cost_usd = 4.0;
    store.record_spend(&row).unwrap();
    assert_eq!(
        store
            .cost_report(&range(0, 0), CostGroup::Day, true)
            .unwrap()
            .total,
        expected
    );
    assert_eq!(store.spend_totals(t(0), true).unwrap(), (4.5, 4.5));
}

/// One usage total of agent `n`, as Aether reports it at `at`.
fn total(n: u32, cost: f64, basis: CostBasis, complete: bool, at: i64) -> SpendRow {
    SpendRow {
        agent: agent(n),
        item: None,
        cost_usd: cost,
        cost_basis: basis,
        cost_complete: Some(complete),
        estimated_cost_usd: 0.0,
        input_tokens: 1,
        output_tokens: 1,
        at: t(at),
    }
}

/// Aether prices a turn at list price while it streams, then reports what the
/// provider billed: that figure is the charge, and all of it is billed.
#[test]
fn a_settled_total_replaces_the_interim_estimate_it_corrects() {
    use CostBasis::{ListPrice, Mixed, ProviderReported};
    let mut store = store();
    let turns = [
        (0.125, ListPrice),
        (0.25, ProviderReported),
        (0.375, Mixed),
        (0.5, Mixed),
        (0.4375, ProviderReported),
    ];
    for (at, (cost, basis)) in turns.into_iter().enumerate() {
        let row = total(1, cost, basis, true, at as i64);
        store.record_spend(&row).unwrap();
        store.record_spend(&row).unwrap();
    }
    assert_eq!(store.spend_for_agent(&agent(1), false).unwrap(), 0.4375);
    assert_eq!(store.spend_totals(t(9), false).unwrap(), (0.4375, 0.4375));
    let report = store
        .cost_report(&range(0, 0), CostGroup::Day, false)
        .unwrap();
    assert_eq!(report.total.counted_usd, 0.4375);
    assert_eq!(report.total.cost_usd, 0.4375, "all of it was billed");
    assert_eq!(report.total.estimated_cost_usd, 0.0);
    assert_eq!(report.total.unclassified_cost_usd, 0.0);

    // The next turn's estimate is charged on top of the settled total.
    store
        .record_spend(&total(1, 0.5625, Mixed, true, 9))
        .unwrap();
    let report = store
        .cost_report(&range(0, 0), CostGroup::Day, false)
        .unwrap();
    assert_eq!(report.total.cost_usd, 0.4375);
    assert_eq!(report.total.unclassified_cost_usd, 0.125);
}

/// A counter that goes down lowers a charge only as the provider's settled
/// figure, and never below an earlier one: no budget is evaded by reporting less.
#[test]
fn only_a_settled_total_lowers_a_charge_and_never_below_an_earlier_one() {
    use CostBasis::{ListPrice, Mixed, ProviderReported};
    let mut store = store();
    store
        .record_spend(&total(1, 2.0, ProviderReported, true, 0))
        .unwrap();
    store.record_spend(&total(1, 3.0, Mixed, true, 1)).unwrap();
    let unsettled = [
        total(1, 1.0, Mixed, true, 2),
        total(1, 1.0, ListPrice, true, 3),
        total(1, 2.5, ProviderReported, false, 4),
        SpendRow {
            cost_complete: None,
            ..total(1, 2.5, ProviderReported, true, 5)
        },
    ];
    for row in unsettled {
        store.record_spend(&row).unwrap();
        assert_eq!(store.spend_for_agent(&agent(1), false).unwrap(), 3.0);
    }
    assert_eq!(store.spend_totals(t(5), false).unwrap(), (3.0, 3.0));

    // Below the $2 the provider already reported: that earlier figure stands.
    store
        .record_spend(&total(1, 0.5, ProviderReported, true, 6))
        .unwrap();
    assert_eq!(store.spend_for_agent(&agent(1), false).unwrap(), 2.0);
    assert_eq!(store.spend_totals(t(6), false).unwrap(), (2.0, 2.0));
    store.record_spend(&total(1, 2.25, Mixed, true, 7)).unwrap();
    assert_eq!(store.spend_totals(t(7), false).unwrap(), (2.25, 2.25));
}

/// What a correction takes back comes out of that agent's own days, newest
/// first: another agent's spend on the same day still counts in full.
#[test]
fn a_correction_takes_back_only_what_that_agent_was_charged() {
    use CostBasis::{Mixed, ProviderReported};
    let mut store = store();
    store.record_spend(&total(1, 1.0, Mixed, true, 0)).unwrap();
    store
        .record_spend(&total(1, 1.5, Mixed, true, DAY))
        .unwrap();
    store
        .record_spend(&total(2, 3.0, ProviderReported, true, DAY))
        .unwrap();
    store
        .record_spend(&total(1, 0.25, ProviderReported, true, DAY + 1))
        .unwrap();

    assert_eq!(store.spend_for_agent(&agent(1), false).unwrap(), 0.25);
    assert_eq!(store.spend_totals(t(DAY), false).unwrap(), (3.0, 3.25));
    let days = store
        .cost_report(&range(0, DAY), CostGroup::Day, false)
        .unwrap();
    let counted: Vec<f64> = days.rows.iter().map(|r| r.amounts.counted_usd).collect();
    assert_eq!(counted, vec![0.25, 3.0], "day 0, then day 1");
    assert_eq!(days.total.cost_usd, 3.25);
    assert_eq!(days.total.unclassified_cost_usd, 0.0);
}

fn usage(n: u32, item: &QueueItem, cost: f64, basis: CostBasis, estimate: f64) -> SpendRow {
    SpendRow {
        item: Some(item.id.clone()),
        estimated_cost_usd: estimate,
        ..total(n, cost, basis, estimate == 0.0, 0)
    }
}

/// Aether 1.0.9 books a Codex turn as an estimate beside a zero cost; older
/// records carried the same dollars as list-price cost. A Claude lead's billed
/// cost and its Codex reviewer's estimate make up a mixed tree.
#[test]
fn codex_estimates_count_like_the_list_price_they_replace() {
    let mut store = store();
    let old = store.insert_item(&item(None, "old codex"), None).unwrap();
    let new = store.insert_item(&item(None, "new codex"), None).unwrap();
    let mixed = store.insert_item(&item(None, "mixed tree"), None).unwrap();
    staged(&mut store, &mixed, "implement", "claude-code/opus", 3);
    let codex = "codex/sol".parse().unwrap();
    store
        .record_delegated(&agent(4), &agent(3), &codex, None)
        .unwrap();
    for row in [
        usage(1, &old, 0.75, CostBasis::ListPrice, 0.0),
        usage(2, &new, 0.0, CostBasis::ListPrice, 0.75),
        usage(3, &mixed, 2.0, CostBasis::ProviderReported, 0.0),
        usage(4, &mixed, 0.0, CostBasis::ListPrice, 0.75),
    ] {
        store.record_spend(&row).unwrap();
    }
    let item_spend =
        |item: &QueueItem, estimates| store.spend_for_item(&item.id, estimates).unwrap();
    assert_eq!(
        (item_spend(&old, true), item_spend(&new, true)),
        (0.75, 0.75)
    );
    assert_eq!(item_spend(&mixed, true), 2.75);
    // Budgets that leave estimates out count only the old record's dollars.
    assert_eq!(
        (item_spend(&old, false), item_spend(&new, false)),
        (0.75, 0.0)
    );
    assert_eq!(item_spend(&mixed, false), 2.0);
    let reviewer = |estimates| store.delegated_beneath(&agent(3), estimates).unwrap()[0].cost_usd;
    assert_eq!((reviewer(true), reviewer(false)), (0.75, 0.0));
    for (estimates, counted) in [(true, 4.25), (false, 2.75)] {
        let report = store
            .cost_report(&range(0, 0), CostGroup::Item, estimates)
            .unwrap();
        assert_eq!(report.total.counted_usd, counted);
        assert_eq!(report.total.cost_usd, 2.0, "only the lead was billed");
        assert_eq!(report.total.estimated_cost_usd, 2.25);
        assert_eq!(report.total.api_equivalent_usd(), 4.25);
    }
}
