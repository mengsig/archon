"""Compare full-suite runs and write the numerical summary."""
import json
import math
import random
import statistics
import sys
from collections import Counter
from pathlib import Path

MODELS = {'haiku': 'Haiku (claude-code/claude-haiku-5-5)', 'luna': 'Luna (codex/gpt-6-luna)'}
Z = 1.959964


def load_run(run_dir):
    if (run_dir / 'results.jsonl').is_file():
        stamp = run_dir
    else:
        stamps = sorted((run_dir / 'results').iterdir())
        assert len(stamps) == 1, f'{run_dir}: expected one results stamp, got {stamps}'
        stamp = stamps[0]
    rows = [json.loads(line) for line in (stamp / 'results.jsonl').read_text().splitlines() if line.strip()]
    assert rows, f'{run_dir}: no trial data'
    for row in rows:
        row['infra'] = not row['success'] and row['error'] is not None
        row['items'] = load_items(stamp, row)
        row['logs'] = stamp / 'trials' / row['task'] / row['arm'] / str(row['trial']) / 'logs'
    start = json.loads((run_dir / 'start.json').read_text())
    assert 'hashes' in start and 'archon' in start['hashes'], 'binary hashes are missing'
    return {'rows': rows, 'meta': json.loads((stamp / 'run.json').read_text()), 'start': start, 'stamp': stamp}


def load_items(stamp, row):
    assert row['arm'] in ('archon', 'aether') and row['trial'] > 0
    assert row['task'] and Path(row['task']).name == row['task'], 'invalid task path'
    path = stamp / 'trials' / row['task'] / row['arm'] / str(row['trial']) / 'logs' / 'archon-items.json'
    if row['arm'] != 'archon' or not path.is_file():
        return []
    items = json.loads(path.read_text())
    assert isinstance(items, list), 'item capture must be a list'
    return items


def effective_cost(r):
    assert r['cost_usd'] is None or math.isfinite(r['cost_usd']) and r['cost_usd'] >= 0
    assert r['estimated_cost_usd'] is None or math.isfinite(r['estimated_cost_usd']) and r['estimated_cost_usd'] >= 0
    if r['cost_usd'] is None and r['estimated_cost_usd'] is None:
        return None
    return (r['cost_usd'] or 0.0) + (r['estimated_cost_usd'] or 0.0)


def wilson(k, n):
    assert isinstance(k, int) and isinstance(n, int)
    assert 0 <= k <= n, 'invalid pass count'
    if n == 0:
        return (float('nan'), float('nan'))
    p = k / n
    d = 1 + Z * Z / n
    c = (p + Z * Z / (2 * n)) / d
    h = Z * math.sqrt(p * (1 - p) / n + Z * Z / (4 * n * n)) / d
    return (max(0.0, c - h), min(1.0, c + h))


def newcombe_diff(k1, n1, k0, n0):
    """Difference p1 - p0 with the Newcombe hybrid-score interval."""
    assert 0 <= k1 <= n1
    assert 0 <= k0 <= n0
    if n1 == 0 or n0 == 0:
        return (float('nan'),) * 3
    l1, u1 = wilson(k1, n1)
    l0, u0 = wilson(k0, n0)
    p1, p0 = k1 / n1, k0 / n0
    d = p1 - p0
    return (d, d - math.sqrt((p1 - l1) ** 2 + (u0 - p0) ** 2), d + math.sqrt((u1 - p1) ** 2 + (p0 - l0) ** 2))


def mcnemar_exact(b, c):
    assert isinstance(b, int) and b >= 0
    assert isinstance(c, int) and c >= 0
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, 2 * tail)


def mann_whitney_p(xs, ys):
    """Two-sided normal-approximation Mann-Whitney U with tie correction."""
    assert all(math.isfinite(value) for value in xs)
    assert all(math.isfinite(value) for value in ys)
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
    assert draws > 0
    assert all(value >= 0 for value in new + old), 'negative measurement'
    if not new or not old or statistics.fmean(old) <= 0:
        return (float('nan'),) * 3
    rng = random.Random(seed)
    est = statistics.fmean(new) / statistics.fmean(old)
    samples = []
    for _ in range(draws):
        a = statistics.fmean(rng.choices(new, k=len(new)))
        b = statistics.fmean(rng.choices(old, k=len(old)))
        if b > 0:
            samples.append(a / b)
    if not samples:
        return (float('nan'),) * 3
    samples.sort()
    return (est, samples[int(0.025 * len(samples))], samples[int(0.975 * len(samples)) - 1])


