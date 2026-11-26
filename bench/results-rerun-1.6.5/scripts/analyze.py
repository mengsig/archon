"""Summarise rerun trials: pass rates, Fisher exact p, bootstrap ratios, failure list."""
import json
import math
import random
import statistics
import sys
from pathlib import Path

TARGETED = ['multi-repo-pagination', 'quality-safe-extract', 'quality-retry-budget']


def load(runs):
    rows = []
    for start in sorted(runs.glob('*/start.json')):
        meta = json.loads(start.read_text())
        found = [json.loads(line) for p in start.parent.glob('results/*/results.jsonl') for line in p.read_text().splitlines()]
        assert len(found) <= 1, start
        if not found:
            rows.append({**meta, 'missing': True})
            continue
        row = found[0]
        cost = (row.get('cost_usd') or 0) + (row.get('estimated_cost_usd') or 0)
        rows.append({**meta, 'success': bool(row['success']), 'timed_out': bool(row.get('timed_out')),
                     'wall': row['wall_secs'], 'cost': cost, 'error': row.get('error'), 'dir': start.parent.name})
    return rows


def fisher_two_sided(a, b, c, d):
    """Exact two-sided p for the 2x2 table [[a, b], [c, d]]."""
    n, row1, col1 = a + b + c + d, a + b, a + c
    assert n > 0 and row1 <= n and col1 <= n

    def prob(x):
        return math.comb(row1, x) * math.comb(n - row1, col1 - x) / math.comb(n, col1)
    observed = prob(a)
    lo, hi = max(0, col1 - (n - row1)), min(row1, col1)
    return min(1.0, sum(prob(x) for x in range(lo, hi + 1) if prob(x) <= observed * (1 + 1e-9)))


def bootstrap_ratio(new, old, draws=4000, seed=7):
    assert new and old
    rng = random.Random(seed)
    ratios = sorted(statistics.fmean(rng.choices(new, k=len(new))) / statistics.fmean(rng.choices(old, k=len(old)))
                    for _ in range(draws))
    return statistics.fmean(new) / statistics.fmean(old), ratios[int(0.025 * draws)], ratios[int(0.975 * draws) - 1]


def pass_stats(rows):
    out = {}
    for arm in ('old', 'new'):
        graded = [r for r in rows if r['arm'] == arm and 'success' in r]
        out[arm] = (sum(r['success'] for r in graded), len(graded))
    (po, no), (pn, nn) = out['old'], out['new']
    p = fisher_two_sided(po, no - po, pn, nn - pn) if no and nn else None
    return out, p


def ratios(rows):
    result = {}
    for key in ('cost', 'wall'):
        old = [r[key] for r in rows if r['arm'] == 'old' and 'success' in r]
        new = [r[key] for r in rows if r['arm'] == 'new' and 'success' in r]
        result[key] = bootstrap_ratio(new, old) if old and new else None
    return result


def main(runs_dir, out_dir):
    rows = load(Path(runs_dir))
    sets = {'A_targeted': [r for r in rows if r['task'] in TARGETED],
            'B_broad': [r for r in rows if r['task'] not in TARGETED],
            'all': rows}
    summary = {}
    for name, subset in sets.items():
        counts, p = pass_stats(subset)
        summary[name] = {'pass': counts, 'fisher_p': p, 'ratios': ratios(subset)}
    per_task = {}
    for r in rows:
        if 'success' in r:
            cell = per_task.setdefault(r['task'], {}).setdefault(r['arm'], [])
            cell.append(r)
    summary['per_task'] = {t: {a: {'pass': sum(x['success'] for x in v), 'n': len(v),
                                   'wall': statistics.fmean(x['wall'] for x in v),
                                   'cost': statistics.fmean(x['cost'] for x in v)} for a, v in arms.items()}
                           for t, arms in per_task.items()}
    summary['failures'] = [{k: r[k] for k in ('task', 'trial', 'arm', 'timed_out', 'wall', 'dir', 'error')}
                           for r in rows if 'success' in r and not r['success']]
    summary['missing'] = [r for r in rows if r.get('missing')]
    summary['load5_at_start'] = {'min': min(r['load5_at_start'] for r in rows), 'max': max(r['load5_at_start'] for r in rows)}
    summary['spent_usd'] = sum(r['cost'] for r in rows if 'cost' in r)
    (Path(out_dir) / 'summary.json').write_text(json.dumps(summary, indent=2, default=str))
    print(json.dumps(summary, indent=2, default=str))


if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2])
