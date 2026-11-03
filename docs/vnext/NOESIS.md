# Noesis: study, licensing decision, parity plan and learner model (issue #377, phase 0)

Noesis is the learning service of Archon 1.5.0 / Aether 1.3.0. It is modelled
on the behaviour of LearnHarness 0.4.1 (commit
`e69e85269c064c6c9c49a8b06ae78c06e79c4a63`, AGPL-3.0-only), which was read but
never run, vendored or modified. The architecture decision is in
[ARCHITECTURE.md §g](ARCHITECTURE.md), the interfaces are in
[CONTRACTS.md §6](CONTRACTS.md), and the tasks (F1–F8) are in
[WORKSTREAMS.md](WORKSTREAMS.md).

## 1. What LearnHarness is

This study was made by static reading only. Behavioural claims come from the
source and the docs, not from running the app.

- **Shape:** a single-user, local-first Next.js 16 / React 19 app with an
  optional Electron shell. The Electron main process starts the standalone
  Next server as a private loopback child. One Node process owns all state:
  agent runs, stores, background workers and SSE fan-out. "One server per
  data profile" is a stated hard assumption.
- **Persistence:** plain JSON and JSONL files, with no database, transactions
  or cross-process lock: chats, jobs, append-only graph ledgers (`nodes`,
  `chains`, `sessions`, `coverage`) and a settings delta file. Secrets are
  kept in a plaintext `auth.json`.
- **Agents:** Prober, Teacher and Practitioner. Each turn builds a new agent
  on the Pi agent engine (`@earendil-works/pi-agent-core`, MIT) from a
  composed markdown system prompt and a small tool registry. The pedagogy
  (adaptive probing, prerequisite planning, the hint ladder, retrieval
  practice) is mostly **prompt doctrine**. The code supplies the structure:
  tool schemas, validation when a tool is called, rules that stop a turn,
  phase derivation, and a deterministic merge.
- **Session state machine:** a pure fold over the transcript: `probe` → `plan`
  (on a successful `probe_done`) → `teach` (on the first question tagged with
  a plan node). The server stores no phase field. Posing a question ends the
  run. The browser grades multiple-choice answers locally, and free-recall
  answers go back to the agent. Each answer carries a confidence stop
  (0/30/50/70/90/100), "don't know" and the learner's thinking.
- **Planning:** the Teacher writes a restricted Mermaid `graph TD`, which a
  hand-written parser validates: slug ids, cycles, self-loops, duplicates,
  missing endpoints. Plan-node status is derived from the questions tagged
  with that node, never declared.
- **Practice:** a conversational session with `problem` (a model solution
  committed and hidden) and `conclude` (outcome, hint rung R0–R5, miss
  class, retest). There is one active problem at a time, and practice never
  writes to the graph.
- **Knowledge graph:**
  - The graph holds propositions (true, self-contained statements, with
    aliases and message-level provenance) and chains (premises that jointly
    entail a conclusion). Edges are derived, never stored.
  - Extraction has three stages: an extractor LLM submits findings; an
    auditor LLM (shown lexical candidates only) links each finding to an
    existing node or leaves it new; then a pure-code merge, chain
    deduplication and `simplifyChains`.
  - Coverage is a SHA-256 of each teacher message's view, so the uncovered
    prose is derived rather than tracked.
  - Extraction triggers automatically once ≥ 4,096 uncovered teacher tokens
    exist, with 3 background permits and **no spend cap**.
- **Learner model:** none. The graph explicitly refuses mastery state.
  Per-answer evidence exists only inside each chat record, and nothing
  aggregates it.
- **Models:** BYOK slots (`strong`, `cheap`, `vision`), API keys, ChatGPT,
  Copilot and xAI subscriptions, an opt-in Claude Code CLI bridge (over a
  loopback MCP bridge), and custom OpenAI-compatible endpoints. There is no
  Codex CLI path. Background use of a subscription needs separate consent.
- **Licence:** AGPL-3.0-only, with no CLA and no NOTICE. Copyright is spread
  across the contributors (about 880 commits by the author, plus several
  outside contributors). The prompts and the methodology doc are copyrighted
  text under the same licence.

## 2. Decision: clean-room reimplementation in Aether

**Reimplement the behaviour as `crates/aether-noesis` (MIT), written from
this document and CONTRACTS.md. Copy no source, prompt text, schema text or
test code. Do not integrate a sidecar, and do not fork.**

### 2.1 Licensing (issue §14; an engineering analysis, not legal advice)

