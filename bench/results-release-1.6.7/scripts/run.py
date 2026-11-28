"""Run one full-suite bench arm (old or new) for one model in a private sandbox.

Usage, from the repository root: python3 run.py <old|new> <haiku|luna> <label> [--only TASKS]
"""
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
# Not gitignored, so a worktree clean of ignored files cannot delete the raw data.
REPORT = WORK / 'bench/results-release-1.6.7/.work'
HOST = Path.home()
SANDBOX = HOST / '.local/share/archon-bench/release-167-R4V9TB'
MODELS = {'haiku': 'claude-code/claude-haiku-5-5', 'luna': 'codex/gpt-6-luna'}
BINARIES = {'old': ('dl/old-archon/archon', 'dl/old-aether/aether'),
            'new': ('bin/archon-new', 'dl/new-aether/aether')}
BENCH = REPORT / 'bin/archon-bench'
CREDENTIALS = ['.claude/.credentials.json', '.codex/auth.json']


def private_environment(base):
    root, home = base / 'archon-bench', base / 'home'
    for name in ['bin', 'run', 'tmp', 'a-config', 'a-data', 'c-config', 'c-data']:
        (root / name).mkdir(parents=True)
    for name in ['.claude', '.codex']:
        (home / name).mkdir(parents=True)
    for name in CREDENTIALS:
        assert (HOST / name).is_file(), f'missing engine credentials {name}'
        shutil.copyfile(HOST / name, home / name)
        (home / name).chmod(0o600)
    (home / '.claude.json').write_text('{"hasCompletedOnboarding":true}')
    identity = subprocess.check_output(['git', 'config', 'user.name'], text=True).strip()
    email = subprocess.check_output(['git', 'config', 'user.email'], text=True).strip()
    assert identity and email, 'the bench commits need the local git identity'
    (home / '.gitconfig').write_text(f'[user]\nname = {identity}\nemail = {email}\n')
    env = {'HOME': str(home), 'USER': os.environ['USER'], 'LANG': 'C.UTF-8',
           'PATH': f'{root}/bin:{HOST}/.cargo/bin:{HOST}/.local/bin:{HOST}/.nvm/versions/node/v22.22.2/bin:/usr/local/bin:/usr/bin:/bin',
           'RUSTUP_HOME': str(HOST / '.rustup'), 'CARGO_HOME': str(root / 'cargo'), 'CARGO_BUILD_JOBS': '2',
           'XDG_DATA_HOME': str(base), 'XDG_CONFIG_HOME': str(home / '.config'),
           'XDG_CACHE_HOME': str(home / '.cache'), 'TMPDIR': str(root / 'tmp'),
           'AETHER_CONFIG_DIR': str(root / 'a-config'), 'AETHER_DATA_DIR': str(root / 'a-data'),
           'AETHER_SOCKET': str(root / 'run/a.sock'),
           'ARCHON_CONFIG_DIR': str(root / 'c-config'), 'ARCHON_DATA_DIR': str(root / 'c-data'),
           'ARCHON_SOCKET': str(root / 'run/c.sock')}
    assert len(env['AETHER_SOCKET']) < 108 and len(env['ARCHON_SOCKET']) < 108, 'socket path too long'
    return root, env


def setup(root, env, arm, temp_root):
    assert arm in BINARIES
    assert temp_root.is_dir() and temp_root.parent == SANDBOX
    hashes = {}
    for name, relative in zip(('archon', 'aether'), BINARIES[arm]):
        source = REPORT / relative
        assert source.is_file(), source
        shutil.copy2(source, root / 'bin' / name)
        hashes[name] = hashlib.sha256(source.read_bytes()).hexdigest()
    hashes['archon-bench'] = hashlib.sha256(BENCH.read_bytes()).hexdigest()
    versions = {name: subprocess.check_output([str(root / 'bin' / name), '--version'], env=env, text=True).strip()
                for name in ('archon', 'aether')}
    config = subprocess.check_output([str(root / 'bin/archon'), 'setup', '--print'], env=env)
    config += f'\n[agents]\ntemp_root = "{temp_root}"\n'.encode()
    (root / 'a-config/config.toml').write_bytes(config)
    (root / 'c-config/config.toml').write_text(f'[aether]\nmode = "shared"\nsocket = "{env["AETHER_SOCKET"]}"\n')
    return hashes, versions


