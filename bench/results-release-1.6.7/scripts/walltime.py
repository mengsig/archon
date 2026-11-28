"""Wall time across releases, every timed-out trial with its cause, and host overlap.

Usage, from the repository root:
  python3 walltime.py <results-release-1.6.7 dir> <load.log> > walltime.json
Reads stored results only: results-full-1.6.4, results-full-1.6.5 and this run's runs/.
"""
import datetime
import json
import statistics
import sys
from pathlib import Path

BENCH = Path(__file__).resolve().parents[2]
RUST_RERUN = ('multi-repo-currency', 'refactor-order-status')
# Host jobs of this item beside the bench, in UTC. All four arms ran side by side from 07:20Z,
# after the candidate's release build; make ci ran on the candidate source before the bench (21:59-22:14Z).
HOST_JOBS = [('release build (candidate)', '2026-11-28T07:13:09', '2026-11-28T07:19:30')]
# Causes read by hand from a run's Aether event log where item counts alone mislead.
TRACED_CAUSES = {
    ('1.6.7', 'luna', 'quality-ledger-transfer', 2):
        'waiting: after 23 s the lead sent a raw send_message to its run-node parent asking whether '
        'locking needs an escalation, then wait_for on it; that parent never replies, so it idled until the limit',
}


def rows(run_dir):
    path = run_dir / 'results.jsonl'
    assert path.is_file(), f'missing {path}'
    out = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    assert out and all('wall_secs' in r and 'task' in r for r in out), f'{path}: bad rows'
    for r in out:
        r['dir'] = run_dir
    return out


def release_rows(model, runs):
    """Every graded trial of each release, keyed by version."""
    assert model in ('haiku', 'luna')
    v164 = BENCH / 'results-full-1.6.4/runs'
    first = [r for r in rows(v164 / f'new-{model}') if r['task'] not in RUST_RERUN]
    rust = [r for r in rows(v164 / f'new-{model}-rust') if r['task'] in RUST_RERUN]
    found = {'1.6.4': first + rust,
             '1.6.5': rows(BENCH / f'results-full-1.6.5/runs/new-{model}'),
             '1.6.6': rows(runs / f'old-{model}')}
    if (runs / f'new-{model}/results.jsonl').is_file():
        found['1.6.7'] = rows(runs / f'new-{model}')
    return found


def wall_table(by_version, arm):
    assert arm in ('archon', 'aether')
    assert {'1.6.4', '1.6.5', '1.6.6'} <= set(by_version) <= {'1.6.4', '1.6.5', '1.6.6', '1.6.7'}
    table = {}
    for version, trials in by_version.items():
        mine = [r for r in trials if r['arm'] == arm and r['error'] is None]
        tasks = sorted({r['task'] for r in mine})
        table[version] = {
            'effort': mine[0]['effort'] if mine else None, 'trials': len(mine),
            'total_min': sum(r['wall_secs'] for r in mine) / 60,
            'mean_s': statistics.fmean(r['wall_secs'] for r in mine) if mine else None,
            'timeouts': sum(bool(r['timed_out']) for r in mine),
            'tasks': {t: statistics.fmean(r['wall_secs'] for r in mine if r['task'] == t) for t in tasks}}
    return table


def item_facts(row):
    path = row['dir'] / 'trials' / row['task'] / row['arm'] / str(row['trial']) / 'logs/archon-items.json'
    if not path.is_file():
        return None
    items = json.loads(path.read_text())
    assert isinstance(items, list), f'{path}: items must be a list'
    reviewers = sum(len(d['agents']) for it in items for d in it.get('delegations', {}).values())
    errors = [s['state'].get('error') or '' for it in items for s in it['stages']]
    states = [s['state'].get('state') for it in items for s in it['stages']]
    return {'items': len(items), 'reviewers': reviewers, 'errors': errors, 'states': states}


def cause(row, version):
    """The likeliest reason a trial ran into its time limit, from its stored logs."""
    assert row['timed_out'], 'only timed-out trials have a timeout cause'
    traced = TRACED_CAUSES.get((version, 'luna' if 'luna' in row['model'] else 'haiku', row['task'], row['trial']))
    if traced:
        return traced
    if row['error'] is not None:
        return f"infrastructure: {row['error']}"
    if row['arm'] != 'archon':
        return 'single agent still working at the limit (engine turn time)'
    facts = item_facts(row)
    if facts is None or facts['items'] == 0:
        return 'orchestrator queued no work before the limit (waiting)'
    if any('failed 3 rounds' in e for e in facts['errors']):
        return (f"review loop: 3 failed review rounds ({facts['reviewers']} reviewers), escalated, "
                'then waiting on the orchestrator until the limit')
    if facts['reviewers'] >= 4:
        return f"review loop: {facts['reviewers']} reviewers, still reviewing at the limit"
    if facts['reviewers'] == 0:
        return 'implement lead still working at the limit, no reviewer delegated yet (long engine turns)'
    return f"review under way at the limit ({facts['reviewers']} reviewers)"


def parse(stamp):
    """A UTC time from a naive ISO stamp or one with `Z` and up to nanoseconds."""
    assert stamp and 'T' in stamp, f'not an ISO time: {stamp!r}'
    stamp = stamp.removesuffix('Z')
    whole, _, fraction = stamp.partition('.')
    parsed = datetime.datetime.fromisoformat(whole).replace(tzinfo=datetime.timezone.utc)
    return parsed + datetime.timedelta(microseconds=int(fraction[:6].ljust(6, '0'))) if fraction else parsed


def overlaps(row, load):
    start, end = parse(row['started_at']), parse(row['finished_at'])
    assert end >= start, 'a trial ends before it starts'
    jobs = [name for name, a, b in HOST_JOBS if parse(a) < end and parse(b) > start]
    during = [value for when, value in load if start <= when <= end]
    return jobs, (max(during) if during else None)


def read_load(path):
    out = []
    for line in path.read_text().splitlines():
        parts = line.split()
        if len(parts) >= 3:
            out.append((datetime.datetime.fromisoformat(parts[0].replace('Z', '+00:00')), float(parts[2])))
    assert out, 'empty load log'
    return out


def timed_out(by_version, load):
    out = []
    for version, trials in by_version.items():
        for r in trials:
            if not r['timed_out']:
                continue
            jobs, peak = overlaps(r, load)
            out.append({'version': version, 'task': r['task'], 'arm': r['arm'], 'trial': r['trial'],
                        'wall_s': round(r['wall_secs']), 'graded_pass': r['success'],
                        'cause': cause(r, version), 'overlapped': jobs, 'peak_load5': peak})
    return out


def host_overlap(by_version, load):
    out = {}
    for version in [v for v in ('1.6.6', '1.6.7') if v in by_version]:
        trials = by_version[version]
        hit = [r for r in trials if overlaps(r, load)[0]]
        names = sorted({job for r in hit for job in overlaps(r, load)[0]})
        out[version] = {'trials': len(trials), 'overlapping': len(hit), 'jobs': names}
    return out


def main():
    out_dir, load_path = Path(sys.argv[1]), Path(sys.argv[2])
    assert (out_dir / 'runs').is_dir(), 'collect the runs first'
    load = read_load(load_path)
    result = {}
    for model in ('haiku', 'luna'):
        by_version = release_rows(model, out_dir / 'runs')
        result[model] = {'archon': wall_table(by_version, 'archon'), 'aether': wall_table(by_version, 'aether'),
                         'timeouts': timed_out(by_version, load), 'overlap': host_overlap(by_version, load)}
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
