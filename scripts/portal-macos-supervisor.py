#!/usr/bin/env python3
"""Session supervisor inherited from the original Terminal/app, with no TCC owner switch."""
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
STAGE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('manager', STAGE / 'portal-macos.py')
manager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manager)


def read(path):
    try:
        return json.loads(manager.metadata_text(path))
    except (OSError, ValueError):
        return {}


def write(path, value):
    manager.private_write(path, json.dumps(value).encode())


def recover(root):
    """Only recover a journal after the worker has released its exclusive lock."""
    try:
        with manager.maintenance_lock(root):
            journal = read(root / '.portal-upgrade.json')
            if not journal:
                return None
            stage = Path(journal['stage']).resolve()
            if stage.parent != (root / '.portal-upgrades').resolve():
                raise ValueError('Upgrade journal points outside this installation.')
    except RuntimeError:  # The worker still owns maintenance.
        return None
    # Release maintenance before spawning. Do not wait for completion: recovery
    # asks this same supervisor to restart Portal and acknowledge readiness.
    with open(stage / 'worker.log', 'ab') as log:
        return subprocess.Popen([sys.executable, str(stage / 'portal-macos-upgrade.py')],
                                cwd=stage, stdin=subprocess.DEVNULL, stdout=log,
                                stderr=log, start_new_session=True)


