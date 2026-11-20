"""Render README.md and the Lavish HTML page from analyze.py's summary."""
import html
import math

ARMS = ('old', 'new', 'next')
ARM_LABEL = {'old': 'OLD v1.6.3', 'new': 'NEW v1.6.4', 'next': 'NEXT c17af3fe'}


def pct(x):
    return 'n/a' if x != x else f'{100 * x:.1f}%'


def interval(ci):
    return f'{100 * ci[0]:.1f}–{100 * ci[1]:.1f}%'


def signed_pp(d):
    return f'{100 * d[0]:+.1f} pp [{100 * d[1]:+.1f}, {100 * d[2]:+.1f}]'


def fx(x):
    return 'n/a' if x != x else f'{x:.2f}×'


def secs(x):
    return 'n/a' if x != x else f'{x:.0f} s'


def usd(x):
    return 'n/a' if x != x else f'${x:.4f}'


def ratio(r):
    return 'n/a' if r[0] != r[0] else f'{r[0]:.2f}× [{r[1]:.2f}, {r[2]:.2f}]'


def pval(p):
    return 'n/a' if p != p else f'{p:.3f}'


def arm_row(label, a):
    return (f"| {label} | {a['pass']}/{a['graded']} of {a['planned']} | {pct(a['rate'])} | {interval(a['ci'])} | "
            f"{secs(a['wall_mean'])} / {secs(a['wall_median'])} | {usd(a['cost_mean'])} ({a['cost_trials']} priced) | "
            f"{a['stages_per_item']:.2f} | {a['reviewers_per_item']:.2f} | {a['infra']} |")


ARM_HEAD = ('| Arm | Pass / graded (planned) | Pass % | 95% interval | Wall mean / median | Cost per trial | '
            'Stage turns per item | Reviewers per item | Infra errors |\n|---|---|---:|---|---|---|---:|---:|---:|\n')


def cmp_row(name, c):
    m = c['mcnemar']
    return (f"| {name} | {signed_pp(c['diff'])} | {m['old_only']} / {m['new_only']} of {m['pairs']} | {pval(m['p'])} | "
            f"{ratio(c['wall_ratio'])} (p={pval(c['wall_p'])}) | {ratio(c['cost_ratio'])} (p={pval(c['cost_p'])}) |")


CMP_HEAD = ('| Comparison (Archon arm) | Pass difference, 95% interval | McNemar baseline-only / candidate-only pass | '
            'McNemar exact p | Wall ratio [interval] (Mann-Whitney p) | Cost ratio [interval] (Mann-Whitney p) |\n'
            '|---|---|---|---:|---|---|\n')


def task_cells(d, name):
    """One row per task: pass/graded and mean wall for each of the three arms."""
    nvo = {t['task']: t for t in d['new_vs_old']['tasks_archon']}
    xvn = {t['task']: t for t in d['next_vs_new']['tasks_archon']}
    rows = []
    for task in sorted(nvo):
        cells = [nvo[task]['old'], nvo[task]['new'], xvn[task]['new']]
        rows.append((task, cells))
    return rows


def task_table_md(d):
    out = ['| Task | OLD pass | NEW pass | NEXT pass | OLD wall | NEW wall | NEXT wall | NEW/OLD wall | NEXT/NEW wall | NEW stages / reviewers per item |',
           '|---|---|---|---|---:|---:|---:|---:|---:|---|']
    nvo = {t['task']: t for t in d['new_vs_old']['tasks_archon']}
    xvn = {t['task']: t for t in d['next_vs_new']['tasks_archon']}
    for task, (o, n, x) in task_cells(d, ''):
        out.append(f"| {task} | {o['pass']}/{o['graded']} | {n['pass']}/{n['graded']} | {x['pass']}/{x['graded']} | "
                   f"{secs(o['wall_mean'])} | {secs(n['wall_mean'])} | {secs(x['wall_mean'])} | "
                   f"{fx(nvo[task]['wall_ratio'])} | {fx(xvn[task]['wall_ratio'])} | "
                   f"{n['stages_per_item']:.1f} / {n['reviewers_per_item']:.1f} |")
    return '\n'.join(out) + '\n'


