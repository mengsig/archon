"""Copy the trial evidence needed to reproduce the report."""
import json
import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path


def copy_sanitized(source, destination, email):
    assert source.is_file() and email
    data = source.read_bytes().replace(email, b'[redacted-email]')
    assert email not in data
    destination.write_bytes(data)


def collect_run(source, destination):
    assert (source / 'start.json').is_file(), f'missing start record: {source}'
    stamps = list((source / 'results').iterdir())
    assert len(stamps) == 1, f'expected one result stamp in {source}'
    stamp = stamps[0]
    if destination.exists():
        assert (destination / 'start.json').read_bytes() == (source / 'start.json').read_bytes(), 'existing run differs'
    destination.mkdir(parents=True, exist_ok=True)
    email = subprocess.check_output(['git', 'config', 'user.email']).strip()
    assert email, 'configured git email is required for redaction'
    copy_sanitized(source / 'start.json', destination / 'start.json', email)
    for name in ('run.json', 'results.jsonl', 'summary.md'):
        path = stamp / name
        if path.is_file():
            copy_sanitized(path, destination / name, email)
    assert (destination / 'run.json').is_file() and (destination / 'results.jsonl').is_file()
    for name in ('answer.txt', 'archon-items.json', 'grade.stdout', 'grade.stderr', '*.result.patch'):
        for path in (stamp / 'trials').rglob(name):
            target = destination / path.relative_to(stamp)
            target.parent.mkdir(parents=True, exist_ok=True)
            copy_sanitized(path, target, email)


def database_spend(run_dir):
    path = run_dir / 'aether-data/aether.db'
    assert path.is_file(), f'missing usage database: {path}'
    with sqlite3.connect(f'file:{path}?mode=ro', uri=True) as db:
        row = db.execute('SELECT COALESCE(SUM(cost_usd + COALESCE(estimated_cost_usd, 0)), 0), COUNT(*) FROM agent_usage').fetchone()
    assert row is not None and row[0] >= 0 and row[1] >= 0
    return {'usd': row[0], 'agents': row[1]}


def collect_budgets(source, destination):
    assert source.is_dir() and destination.is_dir()
    assert not destination.resolve().is_relative_to(source.resolve()), 'output must be outside source'
    budgets = {}
    for model in ('luna', 'haiku'):
        versions = {version: database_spend(source / f'{version}-{model}') for version in ('old', 'new')}
        probes = {}
        labels = ('old-luna-protocol-probe', 'new-luna-budget-probe') if model == 'luna' else ('old-haiku-serial-probe', 'new-haiku-serial-probe')
        for label in labels:
            probes[label] = database_spend(source / label)
            collect_run(source / label, destination / 'probes' / label)
        total = sum(entry['usd'] for entry in list(versions.values()) + list(probes.values()))
        budgets[model] = {'runs': versions, 'probes': probes, 'total_usd': total}
    (destination / 'budget.json').write_text(json.dumps(budgets, indent=2) + '\n')



def main():
    source, destination = map(Path, sys.argv[1:3])
    assert source.is_dir()
    destination.mkdir(parents=True, exist_ok=True)
    for label in ('old-luna', 'new-luna', 'old-haiku', 'new-haiku'):
        collect_run(source / label, destination / label)
    collect_budgets(source, destination)
    for model in ('luna', 'haiku', 'haiku-serial'):
        guard = source / f'{model}-guard.log'
        assert guard.is_file(), f'missing {model} guard log'
        shutil.copy2(guard, destination / guard.name)


if __name__ == '__main__':
    main()