def graded(rows, arm):
    return [r for r in rows if r['arm'] == arm and not r['infra']]


def stage_stats(row):
    """Stage records and delegated reviewer agents over the trial's items."""
    assert isinstance(row['items'], list)
    assert all(isinstance(item['stages'], list) and isinstance(item.get('delegations', {}), dict) for item in row['items'])
    stages = sum(len(i['stages']) for i in row['items'])
    reviewers = sum(len(d['agents']) for i in row['items'] for d in i.get('delegations', {}).values())
    return stages, reviewers


def arm_summary(rows, arm):
    assert arm in ('archon', 'aether')
    assert all(row['arm'] in ('archon', 'aether') for row in rows)
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
        'cost_gaps': dict(Counter(r['cost_gap'] for r in g if r.get('cost_gap'))),
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
    assert len(old) == len(graded(old_rows, arm)), 'duplicate baseline pairs'
    assert len(new) == len(graded(new_rows, arm)), 'duplicate candidate pairs'
    shared = sorted(set(old) & set(new))
    b = sum(old[k]['success'] and not new[k]['success'] for k in shared)
    c = sum(new[k]['success'] and not old[k]['success'] for k in shared)
    both = sum(old[k]['success'] and new[k]['success'] for k in shared)
    neither = len(shared) - b - c - both
    return {'pairs': len(shared), 'old_only': b, 'new_only': c, 'both_pass': both,
            'both_fail': neither, 'p': mcnemar_exact(b, c)}


def compare(old_rows, new_rows, arm):
    assert arm in ('archon', 'aether')
    o, n = arm_summary(old_rows, arm), arm_summary(new_rows, arm)
    assert o['pass'] <= o['graded'] and n['pass'] <= n['graded']
    d, lo, hi = newcombe_diff(n['pass'], n['graded'], o['pass'], o['graded'])
    wr = ratio_ci(n['wall'], o['wall'])
    cr = ratio_ci(n['cost'], o['cost'])
    return {'old': o, 'new': n, 'diff': [d, lo, hi], 'mcnemar': paired(old_rows, new_rows, arm),
            'wall_ratio': list(wr), 'wall_p': mann_whitney_p(n['wall'], o['wall']),
            'cost_ratio': list(cr), 'cost_p': mann_whitney_p(n['cost'], o['cost'])}


def task_table(old_rows, new_rows, arm):
    assert arm in ('archon', 'aether')
    assert all(row['task'] for row in old_rows + new_rows), 'empty task name'
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
    """Item histories and the first error of one trial."""
    assert row['arm'] in ('archon', 'aether') and row['wall_secs'] >= 0
    assert isinstance(row['items'], list)
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


def findings(old_rows, new_rows, cmp_arm_tasks, arm):
    """Tasks that flipped pass to fail or got slower, with item history."""
    assert arm in ('archon', 'aether')
    assert cmp_arm_tasks, 'no tasks to compare'
    assert len({t['task'] for t in cmp_arm_tasks}) == len(cmp_arm_tasks), 'duplicate tasks'
    flips, slow = [], []
    for t in cmp_arm_tasks:
        o, n = t['old'], t['new']
        task_old = [r for r in old_rows if r['task'] == t['task']]
        task_new = [r for r in new_rows if r['task'] == t['task']]
        pairs = paired(task_old, task_new, arm)
        if pairs['old_only'] or pairs['new_only']:
            rows = [r for r in task_new if r['arm'] == arm and not r['infra']]
            baseline = [r for r in task_old if r['arm'] == arm and not r['infra']]
            flips.append({'task': t['task'], 'pairs': pairs,
                          'old': f"{o['pass']}/{o['graded']}", 'new': f"{n['pass']}/{n['graded']}",
                          'old_history': [trial_history(r) for r in baseline],
                          'history': [trial_history(r) for r in rows]})
        old_trials = {r['trial']: r for r in graded(task_old, arm)}
        new_trials = {r['trial']: r for r in graded(task_new, arm)}
        slow_pairs = []
        for trial in sorted(old_trials.keys() & new_trials.keys()):
            before, after = old_trials[trial]['wall_secs'], new_trials[trial]['wall_secs']
            if before > 0 and after >= 2 * before and after - before >= 60:
                slow_pairs.append({'trial': trial, 'old_wall': before, 'new_wall': after})
        slower = t['wall_ratio'] >= 1.5 and n['wall_mean'] - o['wall_mean'] >= 30
        if slower or slow_pairs:
            rows = [r for r in new_rows if r['arm'] == arm and r['task'] == t['task'] and not r['infra']]
            slow.append({'task': t['task'], 'ratio': t['wall_ratio'], 'p': t['wall_p'],
                         'old_mean': o['wall_mean'], 'new_mean': n['wall_mean'],
                         'slow_pairs': slow_pairs, 'history': [trial_history(r) for r in rows]})
    return flips, slow


