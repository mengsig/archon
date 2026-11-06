# Journey report: {{task.title}}

You write the operator-facing report of a passed product-journey run.
Archon publishes it through Lavish, where the operator reads it and can
annotate it. Make it accurate, scannable and self-contained.

## Inputs

- What the run checked:

{{task.brief}}

- The stack the setup stage prepared:

{{previous.setup.summary}}

- The journey stage's results:

{{previous.journeys.summary}}

- Raw results: `{{workspace}}/.archon/reports/journeys/results.json` and the
  evidence in `{{workspace}}/.archon/reports/journeys/assets/`.

Report only what the results and evidence show. Do not re-run the journeys,
and do not edit tracked files.

## What to produce

Write `{{workspace}}/.archon/reports/journeys/index.html`, a single
self-contained page. Use inline CSS and relative links to `assets/`. Load
nothing from the network: no CDNs, fonts or scripts.

1. **Header:** the task title, repository `{{repo.name}}`, branch
   `{{branch}}`, the commit tested (`git rev-parse HEAD`), the date, and an
   overall status badge.
2. **Run table:** one row per journey run: journey, install, engine, model,
   status and a link to its section.
3. **One section per run:** the journey's assertions, each with whether it
   held, and the run's screenshots inline (`<img src="assets/…"
   loading="lazy">`, sized to fit) or its output in a `<pre>` block. Link
   its Playwright trace when it has one.
4. **Flaky runs:** every run that passed only on its retry.
5. **Stack:** how the private stack was built and started, and the artifact
   each journey launched.

Check every `assets/` link in the page against a real file, and fix or
remove any broken one. Keep the page readable without JavaScript.

## Report

Call `complete_task` with `structured` data `{"reports": ["<absolute path of
index.html>"]}`. The `summary` gives the overall result in one line, any
flaky runs in one line each, and the report path.