def history_md(h):
    lines = []
    for t in h['history']:
        items = '; '.join(f"{i['pipeline']} [{', '.join(i['stages'])}] reviewers={i['reviewers']}" for i in t['items']) or 'no item'
        lines.append(f"  - trial {t['trial']} ({'pass' if t['success'] else 'fail'}, {t['wall']:.0f} s): {items}. First error: {t['first_error']}")
    return '\n'.join(lines)


def findings_md(d, model_name):
    out = []
    for label, entry in (('NEW vs OLD', d['new_vs_old']), ('NEXT vs NEW', d['next_vs_new'])):
        for f in entry['flips']:
            out.append(f"- **{model_name}, {label}: {f['task']} lost passes** ({f['old']} to {f['new']}). Failed trials:\n{history_md(f)}")
        for f in entry['slow']:
            out.append(f"- **{model_name}, {label}: {f['task']} slower** ({f['old_mean']:.0f} s to {f['new_mean']:.0f} s, "
                       f"{f['ratio']:.2f}×, p={pval(f['p'])}). Trials of the later arm:\n{history_md(f)}")
    return '\n'.join(out) or f'- {model_name}: no task lost passes or got 1.5× and 30 s slower in either comparison.'


def infra_md(d, model_name):
    out = []
    for n in ARMS:
        for arm, key in (('archon', 'arms'), ('aether', 'aether_arms')):
            a = d[key][n]
            if a['infra']:
                out.append(f"- {model_name} {ARM_LABEL[n]} {arm}: {a['infra']} infrastructure error(s), left out of every rate.")
    return '\n'.join(out) or f'- {model_name}: none.'


def missing_md(d, model_name):
    out = []
    for n in ARMS:
        m = d['meta'][n]
        if m.get('completed') is not None and m['planned'] != m['completed']:
            out.append(f"- {model_name} {ARM_LABEL[n]}: {m['completed']} of {m['planned']} planned trials completed ({m.get('skipped', 0)} skipped by the cost guard).")
    return '\n'.join(out) or f'- {model_name}: every planned trial ran.'


def next_text(summary, m):
    v = summary['next'][m]
    parts = ['loses passes' if v['pass_loss'] else 'no significant pass loss',
             'faster' if v['faster'] else 'not faster', 'cheaper' if v['cheaper'] else 'not cheaper']
    sig = [k.replace('_significant', '') for k in ('faster_significant', 'cheaper_significant') if v[k]]
    return ', '.join(parts) + (f' (significant: {", ".join(sig)})' if sig else ' (neither difference is significant)')


def render_readme(summary, models):
    out = [f"# Full suite: Archon v1.6.4 vs v1.6.3 (and next), Aether 1.4.2\n",
           f"**Verdict: {summary['verdict']}** for Archon v1.6.4 against v1.6.3 on the full suite.\n"]
    out += [f"- {r}" for r in summary['reasons']] or ['- No significant drop in pass rate and no significant rise in wall time on either model.']
    out.append('')
    for m, name in models.items():
        out.append(f"- NEXT (c17af3fe, #459) vs NEW, {name}: {next_text(summary, m)}.")
    out.append('\nIntervals are 95%: Wilson for a rate, Newcombe hybrid for a difference, a 4000-draw percentile bootstrap for a ratio of means. '
               'McNemar is exact two-sided on (task, trial) pairs both graded. Wall p is Mann-Whitney on pooled trials. '
               'Three trials per cell cannot separate small effects.\n')
    out.append('### Notes\n\n' + '\n'.join(f'- {n}' for n in NOTES) + '\n')
    for m, name in models.items():
        d = summary['models'][m]
        out.append(f"## {name}\n\n### Arms\n\n" + ARM_HEAD.rstrip('\n'))
        out += [arm_row(f'Archon {ARM_LABEL[n]}', d['arms'][n]) for n in ARMS]
        out += [arm_row(f'Direct Aether control (run with {ARM_LABEL[n]})', d['aether_arms'][n]) for n in ARMS]
        out.append('\n### Comparisons\n\n' + CMP_HEAD.rstrip('\n'))
        out += [cmp_row('NEW vs OLD', d['new_vs_old']['archon']), cmp_row('NEXT vs NEW', d['next_vs_new']['archon']),
                cmp_row('NEW vs OLD (Aether control)', d['new_vs_old']['aether']),
                cmp_row('NEXT vs NEW (Aether control)', d['next_vs_new']['aether'])]
        out.append('\n### Per task (Archon)\n\n' + task_table_md(d))
        out.append('### Infrastructure errors\n\n' + infra_md(d, name))
        out.append('\n### Missing cells\n\n' + missing_md(d, name))
        out.append('\n### Flipped or slower tasks\n\n' + findings_md(d, name) + '\n')
    return '\n'.join(out) + METHOD