TEST_FAILURE_MARKERS = ('FAILED', 'AssertionError', 'assertion', 'panicked', 'expect_', 'FAIL:', 'mismatch')


def failure_reason(row):
    """Why a failed trial failed: timeout, harness, wrong output or grader."""
    assert not row['success'], 'only failed trials have a failure reason'
    assert isinstance(row['logs'], Path)
    if row['timed_out']:
        return 'timeout'
    if row['error'] is not None:
        return f"harness: {row['error']}"
    patches = list(row['logs'].glob('*.result.patch'))
    if patches and all(path.stat().st_size == 0 for path in patches):
        return 'wrong output: no change delivered (empty patch)'
    grade = ''.join(path.read_text(errors='replace') for path in (row['logs'] / 'grade.stdout', row['logs'] / 'grade.stderr')
                    if path.is_file())
    if any(marker in grade for marker in TEST_FAILURE_MARKERS):
        return 'wrong output: the hidden tests failed'
    if row['score'] is not None:
        return f"wrong output: score {row['score']:.2f}"
    return 'grader: no test failure recorded in the grade log'


def parity(rows):
    """Archon against direct Aether on one run: pass counts overall and per task."""
    assert rows and all(r['arm'] in ('archon', 'aether') for r in rows)
    tasks = sorted({r['task'] for r in rows})
    per_task, short = [], []
    for task in tasks:
        counts = {arm: arm_summary([r for r in rows if r['task'] == task], arm) for arm in ('archon', 'aether')}
        a, d = counts['archon'], counts['aether']
        per_task.append({'task': task, 'archon': f"{a['pass']}/{a['graded']}", 'aether': f"{d['pass']}/{d['graded']}",
                         'delta': a['pass'] - d['pass']})
        if a['pass'] < d['pass']:
            failed = sorted((r for r in rows if r['task'] == task and r['arm'] == 'archon' and not r['success']),
                            key=lambda r: r['trial'])
            short.append({'task': task, 'archon': a['pass'], 'aether': d['pass'],
                          'failures': [{'trial': r['trial'], 'wall': r['wall_secs'], 'reason': failure_reason(r)}
                                       for r in failed]})
    archon, aether = (arm_summary(rows, arm) for arm in ('archon', 'aether'))
    assert archon['graded'] == aether['graded'], 'arms ran different trial counts'
    return {'archon': archon['pass'], 'aether': aether['pass'], 'graded': archon['graded'],
            'verdict': 'PARITY MET' if archon['pass'] >= aether['pass'] else 'PARITY MISSED',
            'tasks': per_task, 'shortfalls': short}


def verdict(per_model):
    assert per_model, 'no model comparisons'
    assert all('archon' in data for data in per_model.values()), 'missing Archon comparison'
    reasons = []
    for m, d in per_model.items():
        c = d['archon']
        assert all(c[arm]['cost_trials'] == c[arm]['graded'] for arm in ('old', 'new')), f'{m}: unpriced trials'
        assert all(math.isfinite(value) for value in c['diff']), f'{m}: ungraded comparison'
        assert all(math.isfinite(value) for value in c['wall_ratio'] + c['cost_ratio']), f'{m}: unpriced or untimed comparison'
        if c['diff'][2] < 0:
            reasons.append(f'{m}: Archon pass rate fell significantly (diff interval entirely below 0)')
        if c['mcnemar']['p'] < 0.05 and c['mcnemar']['old_only'] > c['mcnemar']['new_only']:
            reasons.append(f"{m}: McNemar p={c['mcnemar']['p']:.3f} with more pass-to-fail than fail-to-pass pairs")
        cr = c['cost_ratio']
        if cr[1] > 1.0 and c['cost_p'] < 0.05:
            reasons.append(f"{m}: Archon cost rose significantly (ratio {cr[0]:.2f}, interval {cr[1]:.2f}-{cr[2]:.2f}, p={c['cost_p']:.3f})")
        wr = c['wall_ratio']
        if wr[1] > 1.0 and c['wall_p'] < 0.05:
            reasons.append(f"{m}: Archon wall time rose significantly (ratio {wr[0]:.2f}, interval {wr[1]:.2f}-{wr[2]:.2f}, p={c['wall_p']:.3f})")
    return ('REGRESSION' if reasons else 'NO REGRESSION'), reasons


