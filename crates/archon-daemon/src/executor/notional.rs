//! A stage Aether stopped at its dollar cap, on an engine whose billing
//! Archon could not tell (Claude Code under a login that may be a
//! subscription), says the dollars it was stopped on may be notional.

use archon_core::error::ArchonError;
use archon_core::id::StageRunId;
use archon_core::ports::CostBilling;
use archon_core::run::StageOutcome;
use archon_queue::Store;

use super::spawn;

/// What Aether's error says of an agent it stopped at its cost budget.
const COST_BUDGET_STOP: &str = "cost budget of $";

/// `outcome`, its failure labelled notional cost when Aether stopped stage
/// `id` at a dollar cap whose billing is unknown.
pub(super) fn label(
    store: &Store,
    id: &StageRunId,
    outcome: StageOutcome,
) -> Result<StageOutcome, ArchonError> {
    let StageOutcome::Failed { error, infra } = outcome else {
        return Ok(outcome);
    };
    if !error.contains(COST_BUDGET_STOP) {
        return Ok(StageOutcome::Failed { error, infra });
    }
    let billing = match spawn::load_spawn(store, id) {
        Ok(stored) => stored.request.cost_billing,
        Err(ArchonError::NotFound(_)) => None,
        Err(other) => return Err(other),
    };
    let error = match billing {
        Some(CostBilling::Unknown) => labelled(&error),
        _ => error,
    };
    Ok(StageOutcome::Failed { error, infra })
}

fn labelled(error: &str) -> String {
    assert!(
        error.contains(COST_BUDGET_STOP),
        "only a cost stop is labelled"
    );
    let text = format!(
        "{error} (notional cost: Claude Code reports list-price dollars whether or not its \
         login bills them, and Aether showed no fresh subscription usage window with headroom for it, so the \
         budget was enforced; under a subscription, raise or unset the item's budget)"
    );
    assert!(text.starts_with(error), "Aether's words come first");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_cost_stop_is_labelled_and_aethers_words_stay_first() {
        let stop = "cost budget of $1.50: $1.52 spent";
        let text = labelled(stop);
        assert!(
            text.starts_with(stop) && text.contains("notional cost"),
            "{text}"
        );
    }

    #[test]
    #[should_panic(expected = "only a cost stop")]
    fn labelling_another_failure_is_a_bug() {
        labelled("the engine crashed");
    }
}
