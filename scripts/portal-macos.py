#!/usr/bin/env python3
"""Install/manage the current user's Portal LaunchAgent (Python 3.9+, no packages)."""
import argparse
import ctypes
import fcntl
from contextlib import contextmanager
import getpass
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qs, urlsplit

LIFECYCLE_PROTOCOL = 2
METADATA_LIMIT = 1024 * 1024


def metadata_bytes(path):
    """Read one regular metadata snapshot, including a bound on concurrent growth."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > METADATA_LIMIT:
            raise ValueError('Portal metadata must be a regular file no larger than 1 MiB.')
        value = stream.read(METADATA_LIMIT + 1)
        if len(value) > METADATA_LIMIT:
            raise ValueError('Portal metadata exceeds 1 MiB.')
        return value


def metadata_text(path):
    return metadata_bytes(path).decode('utf-8-sig')


def binary_path(root):
    record = root / '.portal-executable'
    if record.is_file():
        path = Path(metadata_text(record).strip()).resolve()
        allowed = [root / 'target/release/heart-portal'] + [root / name for name in
                   ('heart-portal', 'heart-portal-macos-arm64', 'heart-portal-macos-x86_64')]
        if path not in allowed:
            raise RuntimeError('Saved Portal executable is outside this installation.')
        return path
    checkout = root / 'target/release/heart-portal'
    if checkout.is_file():
        return checkout
    binaries = [root / name for name in ('heart-portal', 'heart-portal-macos-arm64', 'heart-portal-macos-x86_64')
                if (root / name).is_file()]
    if len(binaries) > 1:
        raise RuntimeError('Keep only one Portal release executable in this installation directory.')
    return binaries[0] if binaries else checkout


@contextmanager
def maintenance_lock(root, timeout=0):
    # Kernel-owned lock: automatically released if management/updater crashes.
    with open(root / '.portal-upgrade.lock', 'a+b') as lock:
        deadline = time.monotonic() + timeout
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline or (root / '.portal-upgrade.json').exists():
                    raise RuntimeError('Portal maintenance/upgrade is in progress; retry after it completes.')
                time.sleep(.05)
        yield


def saved(root, name):
    path = root / name
    return metadata_text(path).strip() if path.exists() else ''


def private_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
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


def definition(root, label, config=None):
    return {
        'Label': label,
        'ProgramArguments': ['/bin/sh', str(root / 'scripts/portal-launchagent.sh'), str(root), label] + ([str(config)] if config else []),
        'WorkingDirectory': str(root),
        'RunAtLoad': True,
        'KeepAlive': True,  # Includes successful exits from portal_restart.
        'ThrottleInterval': 5,  # A launch rate limit, not a fixed post-exit delay.
        'ExitTimeOut': 15,  # Portal's managed-process cleanup is bounded to 10s.
        'AbandonProcessGroup': False,
        'Umask': 0o077,
        'EnvironmentVariables': {
            # launchd does not load interactive shell profiles. Preserve kit runtimes.
            'HOME': str(Path.home()),
            'PATH': os.environ.get('PATH', '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin'),
        },
        'StandardOutPath': str(root / 'portal-launchagent.log'),
        'StandardErrorPath': str(root / 'portal-launchagent.err.log'),
    }


def assert_owned(path, root, label):
    if path.exists():
        value = plistlib.loads(metadata_bytes(path))
        expected = definition(root, label)
        arguments = value.get('ProgramArguments', [])
        if (value.get('Label') != label
                or arguments[:4] != expected['ProgramArguments']
                or len(arguments) not in (4, 5)
                or (len(arguments) == 5 and not Path(arguments[4]).is_absolute())
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


def process_identity(pid):
    executable = executable_path(pid)
    if not executable:
        return None
    started = subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'lstart='],
                             env=dict(os.environ, TZ='UTC', LC_ALL='C'),
                             capture_output=True, text=True).stdout.strip()
    return {'pid': pid, 'executable': str(executable), 'started': started} if started else None


def identity_alive(identity):
    return bool(identity and process_identity(identity['pid']) == identity)


def launch_snapshot(identity):
    """Read a live legacy Portal's argv/cwd/environment without logging or saving secrets."""
    if not identity_alive(identity):
        raise RuntimeError('Original Portal exited before its launch settings could be captured.')
    # Darwin KERN_PROCARGS2 preserves argument boundaries (unlike ps command=).
    # Layout: argc, executable path, NUL padding, argc strings, environment.
    libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    libc.sysctl.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_uint, ctypes.c_void_p,
                           ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t]
    libc.sysctl.restype = ctypes.c_int
    mib = (ctypes.c_int * 3)(1, 49, identity['pid'])  # CTL_KERN, KERN_PROCARGS2
    size = ctypes.c_size_t()
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0 or size.value < 5:
        raise RuntimeError('Cannot read the original Portal launch settings; Portal was left running.')
    buffer = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0) != 0:
        raise RuntimeError('Cannot read the original Portal launch settings; Portal was left running.')
    data = buffer.raw[:size.value]
    try:
        argc = int.from_bytes(data[:4], sys.byteorder)
        position = data.index(b'\0', 4) + 1
        while data[position] == 0:
            position += 1
        arguments = []
        for _ in range(argc):
            end = data.index(b'\0', position)
            arguments.append(os.fsdecode(data[position:end]))
            position = end + 1
        environment = dict(value.split('=', 1) for value in
                           map(os.fsdecode, data[position:].split(b'\0')) if '=' in value)
        if not arguments:
            raise ValueError('Empty argument list')
    except (ValueError, IndexError):
        raise RuntimeError('Original Portal launch settings are incomplete; Portal was left running.') from None
    # Fixed-width Darwin ABI from sys/proc_info.h: vnode_info contains a
    # 136-byte vinfo_stat followed by 16 bytes of type/padding/fsid. Reading the
    # native path avoids lsof's escaping of non-printable directory names.
    class VnodePath(ctypes.Structure):
        _fields_ = [('info', ctypes.c_byte * 152), ('path', ctypes.c_char * 1024)]
    paths = (VnodePath * 2)()  # proc_vnodepathinfo: current directory, root directory
    libproc = ctypes.CDLL('/usr/lib/libproc.dylib')
    libproc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    libproc.proc_pidinfo.restype = ctypes.c_int
    count = libproc.proc_pidinfo(identity['pid'], 9, 0, ctypes.byref(paths), ctypes.sizeof(paths))
    directory = os.fsdecode(paths[0].path)
    if count != ctypes.sizeof(paths) or not directory or not Path(directory).is_dir():
        raise RuntimeError('Cannot read the original Portal working directory; Portal was left running.')
    if not identity_alive(identity):
        raise RuntimeError('Original Portal changed while capturing its launch settings; retry the upgrade.')
    return {'arguments': arguments[1:], 'cwd': directory, 'environment': environment}


