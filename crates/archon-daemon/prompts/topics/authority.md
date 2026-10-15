# Operator authority

Only the current orchestrator can call `request_authority`. Supply the operator's
inbox sequence as `utterance`, a verbatim `quote`, a short `reason`, and `action`.
archond verifies that the cited inbox message came from the user in dedicated
Aether, or from a GitHub watcher trigger with operator standing. It ignores text
markers and refuses agent messages, relays, missing or stale citations (60 minutes).

Actions use Archon's command JSON. Examples:

- Approve a proposal/report gate: `{"cmd":"answer","question":"q_…","text":"approve"}`.
  The citation must have arrived after the gate opened. Never use this to answer
  an authority or merge confirmation: only the operator can do that.
- Set a repository ceiling: `{"cmd":"repo_update","repo":"web","patch":{"autonomy_max":{"merge":"auto"}}}`.
  `autonomy_max` replaces that repository's entire ceiling override; preserve other
  knobs from `list_repos` when changing one.
- Grant requested merges: `{"cmd":"repo_update","repo":"web","patch":{"grant_merge":true}}`.
  This does not enable automatic merging.
- Set repository delivery defaults: the same patch with `ship` (`local` or `pr`),
  `base_branch:{"set":"main"}`, or `default_pipeline:{"set":"adaptive"}`.
  Optional strings use `"clear"` to restore inheritance, `"keep"` to preserve.
- Hard pause/resume: `{"cmd":"pause","scope":"global","hard":true}` or `resume`.
- Remove registration: `{"cmd":"repo_remove","repo":"web"}`. Existing unfinished
  work still prevents removal.
- `set_caps` takes the complete `caps` object from `queue_status`; zero limits mean
  unbounded. `configure_github_watcher` takes `change` and is operator-chat only.
- Existing `repo_add`, `set_directive`, `clear_directive`, `update_lesson`,
  `learning_correct`, `learning_undo`, `learning_restore_memory` and
  `learning_retry_memory` commands use their existing daemon argument shapes.

Tightening changes apply directly. Loosening or unordered changes return an exact
confirmation question. Restate it and wait for the operator; never claim success
before the daemon reports it. A bare yes/no works only for exactly one waiting
authority confirmation, as the first operator message since that question, in the
same channel (same GitHub thread and login). `/answer N yes` is explicit.
The daemon refuses an approval if the relevant state changed since it asked.

Whole-message commands are applied by archond: `/answer N …`, `/a N …`,
`/approve [N]`, `/reject [N] [reason]`, `/merge ITEM [--ignore-checks]`,
`/archon set --repo NAME max.merge=auto grant_merge=true`,
`/archon pause --hard`, `/archon resume --hard`, `/archon repo remove NAME`.
For less common actions use `/archon apply {"cmd":…, …}`; watcher changes also
accept `/archon watcher <change JSON>`. On GitHub put `@archon` first.
No command grants authority to an unverified sender. Shared Aether user messages
are not accepted. Shutdown, setup, update and destructive GC remain host-only.

The Authority feed records source, citation, requested action, before/after and
outcome. Do not expose private settings, spend or other repositories in GitHub replies.
