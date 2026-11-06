# Archon documentation

Every page under `docs/`, by what you are doing. The project overview is the
[README](../README.md), and each release's changes are in the
[changelog](../CHANGELOG.md).

## Using Archon

| Page | What it covers |
| --- | --- |
| [Get started](getting-started.md) | Setting Archon up and running your first work from the chat. |
| [User guide](guide.md) | The detailed operating instructions. |
| [Configuration](configuration.md) | Every `config.toml` key. |
| [Queue scripting](queue-scripting.md) | Enqueueing from files and bounded waits. |
| [GitHub watcher](github-watcher.md) | Items from issues and comments, and who may command it. |
| [Operator authority](operator-authority.md) | What only the operator may do, and how archond verifies it. |
| [Troubleshooting](troubleshooting.md) | archond's log and what to do when work is stuck. |

## Installing and upgrading

| Page | What it covers |
| --- | --- |
| [Install](install.md) | Installing, verifying a download, updating and uninstalling. |
| [Upgrading](upgrading.md) | What a release that needs more than `archon update` changes, and the steps (1.4.x to 1.5.0). |

## Remote access

| Page | What it covers |
| --- | --- |
| [Use Archon from your phone](remote-setup.md) | `archon remote setup`, step by step. |
| [Remote access](remote.md) | The threat model, configuration, pairing, endpoints and what a device may do. |
| [Security audit (vNext)](security/vnext-audit.md) | The 1.5.0 audit of the mount, device roles, operator standing and the GitHub surface. |

## Pipelines

| Page | What it covers |
| --- | --- |
| [Writing pipelines](PIPELINES.md) | Stages, gates, fan-out and prompts. |
| [Product journeys](journeys.md) | The `release-journey` gate and its `.archon/journeys.toml` manifest, with a Playwright `_electron` example. |
| `team-pr` example | [pipeline.toml](examples/pipelines/team-pr/pipeline.toml), with the prompts [plan](examples/pipelines/team-pr/plan.md), [build](examples/pipelines/team-pr/build.md), [e2e](examples/pipelines/team-pr/e2e.md) and [review](examples/pipelines/team-pr/review.md). |
| `bugfix-tdd` example | [pipeline.toml](examples/pipelines/bugfix-tdd/pipeline.toml), with the prompts [reproduce](examples/pipelines/bugfix-tdd/reproduce.md), [fix](examples/pipelines/bugfix-tdd/fix.md) and [review](examples/pipelines/bugfix-tdd/review.md). |

## Developing Archon

| Page | What it covers |
| --- | --- |
| [Architecture](ARCHITECTURE.md) | How Archon sits on Aether: the daemon, the store and the protocols. |
| [Development and benchmarks](development.md) | Building, testing, releasing and benchmarking. |

## vNext design (1.5.0)

The design records of the unified Aether app (issue #377).

| Page | What it covers |
| --- | --- |
| [Baseline](vnext/BASELINE.md) | Where both projects stood before the work. |
| [Architecture decisions](vnext/ARCHITECTURE.md) | The decisions the work rests on. |
| [Contracts](vnext/CONTRACTS.md) | Protocols, migrations, version rules and configuration keys. |
| [Workstreams](vnext/WORKSTREAMS.md) | The tasks, their ownership and waves. |
| [Noesis](vnext/NOESIS.md) | The Noesis study, licensing decision and learner model. |