def watch(request):
    root, target = Path(request['root']), Path(request['target'])
    stopping = False

    def stop_signal(*_):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop_signal)
    signal.signal(signal.SIGINT, stop_signal)
    with open(root / '.portal-supervisor.lock', 'a+b') as owner_lock:
        try:
            fcntl.flock(owner_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError('A Portal supervisor already owns this installation.')
        runtime = manager.process_identity(request['runtime_pid'])
        if (not runtime or runtime['executable'] != str(target)
                or request.get('runtime', runtime) != runtime):
            raise RuntimeError('Original Portal exited before supervision was ready.')
        state = {'protocol': 1, 'owner': manager.process_identity(os.getpid()),
                 'token': request['token'], 'runtime': runtime, 'kind': 'inherited-session'}
        manager.private_write(root / '.portal-launch-nonce', request['token'].encode())
        (root / '.portal-ready.json').unlink(missing_ok=True)
        write(root / '.portal-supervisor.json', state)
        child, gate, handled, recovery = None, None, None, None
        recovery_retry = 0
        startup_deadline, retry_at = 0, 0
        # A live pre-supervisor release cannot publish our readiness marker.
        # Its upgrade worker has explicitly adopted it before stopping it.
        armed = bool(request.get('adopted'))
        try:
            while not stopping and manager.saved(root, '.portal-supervisor-stop') != request['token']:
                if child:
                    child.poll()  # Reap our own exited children before inspecting PIDs.
                if ((root / '.portal-upgrade.json').exists() and time.monotonic() >= recovery_retry
                        and (recovery is None or recovery.poll() is not None)):
                    recovery = recover(root)
                    if recovery:
                        recovery_retry = time.monotonic() + 5
                alive = manager.identity_alive(runtime)
                if read(root / '.portal-ready.json').get('pid') == runtime['pid']:
                    armed = True
                if not alive and not armed and not (root / '.portal-upgrade.json').exists():
                    # A foreground config/bind/startup failure must remain a
                    # visible failure, not turn into a background retry loop.
                    break
                if gate and (not alive or time.monotonic() >= startup_deadline
                             or read(root / '.portal-ready.json').get('pid') == runtime['pid']):
                    gate.close()
                    gate = None
                if not alive:
                    command = read(root / '.portal-supervisor-restart.json')
                    journal = read(root / '.portal-upgrade.json')
                    # Only the active transaction can cross maintenance. Its
                    # worker retains the exclusive lock through readiness/rollback.
                    transaction_start = (journal and command.get('transaction') == Path(journal.get('stage', '')).name
                                         and command.get('id') != handled and command.get('owner') == request['token'])
                    can_start = bool(transaction_start)
                    if not journal and time.monotonic() >= retry_at:
                        gate = open(root / '.portal-upgrade.lock', 'a+b')
                        try:
                            fcntl.flock(gate, fcntl.LOCK_SH | fcntl.LOCK_NB)
                            # Recheck after acquiring the lock; never race journal creation.
                            can_start = not (root / '.portal-upgrade.json').exists()
                        except BlockingIOError:
                            can_start = False
                        if not can_start:
                            gate.close()
                            gate = None
                    if can_start:
                        if transaction_start:
                            handled = command['id']
                        else:
                            manager.private_write(root / '.portal-launch-nonce', uuid.uuid4().hex.encode())
                        (root / '.portal-ready.json').unlink(missing_ok=True)
                        env = dict(os.environ, HEART_PORTAL_SUPERVISED='1', HEART_PORTAL_MACOS_SUPERVISOR=request['token'])
                        env.pop('HEART_PORTAL_UPGRADE_START', None)
                        env['HEART_PORTAL_LOG_FILE'] = str(root / 'portal-runtime.log')
                        try:
                            with open(root / 'portal-runtime.log', 'ab') as out, open(root / 'portal-runtime.err.log', 'ab') as err:
                                child = subprocess.Popen([str(target), *request['arguments']], cwd=request['cwd'], env=env,
                                                         stdin=subprocess.DEVNULL, stdout=out, stderr=err, start_new_session=True)
                        except OSError as error:
                            # Keep the original permission owner alive for the
                            # worker's rollback request, even if exec itself fails.
                            print(f'Portal launch failed: {error}', file=sys.stderr, flush=True)
                            if gate:
                                gate.close()
                                gate = None
                            retry_at = time.monotonic() + 5
                            time.sleep(.2)
                            continue
                        runtime = manager.process_identity(child.pid) or {'pid': child.pid, 'executable': str(target), 'started': ''}
                        state['runtime'] = runtime
                        write(root / '.portal-supervisor.json', state)
                        startup_deadline = time.monotonic() + 35
                        retry_at = time.monotonic() + 5
                time.sleep(.2)
        finally:
            if gate:
                gate.close()
            if read(root / '.portal-supervisor.json').get('token') == request['token']:
                (root / '.portal-supervisor.json').unlink(missing_ok=True)


def main():
    payload = sys.stdin.buffer.read(manager.METADATA_LIMIT + 1)
    if len(payload) > manager.METADATA_LIMIT:
        raise ValueError('Supervisor request exceeds 1 MiB.')
    request = json.loads(payload)
    root = Path(request['root'])
    action = sys.argv[1]
    if action == 'watch':
        watch(request)
    elif action == 'start':
        # A legacy updater may roll back after its first candidate launch. Do
        # not attach a new persistent watcher until that transaction commits.
        # Existing supervisors already coordinate through the shared protocol.
        deadline = time.monotonic() + 30
        while (root / '.portal-upgrade.json').exists():
            if time.monotonic() >= deadline:
                raise RuntimeError('Upgrade has not committed; supervision was not attached to the candidate.')
            time.sleep(.1)
        if manager.supervisor_state(root):
            raise RuntimeError('Portal supervisor is already running; use heart-portal status or stop.')
        if manager.checkout_pids(root, exclude=(request['runtime_pid'],)):
            raise RuntimeError('This installation already has a Portal process; use heart-portal stop before changing its settings.')
        with open(root / 'portal-supervisor.log', 'ab') as log:
            child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), 'watch'],
                                     stdin=subprocess.PIPE, stdout=log, stderr=log, start_new_session=True)
        child.stdin.write(json.dumps(request).encode())
        child.stdin.close()
        deadline = time.monotonic() + 15
        while read(root / '.portal-supervisor.json').get('token') != request['token']:
            if child.poll() is not None or time.monotonic() >= deadline:
                child.terminate()
                raise RuntimeError('Supervisor did not become ready; inspect portal-supervisor.log.')
            time.sleep(.05)
        print(request['token'])
    elif action == 'stop':
        with manager.maintenance_lock(root, timeout=3):
            if (root / '.portal-upgrade.json').exists():
                raise RuntimeError('An interrupted upgrade needs recovery before stopping.')
            label = manager.label_for(root)
            plist = Path.home() / 'Library/LaunchAgents' / (label + '.plist')
            manager.assert_owned(plist, root, label)
            service = f'gui/{os.getuid()}/{label}'
            if plist.exists() and not root.is_relative_to((Path.home() / '.heart-portal').resolve()):
                # A stopped legacy registration must not revive next login
                # after the new user installation takes over.
                manager.launchctl('disable', service)
            if manager.launchctl('print', service, check=False).returncode == 0:
                if not plist.exists():
                    raise RuntimeError('Loaded service has no owned plist; refusing to stop it.')
                manager.launchctl('bootout', service)
            manager.stop_supervisor(root)
            manager.stop_checkout(root, exclude=(request['runtime_pid'],))
        print('Portal and its supervisor stopped; configuration is preserved.')
    elif action == 'status':
        state = manager.supervisor_state(root)
        service = f'gui/{os.getuid()}/' + manager.label_for(root)
        print(json.dumps({'supervisor': {'kind': state['kind'], 'pid': state['owner']['pid']} if state else None,
                          'launchagent_loaded': manager.launchctl('print', service, check=False).returncode == 0,
                          'portal_pids': manager.checkout_pids(root, exclude=(request['runtime_pid'],))}, indent=2))
    else:
        raise RuntimeError('Unknown supervisor action.')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Error: {error}', file=sys.stderr)
        sys.exit(1)