NOTES = [
    'Two Rust tasks, multi-repo-currency and refactor-order-status, were void in the first pass of all six runs: the sandbox PATH had no cargo, so the grader and the agents could not run it. They were rerun for every arm (3 trials, cargo on PATH); only the rerun cells are counted.',
    'On Luna, long-backlog fell from 3/3 on OLD to 1/3 on both NEW and NEXT: the failed trials report a work-item branch that conflicted and was left out of grading. The Aether control passed 3/3 on every run. Pooled over the suite the drop is not significant (McNemar p=1.0), but it is a per-task flip worth a look.',
    'NEW was not slower or dearer than OLD in this suite (wall ratio 0.97x on Haiku, 0.84x on Luna; no significant difference), so #459 had no 1.6.4 slowdown to recover. NEXT changes reviewers per item (Haiku 1.65 to 1.47) and cost per trial slightly (0.93x on Haiku); neither wall nor cost difference is significant, and Luna NEXT wall is 1.13x NEW (not significant).',
    'Three trials per task give wide intervals: a +/-12 to 18 point difference is within noise. Luna costs include estimates for unbilled usage and are floors. The $25 (OLD, NEW) and $38 (NEXT) guards never tripped; each run spent under $3.70 (voided Rust trials included).',
]

METHOD = '''
## Method and provenance

- Arms: OLD = published Archon v1.6.3, NEW = published v1.6.4, NEXT = Archon built with `cargo build --release --locked -p archon`
  from origin/next `c17af3feba8f5a6a2adf480d77d4477894d6eccc` (1.6.4 plus #459). All three on the published Aether v1.4.2
  (`x86_64-unknown-linux-musl`). Every downloaded archive passed its release's `SHA256SUMS`.
- Harness: OLD used `archon-bench` built from the `v1.6.3` tag (the head's harness writes a config 1.6.3 refuses). NEW and NEXT used
  next's harness (built at c17af3fe), which has #459's fix for helper `agent.created` events. NEW and NEXT therefore share a harness;
  OLD does not, so the NEW vs OLD time comparison also carries any harness difference (`bench/tasks` and graders are identical).
- Every task under `bench/tasks` (11), 3 trials per (task, arm), medium effort, `archon` and direct `aether` arms, one trial at a time
  (`--parallel 1`), in a private sandbox per run (own HOME, config, data, temp and sockets). Six runs ran side by side, with the Aether
  release bench and a ccso stack on the same machine, so wall times carry shared-machine noise (load5 stayed below 9).
- Guard: `--max-usd 25` for OLD and NEW, 38 for NEXT; no run came near it (spent $1.25 to $3.62 each, the voided Rust trials and the rerun included).
- Stage turns per item count the stage records of each Archon item, every attempt; reviewers per item count agents the stage leads
  delegated (`archon-items.json`). An item with no `delegations` record counts as none. Cost is billed plus estimated dollars.
- Verdict rule: REGRESSION if, on either model, the Archon pass-rate difference interval lies entirely below 0, McNemar p < 0.05 with more
  pass-to-fail pairs, or wall time rose with the bootstrap ratio interval above 1 and Mann-Whitney p < 0.05.

| Binary | SHA256 |
|---|---|
| Archon 1.6.3 (published) | `8fe4927e3bc2d0c66e590e9fab58de8aca468cdfdc1345b07d3903da62b94981` |
| Archon 1.6.4 (published) | `c82005af40e2aed8fe2dae7d10d96d29b38228d5af2268c5f671ef965cf65f5c` |
| Archon NEXT c17af3fe (built) | `ce0e00452467a350db85e224c69a4929eb4b96dcb1a5d8fc2395f91027f4e644` |
| Aether 1.4.2 (published) | `c0510fe9d0669cf8eda7cc3f3154db21eb62c297b24b30896244e65398ab9a88` |
| archon-bench from v1.6.3 | `590163a9c0cd1ae1515d08a8f31d513ce3c05f00616b177323d261a58cbace5a` |
| archon-bench from next c17af3fe | `66764f52631de682f589a4cc14ec4b09464c980ddea8f923cf724d1f8eca737b` |
'''


