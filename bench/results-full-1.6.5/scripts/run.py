import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

WORK = Path.cwd()
REPORT = WORK / '.archon/reports/bench'
HOST = Path('/home/mengsig')
SANDBOX = HOST / '.local/share/archon-bench/full-165-Q4J35ZNK'
CANDIDATE_COMMIT = 'c4bcaa9f8eff80ca81917f9ddaf65a866abb3690'


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
    return root, env


def setup(root, env, arm, temp_root):
    archon = REPORT / f'dl/{arm}/archon'
    aether = REPORT / 'dl/aether/aether'
    hashes = {}
    for name, source in [('archon', archon), ('aether', aether)]:
        assert source.is_file(), source
        shutil.copy2(source, root / 'bin' / name)
        hashes[name] = hashlib.sha256(source.read_bytes()).hexdigest()
    hashes['archon-bench'] = hashlib.sha256(bench_binary(arm).read_bytes()).hexdigest()
    assert (root / 'bin/aether').stat().st_mode & 0o111
    config = subprocess.check_output([str(root / 'bin/archon'), 'setup', '--print'], env=env)
    assert temp_root.is_dir() and temp_root.parent == SANDBOX
    config += f'\n[agents]\ntemp_root = "{temp_root}"\n'.encode()
    (root / 'a-config/config.toml').write_bytes(config)
    (root / 'c-config/config.toml').write_text(f'[aether]\nmode = "shared"\nsocket = "{env["AETHER_SOCKET"]}"\n')
    return hashes


def stop_daemon(proc, root, env):
    assert proc.pid > 0
    assert str(root) in env['AETHER_SOCKET']
    if proc.poll() is None:
        stopped = subprocess.run([str(root / 'bin/aether'), 'stop', '--socket', env['AETHER_SOCKET']],
                                 env=env, capture_output=True, timeout=30)
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


def bench_binary(arm):
    return REPORT / f'bin/archon-bench-{"old" if arm == "old" else "new"}'


def bench_command(root, env, model, destination, parallel, arm, only, budget):
    assert model in ['haiku', 'luna']
    assert str(root) in env['AETHER_SOCKET']
    model_id = 'codex/gpt-6-luna' if model == 'luna' else 'claude-code/claude-haiku-5-5'
    return [str(bench_binary(arm)), 'run', '--archon-bin', str(root / 'bin/archon'),
            '--aether-socket', env['AETHER_SOCKET'], '--results-dir', str(destination / 'results'),
            '--arms', 'archon,aether', '--trials', '3', '--parallel', str(parallel),
            '--effort', 'medium', '--max-usd', budget, '--model', model_id] + (['--only', only] if only else [])


def run(arm, model, label, parallel='1', only='', budget='30'):
    assert arm in ['old', 'new']
    assert model in ['haiku', 'luna']
    assert float(budget) > 0 and float(budget) <= 30
    if model == 'haiku' and datetime.datetime.now(datetime.timezone.utc) < datetime.datetime(2026, 11, 25, 6, 5, tzinfo=datetime.timezone.utc):
        raise RuntimeError('Haiku trials must not start before 06:05 UTC')
    while os.getloadavg()[1] >= 12:
        print(f'Waiting: load5={os.getloadavg()[1]:.2f}', flush=True)
        time.sleep(30)
    destination = REPORT / label
    destination.mkdir()
    SANDBOX.mkdir(mode=0o700, parents=True, exist_ok=True)
    temp_root = SANDBOX / f'{model[0]}{arm[0]}'
    temp_root.mkdir(mode=0o700)
    base = None
    proc = None
    try:
        base = Path(tempfile.mkdtemp(prefix='r-', dir=SANDBOX))
        root, env = private_environment(base)
        hashes = setup(root, env, arm, temp_root)
        record = {'candidate_commit': CANDIDATE_COMMIT,
                  'head': subprocess.check_output(['git','rev-parse','HEAD'], text=True).strip(),
                  'arm': arm, 'model': model, 'parallel': parallel, 'load5': os.getloadavg()[1], 'hashes': hashes,
                  'start': datetime.datetime.now(datetime.timezone.utc).isoformat()}
        (destination / 'start.json').write_text(json.dumps(record, indent=2))
        with (destination / 'aether.log').open('w') as log:
            proc = subprocess.Popen([str(root / 'bin/aether'), 'daemon', '--foreground'],
                                    env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            (destination / 'aether.pid').write_text(str(proc.pid))
            for _ in range(60):
                if proc.poll() is not None:
                    raise RuntimeError(f'aetherd exited {proc.returncode}')
                ready = subprocess.run([str(root / 'bin/aether'), 'status', '--socket', env['AETHER_SOCKET']],
                                       env=env, capture_output=True, timeout=10)
                if ready.returncode == 0:
                    break
                time.sleep(1)
            else:
                raise RuntimeError('private aetherd did not become ready')
            command = bench_command(root, env, model, destination, parallel, arm, only, budget)
            print(f'Start {label}: load5={record["load5"]:.2f}', flush=True)
            with (destination / 'smoke.log').open('w') as bench_log:
                outcome = subprocess.run(command, env=env, stdout=bench_log, stderr=subprocess.STDOUT)
            record['exit'] = outcome.returncode
            record['end'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            (destination / 'start.json').write_text(json.dumps(record, indent=2))
            print(f'Finished {label}: exit={outcome.returncode}', flush=True)
            stop_daemon(proc, root, env)
            shutil.copytree(root / 'a-data', destination / 'aether-data',
                            ignore=shutil.ignore_patterns('t', 'bin', 'aether.build-cache'))
            return outcome.returncode
    finally:
        if proc is not None and proc.poll() is None:
            stop_daemon(proc, root, env)
        if base is not None:
            shutil.rmtree(base)
        shutil.rmtree(temp_root)


if __name__ == '__main__':
    os.umask(0o077)
    raise SystemExit(run(*sys.argv[1:]))
