#!/usr/bin/env python3
"""Install/manage the current user's Portal LaunchAgent (Python 3.9+, no packages)."""
import argparse
import ctypes
import getpass
import hashlib
import os
from pathlib import Path
import plistlib
import re
import signal
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qs, urlsplit


def saved(root, name):
    path = root / name
    return path.read_text().strip() if path.exists() else ''


def private_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(content)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def validate_link(link):
    # Do not include the supplied URL in errors or diagnostics.
    try:
        uri = urlsplit(link)
        being = uri.path.strip('/').split('/')[0]
        valid = (uri.scheme in ('http', 'https') and uri.hostname and being
                 and not uri.username and not uri.password and uri.port != 0
                 and parse_qs(uri.query).get('token', [''])[0])
    except ValueError:
        valid = False
    if not valid:
        raise ValueError('Connection requires an HTTP(S) Loom URL, Being ID and non-empty token.')
    return being


def label_for(root):
    return 'town.beings.heart-portal.' + hashlib.sha256(os.fsencode(root)).hexdigest()[:16]


def launchctl(*args, check=True):
    result = subprocess.run(['/bin/launchctl', *args], capture_output=True, text=True)
    if check and result.returncode:
        raise RuntimeError(f'launchctl {args[0]} failed: {result.stderr.strip()}')
    return result


def definition(root, label):
    return {
        'Label': label,
        'ProgramArguments': ['/bin/sh', str(root / 'scripts/portal-launchagent.sh'), str(root), label],
        'WorkingDirectory': str(root),
        'RunAtLoad': True,
        'KeepAlive': True,  # Includes successful exits from portal_restart.
        'ThrottleInterval': 5,  # A launch rate limit, not a fixed post-exit delay.
        'ExitTimeOut': 15,  # Portal's managed-process cleanup is bounded to 10s.
        'AbandonProcessGroup': False,
        'Umask': 0o077,
        'EnvironmentVariables': {
            # launchd does not load interactive shell profiles. Preserve kit runtimes.
            'PATH': os.environ.get('PATH', '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'),
        },
        'StandardOutPath': str(root / 'portal-launchagent.log'),
        'StandardErrorPath': str(root / 'portal-launchagent.err.log'),
    }


def assert_owned(path, root, label):
    if path.exists():
        value = plistlib.loads(path.read_bytes())
        expected = definition(root, label)
        if (value.get('Label') != label
                or value.get('ProgramArguments') != expected['ProgramArguments']
                or value.get('WorkingDirectory') != str(root)):
            raise RuntimeError('Existing LaunchAgent belongs to another checkout; refusing to replace it.')


def executable_path(pid):
    libproc = ctypes.CDLL('/usr/lib/libproc.dylib')
    libproc.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    libproc.proc_pidpath.restype = ctypes.c_int
    buffer = ctypes.create_string_buffer(4096)
    if libproc.proc_pidpath(pid, buffer, len(buffer)) > 0:
        return Path(os.fsdecode(buffer.value)).resolve()
    return None


def checkout_pids(root):
    # Never match substrings of command lines or kill another checkout's Portal.
    binary = (root / 'target/release/heart-portal').resolve()
    rows = subprocess.check_output(['/bin/ps', '-axo', 'pid=,uid='], text=True)
    return [int(pid) for pid, uid in (row.split() for row in rows.splitlines())
            if int(uid) == os.getuid() and int(pid) != os.getpid()
            and executable_path(int(pid)) == binary]


def stop_checkout(root):
    binary = (root / 'target/release/heart-portal').resolve()
    pids = checkout_pids(root)
    for pid in pids:
        try:
            if executable_path(pid) == binary:
                os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 15
    while pids and time.monotonic() < deadline:
        pids = [pid for pid in pids if executable_path(pid) == binary]
        if pids:
            time.sleep(0.1)
    for pid in pids:
        try:
            if executable_path(pid) == binary:
                os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 5
    while checkout_pids(root):
        if time.monotonic() >= deadline:
            raise RuntimeError('Previous Portal did not stop; refusing to start a duplicate.')
        time.sleep(0.1)


def restore_manual(root):
    """Recover the previous unsupervised service if LaunchAgent startup fails."""
    env = os.environ.copy()
    env.pop('HEART_PORTAL_SUPERVISED', None)
    env.pop('PORTAL_CONNECT_LINK', None)
    link = saved(root, '.portal-connection.url')
    if link:
        env['PORTAL_CONNECT_LINK'] = link
    command = [str(root / 'target/release/heart-portal'), '--config', str(root / 'portal.toml')]
    name = saved(root, '.portal-name')
    if name:
        command += ['--name', name]
    with open(root / 'portal-runtime.log', 'ab') as out, open(root / 'portal-runtime.err.log', 'ab') as err:
        subprocess.Popen(command, cwd=root, env=env, stdin=subprocess.DEVNULL,
                         stdout=out, stderr=err, start_new_session=True)


