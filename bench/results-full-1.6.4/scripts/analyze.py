"""Compare old and new Archon on the full suite and write summary.json, README.md and index.html.

Usage: analyze.py <runs-dir> <out-dir> <html-out>
runs-dir holds old-haiku, new-haiku, old-luna, new-luna (each with results/<stamp>/).
"""
import html
import json
import math
import random
import statistics
import sys
from collections import defaultdict
from pathlib import Path

MODELS = {'haiku': 'Haiku (claude-code/claude-haiku-5-5)', 'luna': 'Luna (codex/gpt-6-luna)'}
Z = 1.959964


RUST_TASKS = {'multi-repo-currency', 'refactor-order-status'}


def read_stamp(run_dir):
    stamps = sorted((run_dir / 'results').iterdir())
    assert len(stamps) == 1, f'{run_dir}: expected one results stamp, got {stamps}'
    stamp = stamps[0]
    rows = [json.loads(line) for line in (stamp / 'results.jsonl').read_text().splitlines() if line.strip()]
    for r in rows:
        r['infra'] = (not r['success']) and r['error'] is not None
        r['items'] = load_items(stamp, r)
    return rows, json.loads((stamp / 'run.json').read_text()), stamp


def load_run(run_dir):
    """Main run without the two Rust tasks (their grader found no cargo), plus the rerun that has it."""
    rows, meta, stamp = read_stamp(run_dir)
    void = [r for r in rows if r['task'] in RUST_TASKS]
    rerun_rows, rerun_meta, _ = read_stamp(run_dir.parent / (run_dir.name + '-rust'))
    assert {r['task'] for r in rerun_rows} == RUST_TASKS, run_dir
    kept = [r for r in rows if r['task'] not in RUST_TASKS] + rerun_rows
    meta['planned'] += rerun_meta['planned'] - len(void)
    meta['completed'] = meta.get('completed', 0) - len(void) + rerun_meta.get('completed', 0)
    meta['skipped'] = meta.get('skipped', 0) + rerun_meta.get('skipped', 0)
    meta['void_rust_trials'] = len(void)
    meta['rerun_spent_usd'] = rerun_meta.get('spent_usd')
    return {'rows': kept, 'meta': meta, 'stamp': stamp}


def load_items(stamp, row):
    path = stamp / 'trials' / row['task'] / row['arm'] / str(row['trial']) / 'logs' / 'archon-items.json'
    if row['arm'] != 'archon' or not path.is_file():
        return []
    return json.loads(path.read_text())


def effective_cost(r):
    if r['cost_usd'] is None and r['estimated_cost_usd'] is None:
        return None
    return (r['cost_usd'] or 0.0) + (r['estimated_cost_usd'] or 0.0)


def wilson(k, n):
    if n == 0:
        return (float('nan'), float('nan'))
    p = k / n
    d = 1 + Z * Z / n
    c = (p + Z * Z / (2 * n)) / d
    h = Z * math.sqrt(p * (1 - p) / n + Z * Z / (4 * n * n)) / d
    return (max(0.0, c - h), min(1.0, c + h))


def newcombe_diff(k1, n1, k0, n0):
    """Difference p1 - p0 with the Newcombe hybrid-score interval."""
    if n1 == 0 or n0 == 0:
        return (float('nan'),) * 3
    l1, u1 = wilson(k1, n1)
    l0, u0 = wilson(k0, n0)
    p1, p0 = k1 / n1, k0 / n0
    d = p1 - p0
    return (d, d - math.sqrt((p1 - l1) ** 2 + (u0 - p0) ** 2), d + math.sqrt((u1 - p1) ** 2 + (p0 - l0) ** 2))


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def mann_whitney_p(xs, ys):
    """Two-sided normal-approximation Mann-Whitney U with tie correction."""
    n1, n2 = len(xs), len(ys)
    if n1 < 2 or n2 < 2:
        return float('nan')
    pooled = sorted([(v, 0) for v in xs] + [(v, 1) for v in ys])
    ranks, i = [0.0] * len(pooled), 0
    tie_term = 0.0
    while i < len(pooled):
        j = i
        while j + 1 < len(pooled) and pooled[j + 1][0] == pooled[i][0]:
            j += 1
        for k in range(i, j + 1):
            ranks[k] = (i + j) / 2 + 1
        t = j - i + 1
        tie_term += t ** 3 - t
        i = j + 1
    r1 = sum(r for r, (_, g) in zip(ranks, pooled) if g == 0)
    u1 = r1 - n1 * (n1 + 1) / 2
    n = n1 + n2
    var = n1 * n2 / 12 * ((n + 1) - tie_term / (n * (n - 1)))
    if var <= 0:
        return 1.0
    z = (u1 - n1 * n2 / 2) / math.sqrt(var)
    return math.erfc(abs(z) / math.sqrt(2))


