"""Copy the trial evidence needed to reproduce the report."""
import json
import shutil
import sqlite3
import sys
from pathlib import Path


def collect_run(source, destination):
    assert (source / 'start.json').is_file(), f'missing start record: {source}'
    stamps = list((source / 'results').iterdir())
    assert len(stamps) == 1, f'expected one result stamp in {source}'
    stamp = stamps[0]
    destination.mkdir(parents=True, exist_ok=False)
    shutil.copy2(source / 'start.json', destination / 'start.json')
    for name in ('run.json', 'results.jsonl', 'summary.md'):
        path = stamp / name
        if path.is_file():
            shutil.copy2(path, destination / name)
    assert (destination / 'run.json').is_file() and (destination / 'results.jsonl').is_file()
    for name in ('answer.txt', 'archon-items.json', 'grade.stdout', 'grade.stderr', '*.result.patch'):
        for path in (stamp / 'trials').rglob(name):
            target = destination / path.relative_to(stamp)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)


def database_spend(run_dir):
    path = run_dir / 'aether-data/aether.db'
    assert path.is_file(), f'missing usage database: {path}'
    with sqlite3.connect(f'file:{path}?mode=ro', uri=True) as db:
        row = db.execute('SELECT COALESCE(SUM(cost_usd + COALESCE(estimated_cost_usd, 0)), 0), COUNT(*) FROM agent_usage').fetchone()
    assert row is not None and row[0] >= 0 and row[1] >= 0
    return {'usd': row[0], 'agents': row[1]}


def collect_budgets(source, destination):
    assert source.is_dir() and destination.is_dir()
    budgets = {}
    for model in ('luna', 'haiku'):
        versions = {version: database_spend(source / f'{version}-{model}') for version in ('old', 'new')}
        probes = {}
        if model == 'luna':
            for label in ('old-luna-protocol-probe', 'new-luna-budget-probe'):
                probes[label] = database_spend(source / label)
                collect_run(source / label, destination / 'probes' / label)
        total = sum(entry['usd'] for entry in list(versions.values()) + list(probes.values()))
        budgets[model] = {'runs': versions, 'probes': probes, 'total_usd': total}
    (destination / 'budget.json').write_text(json.dumps(budgets, indent=2) + '\n')



def main():
    source, destination = map(Path, sys.argv[1:3])
    assert source.is_dir() and not destination.exists()
    for label in ('old-luna', 'new-luna', 'old-haiku', 'new-haiku'):
        collect_run(source / label, destination / label)
    collect_budgets(source, destination)
    for model in ('luna', 'haiku'):
        guard = source / f'{model}-guard.log'
        assert guard.is_file(), f'missing {model} guard log'
        shutil.copy2(guard, destination / guard.name)


if __name__ == '__main__':
    main()