def bar_chart(title, unit, rows, fmt):
    """Horizontal bars on one axis. rows: (label, value)."""
    top = max((v for _, v in rows if v == v), default=1) or 1
    h = 26 * len(rows) + 30
    parts = [f'<svg viewBox="0 0 900 {h}" width="100%" role="img" aria-label="{html.escape(title)}">',
             f'<text x="0" y="14" font-weight="bold" font-size="14">{html.escape(title)} ({unit})</text>']
    for i, (label, v) in enumerate(rows):
        y = 26 + 26 * i
        w = 0 if v != v else 520 * v / top
        parts.append(f'<text x="0" y="{y + 15}" font-size="13">{html.escape(label)}</text>'
                     f'<rect x="280" y="{y}" width="{w:.0f}" height="20" fill="#3b6ea5"/>'
                     f'<text x="{288 + w:.0f}" y="{y + 15}" font-size="13">{html.escape(fmt(v))}</text>')
    parts.append('</svg>')
    return ''.join(parts)


def tile(name, lines, bad):
    colour = '#b3261e' if bad else '#1b6e3a'
    body = ''.join(f'<div><b>{html.escape(k)}</b> {html.escape(v)}</div>' for k, v in lines)
    return f'<div class="tile" style="border-top:6px solid {colour}"><h3>{html.escape(name)}</h3>{body}</div>'


def tiles_for(d, name, key, a, b):
    c = d[key]['archon']
    m = c['mcnemar']
    base, cand = d['arms'][a], d['arms'][b]
    return tile(f'{name}: {ARM_LABEL[b]} vs {ARM_LABEL[a]}', [
        ('Pass %', f"{pct(base['rate'])} → {pct(cand['rate'])}"),
        ('Difference', signed_pp(c['diff'])),
        ('McNemar', f"{m['old_only']} / {m['new_only']} of {m['pairs']} pairs, p={pval(m['p'])}"),
        ('Time ratio', f"{ratio(c['wall_ratio'])}, p={pval(c['wall_p'])}"),
        ('Cost ratio', ratio(c['cost_ratio']))],
        c['diff'][2] < 0 or (c['wall_ratio'][1] > 1 and c['wall_p'] < 0.05))


def md_table_to_html(md):
    lines = [l for l in md.strip().splitlines() if l.startswith('|')]
    rows = [[c.strip() for c in l.strip('|').split('|')] for l in lines if not set(l) <= set('|-: ')]
    head, body = rows[0], rows[1:]
    th = ''.join(f'<th>{html.escape(c)}</th>' for c in head)
    tr = ''.join('<tr>' + ''.join(f'<td>{html.escape(c)}</td>' for c in r) + '</tr>' for r in body)
    return f'<table><thead><tr>{th}</tr></thead><tbody>{tr}</tbody></table>'


