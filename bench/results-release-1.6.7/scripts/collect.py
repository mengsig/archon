"""Copy the trial evidence needed to reproduce the report."""
import json
import sqlite3
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import redact  # noqa: E402


def copy_sanitized(source, destination):
    assert source.is_file()
    destination.write_bytes(redact.redact_bytes(source.read_bytes()))


def collect_run(source, destination):
    assert (source / 'start.json').is_file(), f'missing start record: {source}'
    stamps = list((source / 'results').iterdir())
    assert len(stamps) == 1, f'expected one result stamp in {source}'
    stamp = stamps[0]
    if destination.exists():
        expected = redact.redact_bytes((source / 'start.json').read_bytes())
        assert (destination / 'start.json').read_bytes() == expected, 'existing run differs'
    destination.mkdir(parents=True, exist_ok=True)
    copy_sanitized(source / 'start.json', destination / 'start.json')
    for name in ('run.json', 'results.jsonl', 'summary.md'):
        path = stamp / name
        if path.is_file():
            copy_sanitized(path, destination / name)
    assert (destination / 'run.json').is_file() and (destination / 'results.jsonl').is_file()
    for name in ('answer.txt', 'archon-items.json', 'grade.stdout', 'grade.stderr', '*.result.patch'):
        for path in (stamp / 'trials').rglob(name):
            target = destination / path.relative_to(stamp)
            target.parent.mkdir(parents=True, exist_ok=True)
            copy_sanitized(path, target)


def database_spend(run_dir):
    path = run_dir / 'aether-data/aether.db'
    assert path.is_file(), f'missing usage database: {path}'
    with sqlite3.connect(f'file:{path}?mode=ro', uri=True) as db:
        row = db.execute('SELECT COALESCE(SUM(cost_usd + COALESCE(estimated_cost_usd, 0)), 0), COUNT(*) FROM agent_usage').fetchone()
    assert row is not None and row[0] >= 0 and row[1] >= 0
    return {'usd': row[0], 'agents': row[1]}


def row_spend(run_dir):
    """Spend summed from trial rows, for a run whose usage database was lost."""
    stamps = list((run_dir / 'results').iterdir())
    assert len(stamps) == 1, f'expected one result stamp in {run_dir}'
    rows = [json.loads(line) for line in (stamps[0] / 'results.jsonl').read_text().splitlines() if line.strip()]
    usd = sum((r['cost_usd'] or 0) + (r['estimated_cost_usd'] or 0) for r in rows)
    assert usd >= 0 and rows, f'no spend rows in {run_dir}'
    return {'usd': usd, 'trials': len(rows), 'source': 'trial rows (usage database lost)'}


def run_spend(run_dir):
    if (run_dir / 'aether-data/aether.db').is_file():
        return database_spend(run_dir)
    return row_spend(run_dir)


def collect_budgets(source, destination, extra):
    """Total private-daemon spend per model, including any rerun labels."""
    assert source.is_dir() and destination.is_dir()
    assert not destination.resolve().is_relative_to(source.resolve()), 'output must be outside source'
    budgets = {}
    for model in ('luna', 'haiku'):
        runs = {version: database_spend(source / f'{version}-{model}') for version in ('old', 'new')}
        reruns = {label: run_spend(source / label) for label in extra if label.endswith(model)}
        total = sum(entry['usd'] for entry in list(runs.values()) + list(reruns.values()))
        budgets[model] = {'runs': runs, 'reruns': reruns, 'total_usd': total}
    (destination / 'budget.json').write_text(json.dumps(budgets, indent=2) + '\n')


def main():
    source, destination = map(Path, sys.argv[1:3])
    extra = sys.argv[3:]
    assert source.is_dir()
    destination.mkdir(parents=True, exist_ok=True)
    for label in ['old-luna', 'new-luna', 'old-haiku', 'new-haiku'] + extra:
        collect_run(source / label, destination / label)
    collect_budgets(source, destination, extra)


if __name__ == '__main__':
    main()