def pair_entry(base, cand):
    entry = {'archon': compare(base['rows'], cand['rows'], 'archon'),
             'aether': compare(base['rows'], cand['rows'], 'aether'),
             'tasks_archon': task_table(base['rows'], cand['rows'], 'archon'),
             'tasks_aether': task_table(base['rows'], cand['rows'], 'aether')}
    entry['flips'], entry['slow'] = findings(base['rows'], cand['rows'], entry['tasks_archon'], 'archon')
    entry['aether_flips'], entry['aether_slow'] = findings(base['rows'], cand['rows'], entry['tasks_aether'], 'aether')
    return entry


def analyze_model(runs_dir, model):
    old, new = (load_run(runs_dir / f'{arm}-{model}') for arm in ('old', 'new'))
    assert old['meta']['models'] == new['meta']['models'], 'models differ between runs'
    assert old['meta']['tasks'] == new['meta']['tasks'], 'task sets differ between runs'
    assert old['meta']['trials'] == new['meta']['trials'] == 3
    assert all(run['meta']['completed'] == run['meta']['planned'] for run in (old, new)), 'incomplete run'
    for run in (old, new):
        assert run['start'].get('exit') == 0, 'run did not exit successfully'
        keys = [(row['task'], row['arm'], row['trial']) for row in run['rows']]
        assert len(keys) == run['meta']['planned'], 'missing trial records'
        assert len(set(keys)) == len(keys), 'duplicate trial records'
    return {'comparison': pair_entry(old, new),
            'arms': {name: arm_summary(run['rows'], 'archon') for name, run in (('old', old), ('new', new))},
            'aether_arms': {name: arm_summary(run['rows'], 'aether') for name, run in (('old', old), ('new', new))},
            'parity': {name: parity(run['rows']) for name, run in (('old', old), ('new', new))},
            'meta': {name: run['meta'] for name, run in (('old', old), ('new', new))},
            'starts': {name: run['start'] for name, run in (('old', old), ('new', new))},
            'infra_errors': {name: [{'task': row['task'], 'arm': row['arm'], 'trial': row['trial'], 'error': row['error']}
                                    for row in run['rows'] if row['infra']]
                             for name, run in (('old', old), ('new', new))}}


def json_safe(value, depth=0):
    assert 0 <= depth < 32, 'summary nesting is too deep'
    if isinstance(value, float) and not math.isfinite(value):
        return None
    if isinstance(value, list):
        return [json_safe(item, depth + 1) for item in value]
    if isinstance(value, dict):
        assert all(isinstance(key, str) for key in value), 'summary keys must be strings'
        return {key: json_safe(item, depth + 1) for key, item in value.items()}
    return value


def main():
    runs_dir, out_dir = (Path(arg) for arg in sys.argv[1:3])
    selected = sys.argv[3:] or list(MODELS)
    assert selected and all(model in MODELS for model in selected)
    assert runs_dir.is_dir(), 'runs directory is missing'
    per_model = {model: analyze_model(runs_dir, model) for model in selected}
    word, reasons = verdict({model: {'archon': data['comparison']['archon']} for model, data in per_model.items()})
    budget_file = runs_dir / 'budget.json'
    budgets = json.loads(budget_file.read_text()) if budget_file.is_file() else {}
    parity_word = 'PARITY MET' if all(d['parity']['new']['verdict'] == 'PARITY MET' for d in per_model.values()) else 'PARITY MISSED'
    summary = {'verdict': word, 'reasons': reasons, 'parity_verdict': parity_word, 'models': per_model,
               'budgets': budgets, 'html': 'report.html'}
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / 'summary.json').write_text(json.dumps(json_safe(summary), indent=2, allow_nan=False))
    print(word, reasons, parity_word)


if __name__ == '__main__':
    main()
