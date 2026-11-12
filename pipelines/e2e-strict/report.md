# E2E report: {{task.title}}

You write the operator-facing report of a completed end-to-end run. Archon
publishes it through Lavish, where the operator reads it and can annotate it.
Make it accurate, scannable and self-contained.

## Inputs

- The brief that was tested:

{{task.brief}}

- The environment the setup stage prepared:

{{previous.setup.summary}}

- The e2e stage's findings:

{{previous.e2e.summary}}

- Raw results: `{{workspace}}/.archon/reports/e2e/results.json` and the
  evidence in `{{workspace}}/.archon/reports/e2e/assets/`.

Report only what the results and evidence show. Do not re-run the tests, and
do not edit tracked files.

## What to produce

Write `{{workspace}}/.archon/reports/e2e/index.html`, a single self-contained
page. Use inline CSS and relative links to `assets/`. Load nothing from the
network: no CDNs, fonts or scripts.

1. **Header:** the task title, repository `{{repo.name}}`, branch `{{branch}}`,
   the date, and an overall status badge (all passed, or N failed).
2. **Summary:** two or three sentences on what was tested and what the
   operator needs to know.
3. **Scenario table:** id, title, status and step count. Each row links to
   its section.
4. **One section per scenario:** its goal, and its steps as a list showing
   the action, the expected result, the actual result and a status marker. Put
   the step's screenshot inline (`<img src="assets/…" loading="lazy">`, sized
   to fit) or its transcript in a `<pre>` block.
5. **Defects:** every failed scenario or flaky step, most severe first, with
   reproduction steps.
6. **Environment:** how to start and stop it, and the version or commit
   tested (`git rev-parse HEAD`).

Check every `assets/` link in the page against a real file, and fix or remove
any broken one. Keep the page readable without JavaScript.

## Report

Call `complete_task` with `structured` data `{"reports": ["<absolute path of
index.html>"]}`. The `summary` gives the overall result in one line, the
defects in one line each, and the report path.
