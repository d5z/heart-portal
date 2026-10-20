#!/usr/bin/env python3
"""Query a real binary over TCP and a loopback-only relay, in an isolated home."""
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get('PORTAL_TEST_BINARY', str(
    REPO / 'target/debug' / ('heart-portal.exe' if os.name == 'nt' else 'heart-portal')
))).resolve()


def read_exact(connection, count):
    result = b''
    while len(result) < count:
        part = connection.recv(count - len(result))
        if not part:
            raise EOFError('Fixture connection closed')
        result += part
    return result


def send_frame(connection, opcode, payload):
    length = len(payload)
    prefix = bytes([128 | opcode])
    prefix += bytes([length]) if length < 126 else bytes([126]) + struct.pack('!H', length)
    connection.sendall(prefix + payload)


def read_frame(connection):
    first, second = read_exact(connection, 2)
    length = second & 127
    if length == 126:
        length = struct.unpack('!H', read_exact(connection, 2))[0]
    elif length == 127:
        length = struct.unpack('!Q', read_exact(connection, 8))[0]
    if length > 1024 * 1024:
        raise ValueError('Fixture frame too large')
    mask = read_exact(connection, 4) if second & 128 else None
    payload = read_exact(connection, length)
    if mask:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    return first & 15, payload


def read_json_frame(connection):
    while True:
        opcode, payload = read_frame(connection)
        if opcode == 9:
            send_frame(connection, 10, payload)
        elif opcode == 1:
            message = json.loads(payload)
            if message.get('type') == 'keepalive':
                send_frame(connection, 1, b'{"type":"keepalive_ack"}')
            else:
                return message
        elif opcode == 8:
            raise EOFError('Fixture WebSocket closed')


class RuntimeStatusTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='portal-status-test-')
        self.root = Path(temporary.name).resolve()
        assert self.root.parent == Path(tempfile.gettempdir()).resolve()
        assert self.root.name.startswith('portal-status-test-')
        self.addCleanup(temporary.cleanup)
        self.binary = self.root / 'home' / '.heart-portal' / 'runtime' / BINARY.name
        self.binary.parent.mkdir(parents=True)
        shutil.copy2(BINARY, self.binary)
        self.expected_build = 'sha256:' + hashlib.sha256(self.binary.read_bytes()).hexdigest()
        self.config = self.root / 'config' / 'portal.toml'
        self.config.parent.mkdir()
        self.home = self.root / 'home'
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(('PORTAL_', 'HEART_PORTAL_'))}
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home),
                        HEART_PORTAL_SUPERVISED='1', PORTAL_MCP_TOKEN='private-env-token', RUST_LOG='debug')
        self.process = None
        self.log = (self.root / 'runtime.log').open('wb')
        self.addCleanup(self.log.close)
        self.addCleanup(self.stop)

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)

    def start(self, port, *args, extra=''):
        self.config.write_text(
            f"name='configured-name'\nbind='127.0.0.1:{port}'\n"
            "workspace='./workspace'\nkits_dir='./kits'\nkits_enabled=false\n"
            "portal_mcp_token='private-config-token'\n" + extra +
            "[tools]\nexec=false\nfile=false\ncustom_tools_enabled=false\n[security]\nexpose_host_details=true\n", encoding='utf-8')
        self.process = subprocess.Popen([str(self.binary), '--config', str(self.config), *args],
            cwd=self.root, env=self.env, stdout=self.log, stderr=self.log,
            creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)

    def verify_status(self, status, name, mode, state):
        self.assertEqual(status['schema_version'], 1)
        self.assertEqual(status['portal']['name'], name)
        self.assertEqual(status['portal']['pid'], self.process.pid)
        self.assertEqual(status['portal']['build_id'], self.expected_build)
        self.assertEqual(Path(status['portal']['executable']).resolve(), self.binary.resolve())
        self.assertGreater(status['portal']['started_at_unix_secs'], 0)
        self.assertEqual(status['connection']['mode'], mode)
        self.assertEqual(status['connection']['state'], state)
        self.assertEqual(Path(status['config']['path']), self.config)
        self.assertEqual(status['config']['source'], 'explicit')
        self.assertTrue(status['config']['loaded_from_file'])
        self.assertEqual(Path(status['config']['workspace']).resolve(), self.config.parent / 'workspace')
        self.assertEqual(Path(status['config']['kits_directory']).resolve(), self.config.parent / 'kits')
        self.assertEqual(Path(status['config']['user_directory']), self.home / '.heart-portal')
        self.assertFalse(status['tools']['exec'])
        self.assertFalse(status['tools']['file'])
        self.assertEqual(status['kits']['loaded'], 0)
        self.assertTrue(status['security']['mcp_token_configured'])
        for secret in ['private-env-token', 'private-config-token', 'private-relay-token',
                       'must-not-be-used.invalid']:
            self.assertNotIn(secret, json.dumps(status))

    def accept_relay(self, relay, response=b'{"ok":true,"relay_keepalive":"text-v1"}'):
        connection, _ = relay.accept()
        self.addCleanup(connection.close)
        connection.settimeout(10)
        headers = b''
        while not headers.endswith(b'\r\n\r\n'):
            headers += read_exact(connection, 1)
            self.assertLess(len(headers), 16384)
        fields = dict(line.split(':', 1) for line in headers.decode().split('\r\n')[1:] if ':' in line)
        key = next(value.strip() for name, value in fields.items() if name.lower() == 'sec-websocket-key')
        accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        connection.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
        handshake = read_json_frame(connection)
        self.assertEqual(handshake['portal_name'], 'effective-relay-name')
        self.assertEqual(handshake['loom_token'], 'private-relay-token')
        send_frame(connection, 1, response)
        return connection

    def relay_status(self, connection):
        send_frame(connection, 1, json.dumps(dict(jsonrpc='2.0', id=1, method='tools/call',
            params={'name': 'portal_status', 'arguments': {}})).encode())
        reply = read_json_frame(connection)
        self.assertFalse(reply.get('error'), reply.get('error'))
        status = json.loads(reply['result']['content'][0]['text'])
        self.verify_status(status, 'effective-relay-name', 'relay', 'connected')
        self.assertIsNone(status['connection']['listener'])

    def test_relay_reconnects_after_rejection_and_socket_loss_without_restarting(self):
        with socket.socket() as relay:
            relay.bind(('127.0.0.1', 0))
            relay.listen(4)
            relay.settimeout(60)
            self.env['PORTAL_CONNECT_LINK'] = f'http://127.0.0.1:{relay.getsockname()[1]}/status-being?token=private-relay-token'
            self.start(9100, '--name', 'effective-relay-name')
            # An upstream rejection may echo credentials; it must not reach logs.
            with self.accept_relay(relay, b'{"ok":false,"error":"private-relay-token"}'):
                pass
            for _ in range(2):
                with self.accept_relay(relay) as connection:
                    self.relay_status(connection)
                    send_frame(connection, 9, b'nonempty-heartbeat-probe')
                    while True:
                        opcode, payload = read_frame(connection)
                        if opcode == 10:
                            self.assertEqual(payload, b'nonempty-heartbeat-probe')
                            break
                    connection.shutdown(socket.SHUT_RDWR)
            self.assertIsNone(self.process.poll())
            log = (self.root/'runtime.log').read_text(encoding='utf-8', errors='replace')
            self.assertNotIn('private-relay-token', log)

    def test_oversized_relay_frame_is_rejected_without_waiting_for_payload(self):
        with socket.socket() as relay:
            relay.bind(('127.0.0.1', 0))
            relay.listen(2)
            relay.settimeout(60)
            self.env['PORTAL_CONNECT_LINK'] = f'http://127.0.0.1:{relay.getsockname()[1]}/status-being?token=private-relay-token'
            self.start(9100, '--name', 'effective-relay-name')
            with self.accept_relay(relay) as connection:
                # Only the header: reject before allocating or waiting for 16 MiB.
                connection.sendall(b'\x81\x7f' + struct.pack('!Q', 16 * 1024 * 1024 + 1))
                with self.accept_relay(relay) as recovered:
                    self.relay_status(recovered)
                    self.assertIsNone(self.process.poll())

    def test_silent_relay_times_out_and_reconnects_without_restarting(self):
        with socket.socket() as relay:
            relay.bind(('127.0.0.1', 0))
            relay.listen(2)
            relay.settimeout(125)
            self.env['PORTAL_CONNECT_LINK'] = f'http://127.0.0.1:{relay.getsockname()[1]}/status-being?token=private-relay-token'
            self.start(9100, '--name', 'effective-relay-name')
            # Leave TCP open but send no heartbeat replies. Use production's
            # 90-second deadline, without a test-only runtime configuration.
            with self.accept_relay(relay):
                with self.accept_relay(relay) as recovered:
                    self.relay_status(recovered)
                    self.assertIsNone(self.process.poll())

    def test_tcp_reports_running_binary_and_loaded_configuration(self):
        with socket.socket() as reserve:
            reserve.bind(('127.0.0.1', 0))
            port = reserve.getsockname()[1]
        self.start(port)
        deadline = time.monotonic() + 60
        while True:
            try:
                connection = socket.create_connection(('127.0.0.1', port), timeout=2)
                break
            except OSError:
                self.assertIsNone(self.process.poll(), 'Fixture exited before listening')
                self.assertLess(time.monotonic(), deadline, 'Fixture did not start listening')
                time.sleep(0.1)
        with connection, connection.makefile('rb') as reader:
            connection.settimeout(5)
            for index, method, params in [
                (1, 'auth', {'token': 'private-env-token'}),
                (2, 'tools/call', {'name': 'portal_status', 'arguments': {}}),
                (3, 'tools/call', {'name': 'portal_status', 'arguments': {}}),
            ]:
                connection.sendall((json.dumps(dict(jsonrpc='2.0', id=index, method=method, params=params)) + '\n').encode())
                reply = json.loads(reader.readline())
                self.assertFalse(reply.get('error'), reply.get('error'))
                if index > 1:
                    status = json.loads(reply['result']['content'][0]['text'])
                    self.verify_status(status, 'configured-name', 'tcp', 'listening')
                    self.assertEqual(status['connection']['listener']['port'], port)
                    # The second call must still use the in-memory configuration.
                    self.config.write_text('invalid TOML!', encoding='utf-8')
        log = (self.root/'runtime.log').read_text(encoding='utf-8', errors='replace')
        self.assertNotIn('private-env-token', log, 'MCP authentication must not be logged')
        self.assertNotIn('private-config-token', log)

    def test_relay_reports_effective_identity_and_successful_handshake(self):
        with socket.socket() as relay:
            relay.bind(('127.0.0.1', 0))
            relay.listen(1)
            relay.settimeout(60)
            port = relay.getsockname()[1]
            self.env['PORTAL_CONNECT_LINK'] = f'http://127.0.0.1:{port}/status-being?token=private-relay-token'
            self.start(9100, '--name', 'effective-relay-name',
                       extra="connect='https://must-not-be-used.invalid/being?token=unused'\n")
            connection, _ = relay.accept()
            with connection:
                connection.settimeout(10)
                headers = b''
                while not headers.endswith(b'\r\n\r\n'):
                    headers += read_exact(connection, 1)
                    self.assertLess(len(headers), 16384)
                fields = dict(line.split(':', 1) for line in headers.decode().split('\r\n')[1:] if ':' in line)
                key = next(value.strip() for name, value in fields.items() if name.lower() == 'sec-websocket-key')
                accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
                connection.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
                handshake = read_json_frame(connection)
                self.assertEqual(handshake['portal_name'], 'effective-relay-name')
                self.assertEqual(handshake['loom_token'], 'private-relay-token')
                send_frame(connection, 1, b'{"ok":true}')
                send_frame(connection, 1, json.dumps(dict(jsonrpc='2.0', id=1, method='tools/call',
                    params={'name': 'portal_status', 'arguments': {}})).encode())
                reply = read_json_frame(connection)
                self.assertFalse(reply.get('error'), reply.get('error'))
                status = json.loads(reply['result']['content'][0]['text'])
                self.verify_status(status, 'effective-relay-name', 'relay', 'connected')
                self.assertIsNone(status['connection']['listener'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