def supervisor_state(root):
    try:
        state = json.loads(metadata_text(root / '.portal-supervisor.json'))
        return state if state.get('protocol') == 1 and identity_alive(state.get('owner')) else None
    except (OSError, ValueError, KeyError):
        return None


def stop_supervisor(root):
    state = supervisor_state(root)
    if not state:
        return
    private_write(root / '.portal-supervisor-stop', state['token'].encode())
    deadline = time.monotonic() + 10
    while identity_alive(state['owner']):
        if time.monotonic() >= deadline:
            raise RuntimeError('Portal supervisor did not stop; refusing to race it.')
        time.sleep(.1)


def checkout_pids(root, exclude=()):
    # Never match substrings of command lines or kill another checkout's Portal.
    binary = binary_path(root).resolve()
    rows = subprocess.check_output(['/bin/ps', '-axo', 'pid=,uid='], text=True)
    return [int(pid) for pid, uid in (row.split() for row in rows.splitlines())
            if int(uid) == os.getuid() and int(pid) != os.getpid() and int(pid) not in exclude
            and executable_path(int(pid)) == binary]


def stop_checkout(root, exclude=()):
    binary = binary_path(root).resolve()
    pids = checkout_pids(root, exclude)
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
    while checkout_pids(root, exclude):
        if time.monotonic() >= deadline:
            raise RuntimeError('Previous Portal did not stop; refusing to start a duplicate.')
        time.sleep(0.1)


