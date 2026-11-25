"""Watch a model's private runs for combined spend and host load."""
import os
import signal
import sqlite3
import sys
import time
from pathlib import Path


def owned(pid, expected):
    assert pid > 0
    assert expected in {'archon-bench-old', 'archon-bench-new'}
    cmdline = Path(f'/proc/{pid}/cmdline')
    try:
        args = cmdline.read_bytes().split(b'\0')
    except FileNotFoundError:
        return False
    binary = Path('.archon/reports/bench/bin') / expected
    return bool(args[0]) and Path(os.fsdecode(args[0])).resolve() == binary.resolve()


def spend(path):
    assert path.name == 'aether.db'
    if not path.exists():
        return 0.0
    try:
        with sqlite3.connect(f'file:{path}?mode=ro', uri=True, timeout=5) as db:
            row = db.execute('SELECT COALESCE(SUM(cost_usd + COALESCE(estimated_cost_usd, 0)), 0) FROM agent_usage').fetchone()
    except sqlite3.OperationalError:
        if not path.exists():
            return 0.0
        raise
    assert row is not None and row[0] >= 0
    return row[0]


def stop(pid, expected):
    if owned(pid, expected):
        print(f'Stopping owned {expected} PID {pid}', flush=True)
        os.kill(pid, signal.SIGINT)


def main():
    model = sys.argv[1]
    old_pid, new_pid = map(int, sys.argv[2:4])
    assert model in ('luna', 'haiku')
    assert old_pid > 0 and new_pid > 0 and old_pid != new_pid
    sandbox = Path('/home/mengsig/.local/share/archon-bench/full-165-Q4J35ZNK')
    report = Path('.archon/reports/bench')
    labels = ['old-luna-protocol-probe', 'new-luna-budget-probe'] if model == 'luna' else ['old-haiku-serial-probe', 'new-haiku-serial-probe']
    probe_paths = [report / label / 'aether-data/aether.db' for label in labels]
    assert all(path.is_file() for path in probe_paths), 'missing probe usage'
    probe = sum(spend(path) for path in probe_paths)
    assert sandbox.is_dir() and report.is_dir()
    while owned(old_pid, 'archon-bench-old') or owned(new_pid, 'archon-bench-new'):
        try:
            active = list(sandbox.glob('r-*/archon-bench/a-data/aether.db'))
            finished = [report / f'{label}-{model}/aether-data/aether.db' for label in ('old', 'new')]
            total = probe + sum(spend(path) for path in active + finished)
            load = os.getloadavg()[1]
            print(f'load5={load:.2f} estimated_combined_usd={total:.4f}', flush=True)
            if total >= 29:
                print('Combined model spend approached the $30 guard', flush=True)
                stop(old_pid, 'archon-bench-old')
                stop(new_pid, 'archon-bench-new')
                return 1
            if load > 12:
                print('Load exceeds 12; operator review may be needed', flush=True)
            time.sleep(20)
        except (OSError, sqlite3.Error) as error:
            print(f'Guard failed: {error}', flush=True)
            stop(old_pid, 'archon-bench-old')
            stop(new_pid, 'archon-bench-new')
            raise
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