def ratio_ci(new, old, draws=4000, seed=7):
    """Ratio of means new/old with a percentile bootstrap interval."""
    if not new or not old:
        return (float('nan'),) * 3
    rng = random.Random(seed)
    est = statistics.fmean(new) / statistics.fmean(old)
    samples = []
    for _ in range(draws):
        a = statistics.fmean(rng.choices(new, k=len(new)))
        b = statistics.fmean(rng.choices(old, k=len(old)))
        if b > 0:
            samples.append(a / b)
    samples.sort()
    return (est, samples[int(0.025 * len(samples))], samples[int(0.975 * len(samples)) - 1])


def graded(rows, arm):
    return [r for r in rows if r['arm'] == arm and not r['infra']]


def stage_stats(row):
    """Stage records and delegated reviewer agents over the trial's items."""
    stages = sum(len(i['stages']) for i in row['items'])
    reviewers = sum(len(d['agents']) for i in row['items'] for d in i.get('delegations', {}).values())
    return stages, reviewers


def arm_summary(rows, arm):
    g = graded(rows, arm)
    k = sum(r['success'] for r in g)
    lo, hi = wilson(k, len(g))
    walls = [r['wall_secs'] for r in g]
    costs = [c for c in (effective_cost(r) for r in g) if c is not None]
    per_trial = [stage_stats(r) for r in g]
    items = [len(r['items']) for r in g]
    return {
        'planned': len([r for r in rows if r['arm'] == arm]), 'graded': len(g), 'pass': k,
        'rate': k / len(g) if g else float('nan'), 'ci': [lo, hi],
        'infra': len([r for r in rows if r['arm'] == arm and r['infra']]),
        'wall': walls, 'wall_mean': statistics.fmean(walls) if walls else float('nan'),
        'wall_median': statistics.median(walls) if walls else float('nan'),
        'cost': costs, 'cost_mean': statistics.fmean(costs) if costs else float('nan'),
        'cost_total': sum(costs), 'cost_trials': len(costs),
        'stages': [s for s, _ in per_trial], 'reviewers': [v for _, v in per_trial], 'items': items,
        'stages_per_item': sum(s for s, _ in per_trial) / max(1, sum(items)),
        'reviewers_per_item': sum(v for _, v in per_trial) / max(1, sum(items)),
        'stages_per_trial': statistics.fmean([s for s, _ in per_trial]) if per_trial else float('nan'),
        'reviewers_per_trial': statistics.fmean([v for _, v in per_trial]) if per_trial else float('nan'),
        'timeouts': len([r for r in g if r['timed_out']]),
    }


def paired(old_rows, new_rows, arm):
    key = lambda r: (r['task'], r['trial'])
    old = {key(r): r for r in graded(old_rows, arm)}
    new = {key(r): r for r in graded(new_rows, arm)}
    shared = sorted(set(old) & set(new))
    b = sum(old[k]['success'] and not new[k]['success'] for k in shared)
    c = sum(new[k]['success'] and not old[k]['success'] for k in shared)
    both = sum(old[k]['success'] and new[k]['success'] for k in shared)
    neither = len(shared) - b - c - both
    return {'pairs': len(shared), 'old_only': b, 'new_only': c, 'both_pass': both,
            'both_fail': neither, 'p': mcnemar_exact(b, c)}


def compare(old_rows, new_rows, arm):
    o, n = arm_summary(old_rows, arm), arm_summary(new_rows, arm)
    d, lo, hi = newcombe_diff(n['pass'], n['graded'], o['pass'], o['graded'])
    wr = ratio_ci(n['wall'], o['wall'])
    cr = ratio_ci(n['cost'], o['cost'])
    return {'old': o, 'new': n, 'diff': [d, lo, hi], 'mcnemar': paired(old_rows, new_rows, arm),
            'wall_ratio': list(wr), 'wall_p': mann_whitney_p(n['wall'], o['wall']),
            'cost_ratio': list(cr), 'cost_p': mann_whitney_p(n['cost'], o['cost'])}


def task_table(old_rows, new_rows, arm):
    tasks = sorted({r['task'] for r in old_rows + new_rows})
    out = []
    for t in tasks:
        o = arm_summary([r for r in old_rows if r['task'] == t], arm)
        n = arm_summary([r for r in new_rows if r['task'] == t], arm)
        ratio = n['wall_mean'] / o['wall_mean'] if o['wall'] and n['wall'] and o['wall_mean'] > 0 else float('nan')
        out.append({'task': t, 'old': o, 'new': n, 'wall_ratio': ratio,
                    'wall_p': mann_whitney_p(n['wall'], o['wall'])})
    return out


def trial_history(row):
    """Item histories and the first error of one Archon trial."""
    hist = []
    for it in row['items']:
        stages = [f"{s['stage_id']}#{s['attempt']}:{s['state'].get('state')}/{s.get('verdict')}" for s in it['stages']]
        hist.append({'pipeline': it['item']['pipeline'], 'state': str(it['item']['state']),
                     'stages': stages, 'reviewers': sum(len(d['agents']) for d in it.get('delegations', {}).values())})
    first_error = row['error'] or row['note']
    if not first_error:
        for it in row['items']:
            for s in it['stages']:
                if s['state'].get('state') not in ('completed', 'succeeded'):
                    first_error = f"stage {s['stage_id']} {s['state']}"
                    break
    return {'task': row['task'], 'trial': row['trial'], 'success': row['success'], 'wall': row['wall_secs'],
            'items': hist, 'first_error': first_error or 'none recorded'}


