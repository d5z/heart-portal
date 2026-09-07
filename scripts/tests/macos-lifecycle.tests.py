#!/usr/bin/env python3
"""macOS lifecycle regression: temporary real LaunchAgent, no real relay."""
import importlib.util
import base64
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('portal_macos', REPO / 'scripts/portal-macos.py')
manager = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manager)


def wait_for(predicate, description, timeout=25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise AssertionError(f'Timed out: {description}')


@unittest.skipUnless(sys.platform == 'darwin', 'macOS only')
class LifecycleTests(unittest.TestCase):
    def test_binary_metadata_is_preserved_by_management_commands(self):
        with tempfile.TemporaryDirectory(prefix='portal signature test ') as temporary:
            root = Path(temporary)
            binary = root / 'heart-portal'
            shutil.copy2(REPO / 'target/release/heart-portal', binary)
            config = root / 'portal.toml'
            config.write_text('kits_enabled = false\n')
            subprocess.run(['/usr/bin/xattr', '-w', 'com.beings.portal-test', 'preserve', str(binary)],
                           check=True)
            before = hashlib.sha256(binary.read_bytes()).digest()
            subprocess.run([str(binary), '--config', str(config), 'kit', 'status'],
                           check=True, capture_output=True, timeout=10)
            self.assertEqual(hashlib.sha256(binary.read_bytes()).digest(), before,
                             'config checks must not rewrite the executable or its signing identity')
            attribute = subprocess.run(['/usr/bin/xattr', '-p', 'com.beings.portal-test', str(binary)],
                                       capture_output=True, text=True)
            self.assertEqual(attribute.returncode, 0, 'config checks must preserve file metadata')
            self.assertEqual(attribute.stdout.strip(), 'preserve')

    def test_missing_explicit_config_does_not_start_with_defaults(self):
        with tempfile.TemporaryDirectory(prefix='portal missing config ') as temporary:
            root = Path(temporary)
            result = subprocess.run([str(REPO / 'target/release/heart-portal'),
                                     '--config', str(root / 'missing.toml')],
                                    cwd=root, capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('Config file not found', result.stderr)
            self.assertFalse(list(root.iterdir()))

    def test_real_portal_relay_restart(self):
        binary = REPO / 'target/release/heart-portal'
        self.assertTrue(binary.exists(), 'Run cargo build --release --locked first')
        with tempfile.TemporaryDirectory(prefix='portal relay test ') as temporary, socket.socket() as relay:
            root = Path(temporary).resolve()
            (root / 'scripts').mkdir()
            (root / 'target/release').mkdir(parents=True)
            shutil.copy2(REPO / 'scripts/portal-launchagent.sh', root / 'scripts')
            shutil.copy2(binary, root / 'target/release/heart-portal')
            (root / 'portal.toml').write_text('name = "fixture"\nworkspace = "./workspace"\nbind = "127.0.0.1:0"\nkits_enabled = false\n[cowork]\nhttp_port = 0\n')
            relay.bind(('127.0.0.1', 0))
            relay.listen()
            relay.settimeout(25)
            link = f'http://127.0.0.1:{relay.getsockname()[1]}/fixture/?token=fake-token'
            service = f'gui/{os.getuid()}/{manager.label_for(root)}'
            plist = Path.home() / 'Library/LaunchAgents' / f'{manager.label_for(root)}.plist'
            installer = [sys.executable, str(REPO / 'scripts/portal-macos.py')]

            def accept():
                client, _ = relay.accept()
                client.settimeout(10)
                stream = client.makefile('rwb', buffering=0)
                headers = {}
                stream.readline()
                while True:
                    line = stream.readline().decode().strip()
                    if not line:
                        break
                    key, value = line.split(':', 1)
                    headers[key.lower()] = value.strip()
                digest = hashlib.sha1((headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()
                stream.write(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + base64.b64encode(digest) + b'\r\n\r\n')
                return client, stream

            def read_exact(stream, length):
                data = b''
                while len(data) < length:
                    chunk = stream.read(length - len(data))
                    if not chunk:
                        raise AssertionError('WebSocket closed before restart response')
                    data += chunk
                return data

            def receive(stream):
                header = read_exact(stream, 2)
                length = header[1] & 127
                if length == 126:
                    length = struct.unpack('!H', read_exact(stream, 2))[0]
                elif length == 127:
                    length = struct.unpack('!Q', read_exact(stream, 8))[0]
                mask = read_exact(stream, 4) if header[1] & 128 else b''
                payload = read_exact(stream, length)
                if mask:
                    payload = bytes(value ^ mask[i % 4] for i, value in enumerate(payload))
                value = json.loads(payload)
                if value.get('type') == 'keepalive':
                    send(stream, {'type': 'keepalive_ack'})
                    return receive(stream)
                return value

            def send(stream, value):
                payload = json.dumps(value, separators=(',', ':')).encode()
                header = bytes([0x81, len(payload)]) if len(payload) < 126 else b'\x81\x7e' + struct.pack('!H', len(payload))
                stream.write(header + payload)

            try:
                subprocess.run(installer + ['install', '--root', str(root), '--name', 'stable-name', '--connect-link', link], check=True, capture_output=True)
                client, stream = accept()
                with client, stream:
                    first_handshake = receive(stream)
                    send(stream, {'ok': True, 'relay_keepalive': 'text-v1'})
                    send(stream, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list', 'params': {}})
                    self.assertIn('portal_restart', [tool['name'] for tool in receive(stream)['result']['tools']])
                    pid = manager.checkout_pids(root)[0]
                    # A rotated token cannot create a competing process for this identity.
                    env = dict(os.environ, PORTAL_CONNECT_LINK=link.replace('fake-token', 'rotated-token'))
                    duplicate = subprocess.run([str(root / 'target/release/heart-portal'), '--config', str(root / 'portal.toml')], env=env, capture_output=True, timeout=10)
                    self.assertNotEqual(duplicate.returncode, 0)
                    self.assertIn(b'another Portal instance', duplicate.stderr)
                # A dropped relay connection recovers within this same process;
                # the supervisor is only responsible for process exits.
                client, stream = accept()
                with client, stream:
                    self.assertEqual(receive(stream), first_handshake)
                    send(stream, {'ok': True, 'relay_keepalive': 'text-v1'})
                    self.assertEqual(manager.checkout_pids(root), [pid])
                    send(stream, {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {'name': 'portal_restart', 'arguments': {}}})
                    reply = receive(stream)
                    self.assertEqual(reply['id'], 2)
                    self.assertIn('restart scheduled', reply['result']['content'][0]['text'])
                client, stream = accept()
                with client, stream:
                    self.assertEqual(receive(stream), first_handshake, 'restart preserves relay identity')
                    send(stream, {'ok': True})
                    self.assertNotEqual(manager.checkout_pids(root)[0], pid)
                    subprocess.run(installer + ['uninstall', '--root', str(root)], check=True, capture_output=True)
                self.assertIn('termination signal', (root / 'portal-runtime.log').read_text())
                self.assertFalse(manager.checkout_pids(root))
            finally:
                manager.launchctl('bootout', service, check=False)
                manager.stop_checkout(root)
                plist.unlink(missing_ok=True)

    def test_validation_and_ownership(self):
        for link in ('', 'https://relay.invalid/being/', 'file:///being/?token=x',
                     'https://relay.invalid:bad/being/?token=x'):
            with self.assertRaises(ValueError):
                manager.validate_link(link)
        self.assertEqual(manager.validate_link('https://relay.invalid/a/?token=x'), 'a')
        with tempfile.TemporaryDirectory(prefix='portal unit ') as temporary:
            root = Path(temporary)
            label = manager.label_for(root)
            path = root / 'agent.plist'
            definition = manager.definition(root, label)
            path.write_bytes(plistlib.dumps(definition))
            manager.assert_owned(path, root, label)
            definition['ProgramArguments'][1] += '.other'
            path.write_bytes(plistlib.dumps(definition))
            with self.assertRaises(RuntimeError):
                manager.assert_owned(path, root, label)
            manager.private_write(root / 'secret', b'fake-token')
            self.assertEqual((root / 'secret').stat().st_mode & 0o777, 0o600)

    def test_launchd_recovery_and_lifecycle(self):
        with tempfile.TemporaryDirectory(prefix='portal mac test 空 & ') as temporary:
            root = Path(temporary).resolve()
            (root / 'scripts').mkdir()
            (root / 'target/release').mkdir(parents=True)
            shutil.copy2(REPO / 'scripts/portal-launchagent.sh', root / 'scripts')
            shutil.copy2(REPO / 'portal.example.toml', root)
            # A compiled fixture gives proc_pidpath the same ownership semantics
            # as the production binary. It creates a descendant holding logs.
            source = root / 'fixture.c'
            source.write_text(r'''
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
int main(int argc, char **argv) {
    if (argc > 3 && !strcmp(argv[3], "kit")) return 0;
    FILE *f = fopen("launches.txt", "a");
    fprintf(f, "%d|%s|%s|%s\n", getpid(), argv[argc-1],
            getenv("HEART_PORTAL_SUPERVISED"), getenv("PORTAL_CONNECT_LINK"));
    fclose(f);
    pid_t child = fork();
    if (!child) { puts("child holding inherited stdout"); fflush(stdout); execl("/bin/sleep", "sleep", "120", NULL); return 1; }
    f = fopen("children.txt", "a"); fprintf(f, "%d\n", child); fclose(f);
    for (;;) {
        if (access("exit-success", F_OK) == 0) { unlink("exit-success"); return 0; }
        usleep(100000);
    }
}
''')
            subprocess.run(['cc', str(source), '-o', str(root / 'target/release/heart-portal')], check=True)
            label = manager.label_for(root)
            path = Path.home() / 'Library/LaunchAgents' / f'{label}.plist'
            domain = f'gui/{os.getuid()}'
            service = f'{domain}/{label}'
            link = 'https://relay.invalid/test-being/?token=fake-test-token'
            installer = [sys.executable, str(REPO / 'scripts/portal-macos.py')]

            def run(action, *extra):
                return subprocess.run(installer + [action, '--root', str(root), *extra],
                                      check=True, text=True, capture_output=True)

            def launches():
                file = root / 'launches.txt'
                return file.read_text().splitlines() if file.exists() else []

            def next_launch(count):
                rows = wait_for(lambda: launches() if len(launches()) > count else None,
                                'Portal relaunch')
                return int(rows[-1].split('|')[0])

            try:
                run('install', '--connect-link', link, '--name', 'original-name')
                pid = next_launch(0)
                child_pid = int(wait_for(
                    lambda: (root / 'children.txt').read_text().strip() if (root / 'children.txt').exists() else '',
                    'fixture child start').splitlines()[0])
                definition = plistlib.loads(path.read_bytes())
                self.assertNotIn('fake-test-token', path.read_text())
                self.assertTrue(definition['KeepAlive'])
                self.assertEqual(len(manager.checkout_pids(root)), 1)
                # SIGKILL recovery, while a child holds inherited stdout/stderr.
                os.kill(pid, signal.SIGKILL)
                recovered = next_launch(1)
                self.assertNotEqual(pid, recovered)
                wait_for(lambda: manager.executable_path(child_pid) is None,
                         'launchd cleans child after Portal crash')
                self.assertEqual(len(manager.checkout_pids(root)), 1)
                self.assertTrue((root / 'portal-runtime.log.previous').exists())
                # A zero exit code (portal_restart's contract) also relaunches.
                count = len(launches())
                (root / 'exit-success').touch()
                next_launch(count)
                for line in launches():
                    self.assertIn('|original-name|1|' + link, line)
                # Reinstallation rotates credentials without changing identity/config.
                config = (root / 'portal.toml').read_bytes()
                count = len(launches())
                run('install', '--connect-link', link.replace('fake-test-token', 'rotated-test-token'))
                next_launch(count)
                self.assertEqual((root / 'portal.toml').read_bytes(), config)
                self.assertEqual(manager.saved(root, '.portal-name'), 'original-name')
                self.assertEqual(len(manager.checkout_pids(root)), 1)
                # Failed bootstrap restores saved credentials/config and old agent.
                args = type('Args', (), {'name': 'must-not-persist', 'connect_link': link})()
                real_launchctl = manager.launchctl
                failed = False

                def fail_once(*args, **kwargs):
                    nonlocal failed
                    if args[0] == 'bootstrap' and not failed:
                        failed = True
                        raise RuntimeError('Simulated registration failure')
                    return real_launchctl(*args, **kwargs)

                count = len(launches())
                with patch.object(manager, 'launchctl', side_effect=fail_once):
                    with self.assertRaisesRegex(RuntimeError, 'Simulated'):
                        manager.install(args, root, path, label, domain, service)
                next_launch(count)
                self.assertEqual(manager.saved(root, '.portal-name'), 'original-name')
                self.assertIn('rotated-test-token', manager.saved(root, '.portal-connection.url'))
                run('status')
                run('uninstall')
                count = len(launches())
                time.sleep(6)
                self.assertEqual(len(launches()), count, 'uninstall must prevent recovery')
                self.assertFalse(manager.checkout_pids(root))
                for child in (root / 'children.txt').read_text().splitlines():
                    wait_for(lambda: manager.executable_path(int(child)) is None,
                             'launchd cleans children on uninstall')
                self.assertFalse(path.exists())
                self.assertTrue((root / '.portal-connection.url').exists())
                run('uninstall')  # Idempotent removal.
            finally:
                manager.launchctl('bootout', service, check=False)
                manager.stop_checkout(root)
                path.unlink(missing_ok=True)


if __name__ == '__main__':
    unittest.main(verbosity=2)
