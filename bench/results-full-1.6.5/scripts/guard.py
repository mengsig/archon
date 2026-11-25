"""Watch the private Luna runs' combined spend and host load."""
import os
from pathlib import Path
import signal
import sqlite3
import sys
import time


def owned(pid, expected):
    assert pid > 0 and expected in {'archon-bench-old', 'archon-bench-new'}
    cmdline = Path(f'/proc/{pid}/cmdline')
    try:
        args = cmdline.read_bytes().split(b'\0')
    except FileNotFoundError:
        return False
    return bool(args[0]) and Path(os.fsdecode(args[0])).name == expected


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
    old_pid, new_pid = map(int, sys.argv[1:3])
    assert old_pid > 0 and new_pid > 0 and old_pid != new_pid
    sandbox = Path('/home/mengsig/.local/share/archon-bench/full-165-Q4J35ZNK')
    report = Path('.archon/reports/bench')
    probe = sum(spend(report / f'{label}-luna-{name}-probe/aether-data/aether.db')
                for label, name in [('old', 'protocol'), ('new', 'budget')])
    assert sandbox.is_dir() and report.is_dir()
    while owned(old_pid, 'archon-bench-old') or owned(new_pid, 'archon-bench-new'):
        try:
            active = list(sandbox.glob('r-*/archon-bench/a-data/aether.db'))
            finished = [report / f'{label}-luna/aether-data/aether.db' for label in ('old', 'new')]
            total = probe + sum(spend(path) for path in active + finished)
            load = os.getloadavg()[1]
            print(f'load5={load:.2f} estimated_combined_usd={total:.4f}', flush=True)
            if total >= 29:
                print('Combined model spend approached the $30 guard', flush=True)
                stop(old_pid, 'archon-bench-old')
                stop(new_pid, 'archon-bench-new')
                return 1
            if load > 12 and owned(new_pid, 'archon-bench-new'):
                print('Load exceeded 12; dropping to one run', flush=True)
                stop(new_pid, 'archon-bench-new')
            time.sleep(20)
        except (OSError, sqlite3.Error) as error:
            print(f'Guard failed: {error}', flush=True)
            stop(old_pid, 'archon-bench-old')
            stop(new_pid, 'archon-bench-new')
            raise
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
