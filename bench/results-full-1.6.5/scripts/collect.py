"""Copy the trial evidence needed to reproduce the report."""
from pathlib import Path
import shutil
import sys


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


def main():
    source, destination = map(Path, sys.argv[1:3])
    assert source.is_dir() and not destination.exists()
    for label in ('old-luna', 'new-luna', 'old-haiku', 'new-haiku'):
        collect_run(source / label, destination / label)
    guard = source / 'luna-guard.log'
    assert guard.is_file()
    shutil.copy2(guard, destination / 'luna-guard.log')


if __name__ == '__main__':
    main()
