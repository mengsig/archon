# Choosing models

Choose a `model_plan` entry by `tier` (fast, balanced, strong, frontier) and
effort, not by model name. Archon resolves the tier from Aether's catalog at
dispatch, within model_policy, on the provider you name or else on the item's
routed provider (`provider_weights`, shown by `get_item`) or yours; a
provider without that tier falls back to its next tier down, and the attempt
records the model it ran. Pin `model` only when the operator names one.
Defaults stop at `strong`; ask for `frontier` only when the operator does.

- A stronger tier at lower effort often beats a weaker one at max effort.
  Effort buys deliberation, not capability: raise it on a weaker tier only for
  long, well-specified, test-verifiable work.
- Several fast-tier reviewers, or a fast-tier team checked by one stronger-tier
  judge at low effort, can beat one balanced reviewer for far less.
- Reviewer independence means independent discovery: another provider at the
  same tier counts as independent.
- `model_evidence` groups attempts by stage, tier and effort as well as by
  model; prefer what it shows over these rules of thumb.