| Option | What AGPL-3.0-only implies | Verdict |
| --- | --- | --- |
| Link or port the code | Different language, so this would really be a port: a derivative work. The combined Aether binary would have to be distributed under AGPL, and MIT cannot absorb that. No CLA exists, and the copyright is spread across contributors, so nobody can grant other terms. | Rejected |
| Node sidecar, shipped with Aether | A separate program talking over HTTP is arguably an aggregate. Shipping it still means distributing AGPL code with source obligations. Making it work requires changes (Aether engines and auth, budgets, the off switch), and §13 then obliges us to offer the modified source to every user who interacts with it over a network, which is exactly what our Tailscale access gives. It also brings a second runtime, a second store, single-process JSON state, duplicate auth and no Codex path. | Rejected |
| Fork | The fork stays AGPL-3.0-only forever and cannot be relicensed. It inherits the Pi engine, Next and Electron weight, and the single-writer assumptions, and it diverges from upstream once modified. | Rejected |
| **Clean-room reimplementation** | Ideas, behaviour, interfaces and published pedagogy are not protected by copyright; their expression is. We write our own code, schemas and prompts in Rust/TS under MIT. | **Chosen** |

Clean-room rules for tasks F1–F8:
1. This document and CONTRACTS.md §6 are the specification. They describe
   behaviour in our own words.
2. Implementers of F1–F8 do not open LearnHarness source, prompts, tests or
   docs. A reviewer who has read them may review, and may point to a
   behaviour by its id in §6, but may not supply text.
3. Prompts are written from the general pedagogy literature (retrieval
   practice, worked examples and fading, the interleaving and spacing
   effects, eliciting misconceptions, confidence calibration). No prompt
   segment of LearnHarness is translated, paraphrased or reordered.
4. Every Noesis PR states in its description: "No LearnHarness code or
   text was used." F1 adds a provenance note to the crate README.
5. Before release (H5), the operator decides whether counsel should review
   §2.1. That is recorded as a risk on #377, not as a blocker for phase 1.

### 2.2 Why in Aether, and what changes from upstream

Aether already provides what Noesis needs: subscription engines, budgets,
transcripts, questions and persistence (ARCHITECTURE.md §g, §i). The
deliberate differences from upstream are:

| Upstream | Noesis |
| --- | --- |
| Pi agent engine, rebuilt each turn | Aether agents in run `noesis`, with roles `noesis.prober` / `.teacher` / `.practitioner` / `.extractor` / `.auditor` |
| JSON/JSONL files, single writer | SQLite `noesis.db`, transactional and with migrations; no Aether memory link |
| Phase folded from the transcript | Phase stored on the session, changed only by service transitions and recorded as events; still derivable (rebuild test) |
| Mermaid plan text | A JSON plan DAG `{nodes: [{id, title}], edges: [{from, to}]}` validated by the same rules (§6 T8) |
| Random ids, wall-clock timestamps | Proposition and chain ids are content-derived hashes, so the same findings give the same bytes |
| Automatic extraction at 4,096 tokens, no spend cap | Manual by default. Auto is behind `noesis.extraction.sessions = "auto"`, with background budgets, caps and lowest priority |
| BYOK slots and API keys first | Subscription engines first; API only with `noesis.billing.allow_api` |
| No learner model | A learner model separate from the graph (§5) |
| Browser-only question dock | Questions are protocol objects (`noesis_answer`), so the UI, CLI or TUI can answer them |

## 3. Teaching loop (F2)

- **Prober:** makes the goal concrete, then maps what the learner already
  holds, one question at a time per strand. It brackets a floor and a ceiling
  using the answer, the stated confidence and the learner's reasoning. It
  ends with exactly one `noesis.probe_done` call carrying the per-strand
  assessment (floor, ceiling, misconceptions, starting point). It may read
  the learner model (§5) to skip concepts already demonstrated.
- **Teacher:** builds a minimal prerequisite plan from the learner's current
  state to the goal (`noesis.plan`, which may be rewritten wholesale), and
  presents it. It waits for confirmation, then teaches one step at a time.
  Each step ends with a check, and a wrong answer is diagnosed before the
  Teacher builds on it.
- **Practitioner:** works through retrieval problems in three modes (recall,
  discrimination, composition) with graduated hints. It records outcomes with
  `noesis.problem` / `noesis.conclude`, and targets concepts the learner
  model marks as fragile or due.
- **Harness tools** (registered only while Noesis is enabled, and only for
  `noesis.*` roles):
  - `noesis.ask {kind: choice|recall|goal, prompt, options?, answer_key, explanation, plan_node?, concepts?}`;
    `answer_key` is committed before the learner sees the question;
  - `noesis.probe_done`;
  - `noesis.plan`;
  - `noesis.problem` / `noesis.conclude`;
  - `noesis.graph_search`;
  - `noesis.learner_read`.
  A successful question pose ends the agent's turn. The answer arrives as
  the tool result through `noesis_answer`.
- **Prompts:** our own, kept in `crates/aether-noesis/src/prompts/` as
  numbered segments. An operator override replaces a segment only when both
  the setting and the file exist.