def restore_manual(root, snapshot=None):
    """Recover the previous unsupervised service if LaunchAgent startup fails."""
    env = os.environ.copy()
    env.pop('HEART_PORTAL_SUPERVISED', None)
    env.pop('PORTAL_CONNECT_LINK', None)
    link = saved(root, '.portal-connection.url')
    if link:
        env['PORTAL_CONNECT_LINK'] = link
    command = [str(binary_path(root)), '--config', str(root / 'portal.toml')]
    name = saved(root, '.portal-name')
    if name:
        command += ['--name', name]
    directory = root
    if snapshot is not None:
        command = [str(binary_path(root)), *snapshot['arguments']]
        env = snapshot['environment'].copy()
        directory = snapshot['cwd']
    for key in ('HEART_PORTAL_SUPERVISED', 'HEART_PORTAL_MACOS_SUPERVISOR',
                'HEART_PORTAL_READY_FILE', 'HEART_PORTAL_READY_NONCE', 'HEART_PORTAL_UPGRADE_START'):
        env.pop(key, None)
    env['HEART_PORTAL_LOG_FILE'] = str(root / 'portal-runtime.log')
    with open(root / 'portal-runtime.log', 'ab') as out, open(root / 'portal-runtime.err.log', 'ab') as err:
        # Installation still owns the maintenance lock while unwinding a failed
        # bootstrap. Wait outside the manager before exec, otherwise the restored
        # direct Portal can race that lock and reject its own recovery launch.
        waiter = ("import fcntl, os, sys; "
                  "gate = open(sys.argv[1], 'a+b'); "
                  "fcntl.flock(gate, fcntl.LOCK_SH); gate.close(); "
                  "os.execv(sys.argv[2], sys.argv[2:])")
        return subprocess.Popen([sys.executable, '-c', waiter, str(root / '.portal-upgrade.lock'), *command],
                         cwd=directory, env=env, stdin=subprocess.DEVNULL,
                         stdout=out, stderr=err, start_new_session=True)