def findings(old_rows, new_rows, cmp_arm_tasks):
    """Tasks that flipped pass to fail or got slower, with item history."""
    flips, slow = [], []
    for t in cmp_arm_tasks:
        o, n = t['old'], t['new']
        if o['graded'] and n['graded'] and n['rate'] < o['rate']:
            rows = [r for r in new_rows if r['arm'] == 'archon' and r['task'] == t['task'] and not r['infra'] and not r['success']]
            flips.append({'task': t['task'], 'old': f"{o['pass']}/{o['graded']}", 'new': f"{n['pass']}/{n['graded']}",
                          'history': [trial_history(r) for r in rows]})
        slower = t['wall_ratio'] >= 1.5 and n['wall_mean'] - o['wall_mean'] >= 30
        if slower:
            rows = [r for r in new_rows if r['arm'] == 'archon' and r['task'] == t['task'] and not r['infra']]
            slow.append({'task': t['task'], 'ratio': t['wall_ratio'], 'p': t['wall_p'],
                         'old_mean': o['wall_mean'], 'new_mean': n['wall_mean'],
                         'history': [trial_history(r) for r in rows]})
    return flips, slow


def verdict(per_model):
    reasons = []
    for m, d in per_model.items():
        c = d['archon']
        if c['diff'][2] < 0:
            reasons.append(f'{m}: Archon pass rate fell significantly (diff interval entirely below 0)')
        if c['mcnemar']['p'] < 0.05 and c['mcnemar']['old_only'] > c['mcnemar']['new_only']:
            reasons.append(f"{m}: McNemar p={c['mcnemar']['p']:.3f} with more pass-to-fail than fail-to-pass pairs")
        wr = c['wall_ratio']
        if wr[1] > 1.0 and c['wall_p'] < 0.05:
            reasons.append(f"{m}: Archon wall time rose significantly (ratio {wr[0]:.2f}, interval {wr[1]:.2f}-{wr[2]:.2f}, p={c['wall_p']:.3f})")
    return ('REGRESSION' if reasons else 'NO REGRESSION'), reasons


def pair_entry(base, cand):
    entry = {'archon': compare(base['rows'], cand['rows'], 'archon'),
             'aether': compare(base['rows'], cand['rows'], 'aether'),
             'tasks_archon': task_table(base['rows'], cand['rows'], 'archon'),
             'tasks_aether': task_table(base['rows'], cand['rows'], 'aether')}
    entry['flips'], entry['slow'] = findings(base['rows'], cand['rows'], entry['tasks_archon'])
    return entry


def next_verdict(per_model):
    """Does NEXT recover 1.6.4's time and cost without losing passes?"""
    out = {}
    for m, d in per_model.items():
        c = d['next_vs_new']['archon']
        out[m] = {'pass_loss': c['diff'][2] < 0 or (c['mcnemar']['p'] < 0.05 and c['mcnemar']['old_only'] > c['mcnemar']['new_only']),
                  'faster': c['wall_ratio'][0] < 1.0, 'cheaper': c['cost_ratio'][0] < 1.0,
                  'faster_significant': c['wall_ratio'][2] < 1.0 and c['wall_p'] < 0.05,
                  'cheaper_significant': c['cost_ratio'][2] < 1.0 and c['cost_p'] < 0.05}
    return out


def main():
    runs_dir, out_dir, html_out = (Path(a) for a in sys.argv[1:4])
    per_model = {}
    for m in MODELS:
        old, new, nxt = (load_run(runs_dir / f'{a}-{m}') for a in ('old', 'new', 'next'))
        per_model[m] = {'new_vs_old': pair_entry(old, new), 'next_vs_new': pair_entry(new, nxt),
                        'arms': {n: arm_summary(r['rows'], 'archon') for n, r in (('old', old), ('new', new), ('next', nxt))},
                        'aether_arms': {n: arm_summary(r['rows'], 'aether') for n, r in (('old', old), ('new', new), ('next', nxt))},
                        'meta': {n: r['meta'] for n, r in (('old', old), ('new', new), ('next', nxt))}}
    word, reasons = verdict({m: {'archon': d['new_vs_old']['archon']} for m, d in per_model.items()})
    summary = {'verdict': word, 'reasons': reasons, 'next': next_verdict(per_model), 'models': per_model}
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / 'summary.json').write_text(json.dumps(summary, indent=1, default=str))
    from render import render_readme, render_html
    (out_dir / 'README.md').write_text(render_readme(summary, MODELS))
    html_out.parent.mkdir(parents=True, exist_ok=True)
    html_out.write_text(render_html(summary, MODELS))
    print(word, reasons)


if __name__ == '__main__':
    main()