## 4. Knowledge graph and extraction (F8)

- `propositions(id, name, statement, created_at)`,
  `proposition_aliases`, `proposition_provenance(source, message_ref)`;
  `chains(id, conclusion, created_at)` with `chain_premises`;
  `coverage(source, message_ref, fingerprint, tokens, at)`;
  `sources(id, name, artifact, imported_at)`.
- The graph holds true statements only. It contains nothing about the
  learner, no quiz outcomes and no mastery. A misconception enters only as
  its correction.
- Pipeline: select the uncovered teacher prose, then batch it by token
  budget without splitting a message. The extractor submits findings
  validated against the batch. The auditor sees propositions only, with
  lexical candidates, and returns links. Then comes a deterministic merge,
  chain deduplication by premise set and conclusion, and the simplification
  rules (drop supersets, drop chains whose premises are covered by
  ancestry). Coverage is written last, in the same transaction as the merge.
- Jobs run on the background budget, `noesis.concurrency.background`, and the
  scheduler's lowest priority. Cancelling a job keeps the last committed
  merge.

## 5. Learner model (F4; issue §7.5)

**The learner model is a separate store of evidence about one person. The
proposition graph is a store of claims about the world. Neither writes the
other.** They share only proposition ids, stored as plain values. Graph reads
never touch learner tables, and deleting the graph leaves the learner model
intact (with dangling concepts marked).

Tables (`noesis.db`, migration N2):

- `concepts(id, kind: proposition|plan_node|topic, ref, title, created_at)`: a
  concept usually names a proposition. A plan node or a free topic is used
  when nothing has been extracted yet, and is relinked when extraction
  produces a matching proposition.
- `evidence(id, concept, session, at, kind, outcome, confidence?, dont_know, mode?, hint_rung?, miss_class?, note?)`,
  **append-only**. `kind` is one of: `probe_answer`, `predict_answer`,
  `check_answer`, `recall_judged`, `practice_conclude`, `exposure`,
  `misconception_observed`, `operator_correction`.
- `concept_state(concept, state, strength, last_success_at?, review_due_at?, calibration?, misconceptions, as_of_evidence)`
  is a **derived** cache. It is rebuilt from `evidence` on demand and checked
  by a rebuild test.

Derivation rules (a pure function of the evidence and the current time):

| State | Rule |
| --- | --- |
| `unseen` | no evidence |
| `exposed` | only `exposure` evidence (the concept was taught or shown). **Exposure never counts as mastery.** |
| `demonstrated` | at least 2 correct retrievals (`check`, `recall_judged` or practice `solved` at rung ≤ R1) on different days, at a stated confidence ≥ 50, with no unresolved misconception and `strength ≥ 0.5` |
| `fragile` | demonstrated before but decayed (`strength < 0.5`); or the latest attempt is wrong; or correct at confidence ≤ 30; or solved only with hints ≥ R3 |
| `misconception` | a `misconception_observed` record (from `probe_done`, or a confident wrong answer on a discrimination item) not yet cleared by 2 later correct discrimination answers |

- **Decay:** `strength = exp(-Δt / half_life)`. The half-life starts at 2 days,
  doubles after each successful spaced retrieval, halves after each failure,
  and stays between 1 and 180 days. `review_due_at` is when strength reaches
  0.5.
- **Calibration:** the Brier score of stated confidence against correctness,
  per learner and per concept. It is shown, and never used to grade.
- "Don't know" is recorded as evidence of absence. It is never a wrong
  answer at high confidence, and never mastery.
- **Operator correction** (`noesis_correct_learner`) appends evidence of kind
  `operator_correction` and wins until newer evidence arrives. It never
  edits history.
- **Uses:** the Prober skips `demonstrated` concepts, the Practitioner
  targets `fragile`, `misconception` and due concepts, and the UI shows
  "my understanding" with the `--mastery-*` tokens and text labels.

## 6. Behavioural parity plan (F2, F7, F8)

The parity suite lives in `crates/aether-noesis/tests/` and runs with a
scripted faux engine. It never calls a paid model. Ids follow the upstream
behaviour list. Where Noesis deliberately differs, the row says how.

**Teaching (T, task F2)**