def install(args, root, path, label, domain, service):
    binary = root / 'target/release/heart-portal'
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError('Build first: cargo build --release --locked')
    if not (root / 'scripts/portal-launchagent.sh').is_file():
        raise RuntimeError('Missing scripts/portal-launchagent.sh')
    link = args.connect_link or os.environ.get('PORTAL_CONNECT_LINK') or saved(root, '.portal-connection.url')
    if not link and sys.stdin.isatty():
        link = getpass.getpass('Loom connection URL (hidden): ')
    being = validate_link(link.strip())
    name = args.name or saved(root, '.portal-name')
    if not name:
        host = os.uname().nodename.split('.')[0].lower()
        name = re.sub(r'[^A-Za-z0-9_-]', '-', f'{being}-{host}').strip('-_')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]*', name):
        raise ValueError('Portal name must use letters, digits, hyphens or underscores.')
    config = root / 'portal.toml'
    config_data = config.read_bytes() if config.exists() else (root / 'portal.example.toml').read_bytes()
    # Parse the actual config before interrupting a working service.
    validation = subprocess.run([str(binary), '--config', str(config if config.exists() else root / 'portal.example.toml'), 'kit', 'status'], capture_output=True)
    if validation.returncode:
        raise RuntimeError('Portal config validation failed; run heart-portal --config portal.toml kit status.')
    launchctl('print', domain)  # Requires this user's logged-in GUI session.
    assert_owned(path, root, label)
    running = launchctl('print', service, check=False).returncode == 0
    if running and not path.exists():
        raise RuntimeError('Loaded service has no owned plist; refusing to replace it.')
    updates = {
        config: config_data,
        root / '.portal-connection.url': (link.strip() + '\n').encode(),
        root / '.portal-name': (name + '\n').encode(),
        root / '.portal-launchagent-label': (label + '\n').encode(),
        path: plistlib.dumps(definition(root, label)),
    }
    backups = {file: file.read_bytes() if file.exists() else None for file in updates}
    had_manual = not running and bool(checkout_pids(root))
    (root / 'workspace').mkdir(exist_ok=True)
    if running:
        launchctl('bootout', service)
    bootstrapped = False
    try:
        stop_checkout(root)
        for file, data in updates.items():
            private_write(file, data)
        launchctl('enable', service)
        launchctl('bootstrap', domain, str(path))
        bootstrapped = True
        deadline = time.monotonic() + 12
        while not checkout_pids(root):
            if time.monotonic() >= deadline:
                raise RuntimeError('Portal did not start; inspect portal-launchagent.err.log and portal-runtime.err.log.')
            time.sleep(0.1)
    except Exception:
        if bootstrapped:
            launchctl('bootout', service, check=False)
        for file, data in backups.items():
            if data is None:
                file.unlink(missing_ok=True)
            else:
                private_write(file, data)
        if running:
            launchctl('bootstrap', domain, str(path), check=False)
        elif had_manual and config.exists():
            restore_manual(root)
        raise
    print(f'Installed and started {label} for Portal {name}.')
    print('Starts at user login; launchd restarts Portal after exits. Relay reconnect stays in Portal.')
    print(f'Logs: {root / "portal-runtime.log"}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['install', 'uninstall', 'status'])
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--name', help='Reuse the original Portal name on first installation.')
    parser.add_argument('--connect-link', help='Loom URL; prefer saved file, environment or hidden prompt.')
    args = parser.parse_args()
    if sys.platform != 'darwin' or os.getuid() == 0:
        parser.error('Run as the logged-in macOS user, without sudo.')
    root = args.root.resolve(strict=True)
    label = label_for(root)
    path = Path.home() / 'Library/LaunchAgents' / f'{label}.plist'
    domain = f'gui/{os.getuid()}'
    service = f'{domain}/{label}'
    assert_owned(path, root, label)
    if args.action == 'install':
        install(args, root, path, label, domain, service)
    elif args.action == 'uninstall':
        loaded = launchctl('print', service, check=False).returncode == 0
        if loaded and not path.exists():
            raise RuntimeError('Loaded service has no owned plist; refusing to remove it.')
        if loaded:
            launchctl('bootout', service)
        stop_checkout(root)
        path.unlink(missing_ok=True)
        print(f'Removed {label}; local config, credentials and name are preserved.')
    else:
        result = launchctl('print', service, check=False)
        if result.returncode:
            print(f'{label} is not loaded.')
            return 1
        # Avoid dumping launchd's inherited environment (which can contain secrets).
        for line in result.stdout.splitlines():
            if re.match(r'\s*(state|pid|runs|last exit code|last terminating signal) =', line):
                print(line.strip())
        print(f'LaunchAgent: {path}')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, plistlib.InvalidFileException) as error:
        print(f'Error: {error}', file=sys.stderr)
        sys.exit(1)
