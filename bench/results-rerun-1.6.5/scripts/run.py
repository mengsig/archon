"""Interleaved OLD/NEW Archon trials on Haiku, one private daemon per trial."""
import datetime
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

WORK = Path.cwd()
RERUN = WORK / '.archon/reports/rerun'
OUT = WORK / 'bench/results-rerun-1.6.5/runs'
HOST = Path('/home/mengsig')
SANDBOX = HOST / '.local/share/archon-bench/rerun-165'
MODEL = 'claude-code/claude-haiku-5-5'
BUDGET_USD = 25.0
TARGETED = ['multi-repo-pagination', 'quality-safe-extract', 'quality-retry-budget']
BROAD = ['ask-cache-invalidation', 'review-seeded-bug', 'fix-off-by-one',
         'feature-token-bucket', 'refactor-order-status', 'long-backlog']
VERSION = {'old': '1.6.4', 'new': '1.6.5'}
BUSY = (b'cargo', b'rustc', b'playwright')
LOAD5_LIMIT = 8
LONG_LIVED_SECONDS = 3600


def schedule():
    """Rounds of one trial per cell; within a cell the arms alternate OLD, NEW."""
    plan = []
    for trial in range(1, 9):
        for task in TARGETED:
            plan += [(task, trial, 'old'), (task, trial, 'new')]
        if trial <= 3:
            for task in BROAD:
                plan += [(task, trial, 'old'), (task, trial, 'new')]
    assert len(plan) == 48 + 36
    return plan


def process_age(entry):
    try:
        fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
    except OSError:
        return 0.0
    started = int(fields[19]) / os.sysconf('SC_CLK_TCK')
    return float(Path('/proc/uptime').read_text().split()[0]) - started


def long_lived_browser(entry, name):
    """The operator's own browser, or a child of it, is exempt once its session is over an hour old."""
    if not name.startswith(b'chromium'):
        return False
    try:
        parent = int((entry / 'stat').read_text().rsplit(')', 1)[1].split()[1])
    except OSError:
        return False
    return process_age(entry) > LONG_LIVED_SECONDS or process_age(Path('/proc') / str(parent)) > LONG_LIVED_SECONDS


def busy_processes():
    found = []
    for entry in Path('/proc').iterdir():
        if not entry.name.isdigit():
            continue
        try:
            name = (entry / 'comm').read_bytes().strip()
        except OSError:
            continue
        if any(name.startswith(word) for word in BUSY) and not long_lived_browser(entry, name):
            found.append(f'{entry.name}:{name.decode()}')
    return found


def wait_quiet(label):
    while True:
        load1, load5 = os.getloadavg()[:2]
        busy = busy_processes()
        if load5 < LOAD5_LIMIT and not busy:
            return load1, load5
        print(f'{label} waiting load5={load5:.2f} busy={busy[:4]}', flush=True)
        time.sleep(30)


def private_environment(base):
    root = base / 'archon-bench'
    home = base / 'home'
    for name in ['bin', 'run', 'tmp', 'a-config', 'a-data', 'c-config', 'c-data']:
        (root / name).mkdir(parents=True)
    for name in ['.claude', '.codex']:
        (home / name).mkdir(parents=True)
    for name in ['.claude/.credentials.json', '.codex/auth.json']:
        source = HOST / name
        assert source.is_file(), source
        shutil.copyfile(source, home / name)
        (home / name).chmod(0o600)
    (home / '.claude.json').write_text('{"hasCompletedOnboarding":true}')
    identity = subprocess.check_output(['git', 'config', 'user.name'], text=True).strip()
    email = subprocess.check_output(['git', 'config', 'user.email'], text=True).strip()
    assert identity and email
    (home / '.gitconfig').write_text(f'[user]\nname = {identity}\nemail = {email}\n')
    env = {'HOME': str(home), 'USER': 'mengsig', 'LANG': 'C.UTF-8',
           'PATH': f'{root}/bin:/home/mengsig/.cargo/bin:/home/mengsig/.local/bin:/home/mengsig/.nvm/versions/node/v22.22.2/bin:/usr/local/bin:/usr/bin:/bin',
           'RUSTUP_HOME': '/home/mengsig/.rustup', 'CARGO_HOME': str(root / 'cargo'), 'CARGO_BUILD_JOBS': '2',
           'XDG_DATA_HOME': str(base), 'XDG_CONFIG_HOME': str(home / '.config'),
           'XDG_CACHE_HOME': str(home / '.cache'), 'TMPDIR': str(root / 'tmp'),
           'AETHER_CONFIG_DIR': str(root / 'a-config'), 'AETHER_DATA_DIR': str(root / 'a-data'),
           'AETHER_SOCKET': str(root / 'run/a.sock'),
           'ARCHON_CONFIG_DIR': str(root / 'c-config'), 'ARCHON_DATA_DIR': str(root / 'c-data'),
           'ARCHON_SOCKET': str(root / 'run/c.sock')}
    assert len(env['AETHER_SOCKET']) < 108
    return root, env, email


