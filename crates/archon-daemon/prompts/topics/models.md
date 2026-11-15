# Choosing models

Choose a `model_plan` entry by `tier` (fast, balanced, strong, frontier) and
effort, not by model name. Archon resolves the tier from Aether's catalog at
dispatch, within model_policy, on the provider you name or else on the item's
routed provider (`provider_weights`, shown by `get_item`) or yours; a
provider without that tier falls back to its next tier down, and the attempt
records the model it ran. Pin `model` only when the operator names one.
Routing scales `provider_weights` by each provider's live quota headroom: a
nearly spent provider gets little, an item moves its unstarted stages when its
provider runs out (`get_item` says why), and it waits for a reset only when
every provider is out (`queue_status` shows headroom, reserve and next reset).
Defaults stop at `strong`; ask for `frontier` only when the operator does.

- A stronger tier at lower effort often beats a weaker one at max effort.
  Effort buys deliberation, not capability: raise it on a weaker tier only for
  long, well-specified, test-verifiable work.
- Several fast-tier reviewers, or a fast-tier team checked by one stronger-tier
  judge at low effort, can beat one balanced reviewer for far less.
- Reviewer independence means independent discovery: another provider at the
  same tier counts as independent.
- A delegating lead's workers run on its pipeline's worker default
  (`get_item` shows it), never the lead's own model by default. Name
  `<stage>.workers` choices only when the parts need another tier or effort;
  a cheaper tier than the lead is the usual choice.
- `model_evidence` groups attempts by stage, tier and effort as well as by
  model; prefer what it shows over these rules of thumb.
