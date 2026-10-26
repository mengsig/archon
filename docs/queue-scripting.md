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
extension must be `.json` or `.toml`. The file is optional, and it may name
only some stages: a stage it leaves out gets the choice learned for the
repository ([configuration](configuration.md#learning)), else the
orchestrator's model where your policy allows it, else the first allowed
model, at the effort the pipeline's tier suggests. The equivalent TOML plan is:

```toml
[[answer]]
model = "codex/gpt-6.1-sol"
effort = "high"
```

Each choice's `model` is written `provider/model`, such as
`codex/gpt-6.1-sol`; a bare model name is refused. Instead of `model` a choice
may give a `tier` (`fast`, `balanced`, `strong` or `frontier`), and `effort` is
optional. `archon aether models` lists the models you can name.

Use `archon pipelines list` to find stage ids. Optional
`--model-reasons-file` takes a JSON or TOML map from stage ids to short,
single-line explanations, e.g. `{"answer":"Inspect the cache implementation"}`.
The daemon enforces the current policy; these examples do not grant permission
to use a model outside that policy.

`--repo` takes a registered name or id. `--pipeline` can be omitted when
using a repository, letting archond apply its repository/autonomy defaults.
Other inputs are `--priority low|normal|high|critical` (default `normal`),
repeatable `--depends-on <id-or-prefix>`, `--onto <item|number|branch>` (an
existing pull request to deliver into, frozen at enqueue; in a `ship = local`
repository, a finished item's branch to commit onto), repeatable `--tag`,
`--idempotency-key`, and `--budget-usd`. A repeated enqueue with the same
idempotency key and arguments returns the original item, even when archond
would now fill the stages the arguments leave open with other models;
conflicting arguments are refused.

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
and never starts it or Aether, as no command that only reads does; `queue add`
starts archond when it is not running, and says so on stderr.
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
- **5:** an item waits on a dependency that failed, so it cannot finish by
  itself; `blocked_dependencies` names every such item, and `items` still
  holds the last observation of each item.
- **1:** a command or daemon error; **2:** a clap argument parsing error.

Text output lists the last observed item ids and states and adds a timeout
message when needed. JSON output is a single object
`{"timed_out":false,"items":[...]}` containing complete last observed items,
in argument order. At timeout, the array may be partial or empty if connection
or an initial request has not completed; later refreshes preserve earlier
observations. Command errors use the usual `{"error":{"code":...,"message":...}}`
envelope instead. Waiting never cancels an item.

## Errors

Under `--json` an error prints one `{"error":{"code":...,"message":...}}`
object on stdout and nothing on stderr, and exits 1. A command line that
does not parse (an unknown flag, a missing argument) prints the same
envelope with the code `invalid_argument` and exits 2, as it does without
`--json`. `queue wait` reports what it waited for with the statuses above.