def render_html(summary, models):
    regress = summary['verdict'] == 'REGRESSION'
    banner = f"RESULT: Archon v1.6.4 vs v1.6.3 full suite — {summary['verdict']}"
    colour = '#b3261e' if regress else '#1b6e3a'
    parts = [HTML_HEAD, f'<div class="banner" style="background:{colour}">{html.escape(banner)}</div>']
    if summary['reasons']:
        parts.append('<ul>' + ''.join(f'<li>{html.escape(r)}</li>' for r in summary['reasons']) + '</ul>')
    parts.append('<h3>Notes</h3><ul>' + ''.join(f'<li>{html.escape(n)}</li>' for n in NOTES) + '</ul>')
    for m, name in models.items():
        d = summary['models'][m]
        parts.append(f'<h2>{html.escape(name)}</h2><p>NEXT vs NEW: {html.escape(next_text(summary, m))}.</p>')
        parts.append('<div class="tiles">' + tiles_for(d, name, 'new_vs_old', 'old', 'new')
                     + tiles_for(d, name, 'next_vs_new', 'new', 'next') + '</div>')
        parts.append('<h3>Per arm</h3>' + md_table_to_html(
            ARM_HEAD + '\n'.join([arm_row(f'Archon {ARM_LABEL[n]}', d['arms'][n]) for n in ARMS]
                                 + [arm_row(f'Aether control ({ARM_LABEL[n]} run)', d['aether_arms'][n]) for n in ARMS])))
        parts.append('<h3>Comparisons</h3>' + md_table_to_html(
            CMP_HEAD + '\n'.join([cmp_row('NEW vs OLD', d['new_vs_old']['archon']), cmp_row('NEXT vs NEW', d['next_vs_new']['archon'])])))
        parts.append('<h3>Per task (Archon)</h3>' + md_table_to_html(task_table_md(d)))
        arms = [(ARM_LABEL[n], d['arms'][n]) for n in ARMS]
        parts.append('<div class="charts">'
                     + bar_chart(f'{name}: Archon pass rate', '%', [(l, 100 * a['rate']) for l, a in arms], lambda v: f'{v:.1f}%')
                     + bar_chart(f'{name}: Archon mean wall time', 'seconds', [(l, a['wall_mean']) for l, a in arms], lambda v: f'{v:.0f} s')
                     + bar_chart(f'{name}: Archon cost per trial', 'USD', [(l, a['cost_mean']) for l, a in arms], lambda v: f'${v:.4f}')
                     + bar_chart(f'{name}: reviewers per item', 'agents', [(l, a['reviewers_per_item']) for l, a in arms], lambda v: f'{v:.2f}')
                     + '</div>')
        parts.append('<h3>Flipped or slower tasks</h3><pre>' + html.escape(findings_md(d, name)) + '</pre>')
        parts.append('<h3>Infrastructure errors</h3><pre>' + html.escape(infra_md(d, name) + '\n' + missing_md(d, name)) + '</pre>')
    parts.append('<h2>Method and provenance</h2><pre>' + html.escape(METHOD) + '</pre></body></html>')
    return '\n'.join(parts)


HTML_HEAD = '''<!doctype html><html><head><meta charset="utf-8"><title>Archon v1.6.4 full suite</title><style>
body{font-family:system-ui,sans-serif;margin:24px;max-width:1500px}
.banner{color:#fff;font-size:34px;font-weight:700;padding:28px;border-radius:8px;margin-bottom:16px}
.tiles{display:flex;flex-wrap:wrap;gap:16px}.tile{flex:1 1 420px;background:#f4f4f6;padding:14px;border-radius:6px;font-size:15px}
.tile div{margin:4px 0}table{width:100%;border-collapse:collapse;font-size:13px;margin:8px 0 20px}
th,td{border:1px solid #ccc;padding:5px 8px;text-align:left}th{background:#e8e8ee}
.charts{display:grid;grid-template-columns:repeat(auto-fit,minmax(520px,1fr));gap:16px}pre{white-space:pre-wrap;background:#f4f4f6;padding:10px}
</style></head><body>'''