def install(args, root, path, label, domain, service):
    binary = binary_path(root)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError('Build first: cargo build --release --locked')
    support = Path(__file__).resolve().parent
    if not (support / 'portal-launchagent.sh').is_file():
        raise RuntimeError('Missing scripts/portal-launchagent.sh next to the management script')
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
    # Preserve a saved profile on reinstall; new services share the CLI's user config.
    config_arg = getattr(args, 'config', None)
    if config_arg is None and path.exists():
        assert_owned(path, root, label)
        arguments = plistlib.loads(metadata_bytes(path))['ProgramArguments']
        if len(arguments) == 5:
            config_arg = arguments[4]
    command = [str(binary)]
    if config_arg is not None:
        command += ['--config', str(Path(config_arg).expanduser().resolve())]
    initialized = subprocess.run(command + ['config', 'init'], cwd=root,
                                 capture_output=True, text=True, timeout=15)
    if initialized.returncode:
        raise RuntimeError('Portal config initialization failed; inspect heart-portal config path or supply --config.')
    config = Path(json.loads(initialized.stdout)['config']['path'])
    launchctl('print', domain)  # Requires this user's logged-in GUI session.
    assert_owned(path, root, label)
    running = launchctl('print', service, check=False).returncode == 0
    if running and not path.exists():
        raise RuntimeError('Loaded service has no owned plist; refusing to replace it.')
    updates = {
        root / '.portal-connection.url': (link.strip() + '\n').encode(),
        root / '.portal-name': (name + '\n').encode(),
        root / '.portal-launchagent-label': (label + '\n').encode(),
        root / '.portal-python': os.fsencode(Path(sys.executable).resolve()),
        root / '.portal-executable': os.fsencode(binary.resolve()),
        path: plistlib.dumps(definition(root, label, config)),
    }
    backups = {file: metadata_bytes(file) if file.exists() else None for file in updates}
    manual_pids = checkout_pids(root) if not running else []
    # Reuse the existing launch snapshot for rollback, including an external config.
    # Capture before stopping anything; no duplicate argv or config parser.
    manual_launch = launch_snapshot(process_identity(manual_pids[0])) if manual_pids else None
    if running:
        launchctl('bootout', service)
    bootstrapped = False
    try:
        stop_supervisor(root)
        stop_checkout(root)
        # The existing management entry also supports a downloaded raw binary.
        # Extract no installer: persist only the same lifecycle helper/launcher.
        (root / 'scripts').mkdir(exist_ok=True)
        for name in ('portal-macos.py', 'portal-launchagent.sh'):
            source = support / name
            destination = root / 'scripts' / name
            if source.resolve() != destination.resolve():
                private_write(destination, metadata_bytes(source))
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
        elif manual_launch is not None:
            restore_manual(root, manual_launch)
        raise
    print(f'Installed and started {label} for Portal {name}.')
    print('Starts at user login; launchd restarts Portal after exits. Relay reconnect stays in Portal.')
    print(f'Logs: {root / "portal-runtime.log"}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['install', 'uninstall', 'status'])
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--config', type=Path, help='Existing config or migrated profile; defaults to the user directory.')
    parser.add_argument('--name', help='Reuse the original Portal name on first installation.')
    parser.add_argument('--connect-link', help='Loom URL; prefer saved file, environment or hidden prompt.')
    args = parser.parse_args()
    if sys.platform != 'darwin' or os.getuid() == 0:
        parser.error('Run as the logged-in macOS user, without sudo.')
    root = args.root.resolve(strict=True)
    if args.action == 'install':
        # Complete filesystem preparation before touching any LaunchAgent.
        # The binary rejects an active old installation or conflicting config.
        if not args.name:
            args.name = saved(root, '.portal-name') or None
        if not args.connect_link:
            args.connect_link = os.environ.get('PORTAL_CONNECT_LINK') or saved(root, '.portal-connection.url') or None
        previous_label = label_for(root)
        previous_plist = Path.home() / 'Library/LaunchAgents' / (previous_label + '.plist')
        if args.config is None and previous_plist.exists():
            assert_owned(previous_plist, root, previous_label)
            arguments = plistlib.loads(metadata_bytes(previous_plist))['ProgramArguments']
            if len(arguments) == 5:
                args.config = Path(arguments[4])
        command = [str(binary_path(root))]
        if args.config:
            command += ['--config', str(args.config.expanduser().resolve())]
        installed = subprocess.run(command + ['--install-user-runtime'], cwd=root,
                                   capture_output=True, text=True, timeout=30)
        if installed.returncode:
            raise RuntimeError('Cannot prepare the user Portal installation; stop the legacy Portal before migrating, and check config conflicts.')
        root = Path(json.loads(installed.stdout)['root']).resolve(strict=True)
    else:
        managed = Path.home() / '.heart-portal/runtime'
        if saved(managed, '.portal-origin') == str(root):
            root = managed.resolve(strict=True)
    label = label_for(root)
    path = Path.home() / 'Library/LaunchAgents' / f'{label}.plist'
    domain = f'gui/{os.getuid()}'
    service = f'{domain}/{label}'
    assert_owned(path, root, label)
    if args.action == 'status':
        return manage(args, root, path, label, domain, service)
    with maintenance_lock(root):
        if (root / '.portal-upgrade.json').exists():
            raise RuntimeError('An interrupted upgrade needs worker recovery before maintenance.')
        return manage(args, root, path, label, domain, service)


def manage(args, root, path, label, domain, service):
    if args.action == 'install':
        install(args, root, path, label, domain, service)
    elif args.action == 'uninstall':
        loaded = launchctl('print', service, check=False).returncode == 0
        if loaded and not path.exists():
            raise RuntimeError('Loaded service has no owned plist; refusing to remove it.')
        if loaded:
            launchctl('bootout', service)
        stop_supervisor(root)
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