def stop_daemon(proc, root, env):
    """Stop the private aetherd this script started, by its socket and then its own PID."""
    assert proc.pid > 0
    assert str(root) in env['AETHER_SOCKET']
    if proc.poll() is not None:
        return
    stopped = subprocess.run([str(root / 'bin/aether'), 'stop', '--socket', env['AETHER_SOCKET']],
                             env=env, capture_output=True, timeout=30, check=False)
    if stopped.returncode:
        print(f'Stop request exited {stopped.returncode}; terminating owned PID {proc.pid}', flush=True)
        proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait(timeout=10)


def start_daemon(root, env, destination):
    assert not (root / 'run/a.sock').exists(), 'a stale private socket exists'
    log = (destination / 'aether.log').open('w')
    proc = subprocess.Popen([str(root / 'bin/aether'), 'daemon', '--foreground'],
                            env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    (destination / 'aether.pid').write_text(str(proc.pid))
    for _ in range(60):
        if proc.poll() is not None:
            raise RuntimeError(f'private aetherd exited {proc.returncode}')
        ready = subprocess.run([str(root / 'bin/aether'), 'status', '--socket', env['AETHER_SOCKET']],
                               env=env, capture_output=True, timeout=10, check=False)
        if ready.returncode == 0:
            return proc
        time.sleep(1)
    stop_daemon(proc, root, env)
    raise RuntimeError('private aetherd did not become ready')


def bench_command(root, env, model, destination, only):
    assert model in MODELS
    assert str(root) in env['AETHER_SOCKET']
    return [str(BENCH), 'run', '--archon-bin', str(root / 'bin/archon'),
            '--aether-socket', env['AETHER_SOCKET'], '--results-dir', str(destination / 'results'),
            '--arms', 'archon,aether', '--trials', '3', '--parallel', '3',
            '--effort', 'high', '--max-usd', '37', '--model', MODELS[model]] + (['--only', only] if only else [])


def run_bench(root, env, record, destination, only):
    proc = start_daemon(root, env, destination)
    try:
        command = bench_command(root, env, record['model'], destination, only)
        record['command'] = [Path(command[0]).name] + command[1:]
        print(f'Start {destination.name}: load5={record["load5"]:.2f}', flush=True)
        with (destination / 'bench.log').open('w') as bench_log:
            outcome = subprocess.run(command, env=env, stdout=bench_log, stderr=subprocess.STDOUT, check=False)
        record['exit'] = outcome.returncode
        record['end'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (destination / 'start.json').write_text(json.dumps(record, indent=2))
        print(f'Finished {destination.name}: exit={outcome.returncode}', flush=True)
    finally:
        stop_daemon(proc, root, env)
    shutil.copytree(root / 'a-data', destination / 'aether-data',
                    ignore=shutil.ignore_patterns('t', 'bin', 'aether.build-cache'))
    return record['exit']


def run(arm, model, label, only=''):
    assert arm in BINARIES and model in MODELS
    assert label and '/' not in label
    while os.getloadavg()[1] >= 12:
        print(f'Waiting: load5={os.getloadavg()[1]:.2f}', flush=True)
        time.sleep(30)
    destination = REPORT / label
    destination.mkdir()
    SANDBOX.mkdir(mode=0o700, parents=True, exist_ok=True)
    temp_root = SANDBOX / f'{model[0]}{arm[0]}'
    temp_root.mkdir(mode=0o700)
    base = Path(tempfile.mkdtemp(prefix='r-', dir=SANDBOX))
    try:
        root, env = private_environment(base)
        hashes, versions = setup(root, env, arm, temp_root)
        record = {'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                  'arm': arm, 'model': model, 'only': only, 'load5': os.getloadavg()[1],
                  'hashes': hashes, 'versions': versions,
                  'start': datetime.datetime.now(datetime.timezone.utc).isoformat()}
        (destination / 'start.json').write_text(json.dumps(record, indent=2))
        return run_bench(root, env, record, destination, only)
    finally:
        shutil.rmtree(base)
        shutil.rmtree(temp_root)


if __name__ == '__main__':
    os.umask(0o077)
    args = sys.argv[1:]
    only = ''
    if '--only' in args:
        index = args.index('--only')
        only = args[index + 1]
        del args[index:index + 2]
    raise SystemExit(run(*args, only=only))