def install_binaries(root, env, arm, temp_root):
    archon = RERUN / f'dl/{arm}/archon'
    aether = RERUN / 'dl/aether/aether'
    hashes = {}
    for name, source in [('archon', archon), ('aether', aether)]:
        assert source.is_file(), source
        shutil.copy2(source, root / 'bin' / name)
        hashes[name] = hashlib.sha256(source.read_bytes()).hexdigest()
    harness = bench_binary(arm)
    hashes['archon-bench'] = hashlib.sha256(harness.read_bytes()).hexdigest()
    config = subprocess.check_output([str(root / 'bin/archon'), 'setup', '--print'], env=env)
    config += f'\n[agents]\ntemp_root = "{temp_root}"\n'.encode()
    (root / 'a-config/config.toml').write_bytes(config)
    (root / 'c-config/config.toml').write_text(f'[aether]\nmode = "shared"\nsocket = "{env["AETHER_SOCKET"]}"\n')
    return hashes


def bench_binary(arm):
    return RERUN / f'bin/archon-bench-{VERSION[arm]}'


def stop_daemon(proc, root, env):
    assert str(root) in env['AETHER_SOCKET']
    if proc.poll() is not None:
        return
    subprocess.run([str(root / 'bin/aether'), 'stop', '--socket', env['AETHER_SOCKET']],
                   env=env, capture_output=True, timeout=30, check=False)
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=15)


def wait_ready(proc, root, env):
    for _ in range(60):
        assert proc.poll() is None, f'aetherd exited {proc.returncode}'
        ready = subprocess.run([str(root / 'bin/aether'), 'status', '--socket', env['AETHER_SOCKET']],
                               env=env, capture_output=True, timeout=10, check=False)
        if ready.returncode == 0:
            return
        time.sleep(1)
    raise RuntimeError('private aetherd did not become ready')


def run_trial(task, trial, arm, destination, load1, load5):
    base = Path(tempfile.mkdtemp(prefix='r-', dir=SANDBOX))
    temp_root = Path(tempfile.mkdtemp(prefix='t-', dir=SANDBOX))
    proc = None
    try:
        root, env, email = private_environment(base)
        hashes = install_binaries(root, env, arm, temp_root)
        destination.mkdir(parents=True)
        record = {'task': task, 'trial': trial, 'arm': arm, 'version': VERSION[arm], 'load1_at_start': load1, 'load5_at_start': load5,
                  'hashes': hashes, 'start': datetime.datetime.now(datetime.timezone.utc).isoformat()}
        with (destination / 'aether.log').open('w') as log:
            proc = subprocess.Popen([str(root / 'bin/aether'), 'daemon', '--foreground'], env=env,
                                    stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            wait_ready(proc, root, env)
            command = [str(bench_binary(arm)), 'run', '--archon-bin', str(root / 'bin/archon'),
                       '--aether-socket', env['AETHER_SOCKET'], '--results-dir', str(destination / 'results'),
                       '--arms', 'archon', '--trials', '1', '--parallel', '1', '--effort', 'medium',
                       '--max-usd', '5', '--model', MODEL, '--only', task]
            with (destination / 'smoke.log').open('w') as bench_log:
                outcome = subprocess.run(command, env=env, stdout=bench_log, stderr=subprocess.STDOUT, check=False)
            record['exit'] = outcome.returncode
            record['end'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            stop_daemon(proc, root, env)
            shutil.copytree(root / 'a-data', destination / 'aether-data',
                            ignore=shutil.ignore_patterns('t', 'bin', 'aether.build-cache'))
        redact(destination, email.encode())
        (destination / 'start.json').write_text(json.dumps(record, indent=2))
        return record
    finally:
        if proc is not None:
            stop_daemon(proc, root, env)
        shutil.rmtree(base, ignore_errors=True)
        shutil.rmtree(temp_root, ignore_errors=True)


def redact(directory, email):
    assert email
    for path in directory.rglob('*'):
        if path.is_file() and path.suffix in {'.log', '.jsonl', '.json', '.txt', '.md', '.stdout', '.stderr', '.patch'}:
            data = path.read_bytes()
            if email in data:
                path.write_bytes(data.replace(email, b'[redacted-email]'))


def trial_cost(destination):
    rows = [json.loads(line) for path in destination.glob('results/*/results.jsonl') for line in path.read_text().splitlines()]
    assert len(rows) == 1, f'expected one result row in {destination}, got {len(rows)}'
    return (rows[0].get('cost_usd') or 0) + (rows[0].get('estimated_cost_usd') or 0)


def main():
    os.umask(0o077)
    SANDBOX.mkdir(mode=0o700, parents=True, exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    spent = sum(trial_cost(path.parent) for path in OUT.glob('*/start.json') if json.loads(path.read_text()).get('exit') is not None)
    for task, trial, arm in schedule():
        label = f'{task}-t{trial}-{arm}'
        destination = OUT / label
        if (destination / 'start.json').exists():
            continue
        if spent >= BUDGET_USD:
            print(f'Budget guard hit: ${spent:.2f}', flush=True)
            return 3
        load1, load5 = wait_quiet(label)
        print(f'Start {label} load1={load1:.2f} load5={load5:.2f} spent=${spent:.2f}', flush=True)
        run_trial(task, trial, arm, destination, load1, load5)
        spent += trial_cost(destination)
        print(f'Done {label} spent=${spent:.2f}', flush=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