| Id | Behaviour | Noesis form |
| --- | --- | --- |
| T1 | Agent selection: practice sessions get the Practitioner; the Teacher only after a successful, non-refused `probe_done` | same |
| T2 | `probe_done` is refused while a question is open and on a second call; a fixed short acknowledgement; the hand-off starts the Teacher, and recovery works if the wake fails | same |
| T3 | Phase moves probe → plan on `probe_done`, and plan → teach on the first question tagged with a plan node; a plan alone does not advance it | the stored phase equals the fold over events |
| T4 | A question pose ends the run, including mixed tool batches. An invalid pose returns an error, and the model may retry. The pending question is derived from the record. | same |
| T5 | Question authoring: ≥ 2 options; the answer key matches the option values; multi-select is graded as an exact set; `plan_node` exists; recall needs a committed answer key | same |
| T6 | Deterministic option shuffle and local grading; confidence stops 0/30/50/70/90/100, default 50; "don't know" excludes confidence | same |
| T7 | Reveal rule: teach-phase checks are collapsed; predictions, probes and goal questions are never collapsed | same |
| T8 | Plan validation: slug ids, cycles, self-loops, duplicates and missing endpoints rejected, with every problem reported; a rewrite replaces the whole plan | JSON DAG instead of Mermaid |
| T9 | Plan-node status (current, visited, upcoming) is derived from the latest tagged question | same |
| T10 | Practice gate: one active problem; `conclude` with none is refused; conclude then problem in one turn works; target names normalize to ids; an unknown target is refused | same |
| T11 | `conclude` enums (outcome, rung, miss class, retest); the solution is hidden until `conclude`; practice never writes to the graph | same, plus learner evidence (§5) |
| T12 | Malformed tool arguments (stringified arrays, quoted enums) are healed | same |

**Service (S, tasks F3 and F7)**

| Id | Behaviour | Noesis form |
| --- | --- | --- |
| S1 | One turn per session: a concurrent turn is refused, input is persisted before inference, missing models are refused explicitly, and an aborted turn keeps its partial output | `conflict` error; `no_model` with alternatives |
| S2 | Job results are delivered at turn boundaries exactly once, and recovered after a restart | the Aether steering and inbox paths |
| S3 | Prompt segment order, and an override needs both the switch and the file | same |
| S4 | Settings sanitizing | **differs:** invalid values are refused with an error rather than clamped (CONTRACTS.md §6.1) |
| S5 onward | The off-by-default guarantees of ARCHITECTURE.md §g | one test each (F7) |

**Graph (X, task F8)**

| Id | Behaviour | Noesis form |
| --- | --- | --- |
| X1 | Coverage counts teacher prose only, fingerprinted per message; the uncovered set is derived; batches never split a message; learner answers never reopen covered prose | same |
| X2 | The extraction trigger threshold (default 4,096 tokens, floor 500) and its gate | **differs:** manual by default |
| X3 | Extractor submissions are validated: anchors are in the batch, and chain endpoints are local or existing ids | same |
| X4 | The auditor returns links only; an unknown link creates a new node; 5 lexical candidates per finding, with paging | same |
| X5 | Merge: link appends, create adds; aliases and provenance are unioned; chains are deduplicated by key; a re-asserted chain keeps its id; premise = conclusion is dropped; coverage is written last | same, plus content-derived ids |
| X6 | Chain simplification rules and their tie-break order | same |
| X7 | Snapshot: junctions only for multi-premise chains; clusters by birth session (≥ 3); dangling chains dropped | same |
| X8 | Notes import: whole file, stable source identity, partial outcomes, 3 in flight, 25 MiB limit | via `noesis_import_source` (artifact) |
| X9 | Deletion: a node cascades to its chains; session deletion preserves or deletes the graph; a source deletion keeps nodes supported elsewhere | same |
| X10 | Store versioning: a newer version is refused | SQLite migrations; newer schema refused |

**Optional differential harness (F7, never in CI).** For maintainers who have
their own LearnHarness checkout, `evals/noesis/differential/` runs the
upstream app **unmodified** and treats it as a black box:

1. The upstream app is started locally from the user's checkout.
2. It is configured with a keyless custom OpenAI-compatible endpoint, served
   by our scripted faux model.
3. The harness drives the same scripted conversation through its HTTP API.

The harness compares **structure only**: phase sequence, question
validity, plan validation results, and graph statements and premise sets,
never ids or bytes. Nothing from upstream is imported, vendored, modified or
distributed. The harness needs no credentials and posts nothing anywhere.

**Quality evals (not parity).** Probing efficiency, plan quality and
question quality are not deterministic upstream either. F7 adds a small
rubric eval under `evals/noesis/quality/`, run manually on a subscription
model and reported in the PR, never gated in CI.

## 7. Risks

- **Licensing (medium):** clean-room discipline relies on process (§2.1). The
  mitigation is the rules above plus an optional review by counsel before
  release.
- **Teaching quality (medium):** our prompts start from zero. Quality evals
  and dogfooding precede any default-on suggestion. Noesis stays off by
  default.
- **Spend (low):** subscription-first, explicit budgets, manual extraction
  by default, and background priority last.
- **Data (low):** `noesis.db` is a separate file, independent of Aether
  memory. Deleting it removes all learning state.
