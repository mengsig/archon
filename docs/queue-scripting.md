# Scripted enqueue and bounded waits

`archon queue add` sends the same enqueue request as MCP `enqueue`. Archond
resolves the repository and pipeline defaults and checks stage ids, model
choices, model policy, autonomy ceilings, dependencies, budgets and idempotency.
No orchestrator or MCP stdio harness is needed to submit work.

The smallest enqueue names a title and a brief; Archon chooses the model
of every stage from your policy:

```sh
item=$(archon queue add --title 'Explain the cache design' \
  --pipeline ask --brief-file brief.md)
```

To choose the models yourself, for a repository-free question, save a brief
in `brief.md` and a model plan in `plan.json`:

```json
{"answer": [{"model": "codex/gpt-6.1-sol", "effort": "high"}]}
```

```sh
item=$(archon queue add --title 'Explain the cache design' \
  --pipeline ask --brief-file brief.md --model-plan-file plan.json \
  --idempotency-key cache-design-1)
archon queue wait "$item" --timeout 30m
```

`--brief-file -` reads UTF-8 text from stdin. The model plan is a JSON or TOML
file with the same stage-to-choice-array shape as MCP `model_plan`. The file
extension must be `.json` or `.toml`. It may name only some stages: a stage
it leaves out gets the orchestrator's model where your policy allows it,
else the first allowed model, at the effort the pipeline's tier suggests. The equivalent TOML plan is:

```toml
[[answer]]
model = "codex/gpt-6.1-sol"
effort = "high"
```

Use `archon pipelines list` to find stage ids. Optional
`--model-reasons-file` takes a JSON or TOML map from stage ids to short,
single-line explanations, e.g. `{"answer":"Inspect the cache implementation"}`.
The daemon enforces the current policy; these examples do not grant permission
to use a model outside that policy.

`--repo` takes a registered name or id. `--pipeline` can be omitted when
using a repository, letting archond apply its repository/autonomy defaults.
Other inputs are `--priority low|normal|high|critical` (default `normal`),
repeatable `--depends-on <id-or-prefix>`, repeatable `--tag`,
`--idempotency-key`, and `--budget-usd`. A repeated enqueue with the same
idempotency key and arguments returns the original item; conflicting
arguments are refused.

The autonomy flags are `--review strict|standard|none`,
`--proposal lavish|off`, `--report before-pr|with-pr|off`,
`--merge manual|auto`, and `--tools guarded|yolo`. Omitted knobs use daemon
defaults; values above the effective ceiling are refused. Text output is
just the item id; `--json` outputs the complete item. Daemon failures retain
the CLI JSON error envelope and code, including `invalid_argument` for an
invalid stage plan and `policy` for a disallowed model.

`archon queue wait <item>...` accepts one or more full ids or unambiguous
prefixes. It waits until **every** item is succeeded, failed or cancelled.
A blocked item is still pending. Waiting requires an already running archond
(and never starts Aether); `queue add` uses the usual CLI autostart path.
The default timeout is **30m**; override it
with a positive whole number and `s`, `m`, `h`, `d` or `w`, e.g. `90s` or `2h`.
The deadline covers connection, subscription and daemon requests as well as
waiting for state changes. It subscribes before reading state and reconciles
state periodically. Connection, subscription and request failures remain
errors rather than being silently retried.

Exit statuses:

- **0:** every item succeeded.
- **3:** every item is terminal and at least one failed or was cancelled.
- **4:** the timeout expired, even if some items already failed.
- **1:** a command or daemon error; **2:** a clap argument parsing error.

Text output lists the last observed item ids and states and adds a timeout
message when needed. JSON output is a single object
`{"timed_out":false,"items":[...]}` containing complete last observed items,
in argument order. At timeout, the array may be partial or empty if connection
or an initial request has not completed; later refreshes preserve earlier
observations. Command errors use the usual `{"error":{"code":...,"message":...}}`
envelope instead. Waiting never cancels an item.
