# Settings and item autonomy

Read `settings` (optional `repo`) before replacing an overlay. It returns `scoped`
fields and effective values with sources. `{"cmd":"set_settings","repo":"web",
"settings":{"autonomy_default":{"merge":"auto"},"autonomy_max":{"merge":"auto"}}}`
replaces that scope; preserve other scoped fields. Omit `repo` for global.
`reset_settings` clears its scope; it can loosen and need confirmation. Supported:
defaults/ceiling, budgets_enabled, budget_usd_per_item, max_per_repo, default_pipeline,
checks (`"waived"`: checks that never start are a quiet feed line, never a wake). Other
budget/concurrency caps are global only. Full-auto is not yet supported.

`update_item` and `steer_item` accept autonomy within the ceiling. For an override,
request authority with `{"cmd":"update_item","item":"qi_…","patch":{"autonomy":
{"merge":"auto"}}}` and no other patch fields. Above-ceiling changes ask first;
the repository ceiling stays unchanged. A shipped item's merge change checks its
reviewed PR head. Running items can change only merge/tools; tool changes affect
future spawns. Undo uses `{"cmd":"undo_authority","change":N}` from the feed.
