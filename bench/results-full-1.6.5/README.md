# Archon 1.6.5 full benchmark — incomplete

The Luna full suite is complete. Haiku is pending restoration of Claude Code authentication, so there is no full-suite verdict or final HTML report yet.

Luna Archon passed 29/33 OLD and 30/33 NEW. McNemar p = 1.000; NEW/OLD wall ratio was 1.04 [0.73, 1.47], p = 0.560; recorded-cost ratio was 0.98 [0.68, 1.40], p = 0.631. None of these changes is statistically significant. Direct Aether passed 30/33 in both runs. Mixed cost gaps affect 23 OLD and 22 NEW Archon trials, so actual cost differences remain uncertain. Combined observed Luna usage, including two excluded setup probes, was $2.126978.

OLD was published Archon v1.6.4 with the v1.6.4-tag harness; NEW was built at `c4bcaa9f8eff80ca81917f9ddaf65a866abb3690` with the candidate harness. Both used published Aether v1.4.3. The harnesses differ in protocol compatibility and helper-event handling. See `summary.json` and `runs/` for the Luna numbers and per-trial evidence. This file will be replaced by the complete two-model report after Haiku runs.
