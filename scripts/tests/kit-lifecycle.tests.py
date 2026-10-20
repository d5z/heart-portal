#!/usr/bin/env python3
"""Real Portal/stdio fixtures on Windows, macOS and Linux; loopback only."""
import itertools
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get('PORTAL_TEST_BINARY', str(
    REPO / 'target/debug' / ('heart-portal.exe' if os.name == 'nt' else 'heart-portal')
))).resolve()

FIXTURE = '''import json, os, sys, time
from pathlib import Path
marker = Path("code.txt").read_text()
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message: continue
    if message["method"] == "initialize":
        result = {"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
    elif message["method"] == "tools/call":
        arguments = message["params"].get("arguments", {})
        if arguments.get("wait"):
            Path("call-started").write_text("yes")
            time.sleep(2)
        result = {"content":[{"type":"text","text":json.dumps({"pid":os.getpid(),"marker":marker,"configured":os.environ.get("PORTAL_FIXTURE_TOKEN")=="fixture-credential"})}]}
        if arguments.get("fail"):
            result = {"isError":True,"content":[{"type":"text","text":"private-tool-error-value"}]}
    else:
        result = {"tools":[]}
    print(json.dumps({"jsonrpc":"2.0","id":message["id"],"result":result}), flush=True)
'''


class KitLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='portal-kit-lifecycle-')
        self.root = Path(self.temporary.name).resolve()
        assert self.root.parent == Path(tempfile.gettempdir()).resolve()
        assert self.root.name.startswith('portal-kit-lifecycle-')
        self.addCleanup(self.temporary.cleanup)
        self.binary = self.root / 'home' / '.heart-portal' / 'runtime' / BINARY.name
        self.binary.parent.mkdir(parents=True)
        shutil.copy2(BINARY, self.binary)
        self.kits = self.root / 'kits'
        self.kits.mkdir()
        with socket.socket() as reserve:
            reserve.bind(('127.0.0.1', 0))
            self.port = reserve.getsockname()[1]
        config = self.root / 'portal.toml'
        config.write_text(f"name='fixture'\nbind='127.0.0.1:{self.port}'\nworkspace='./workspace'\nkits_dir='./kits'\nkits_enabled=true\n[tools]\nexec=false\nfile=false\n[custom_tools]\nconfig_path='private-ignored-path'\ntoken='private-ignored-token'\n", encoding='utf-8')
        if getattr(self, 'expose_host_details', True):
            with config.open('a') as stream:
                stream.write('[security]\nexpose_host_details=true\n')
        env = {key:value for key,value in os.environ.items() if not key.startswith(('PORTAL_', 'HEART_PORTAL_'))}
        env.update(HOME=str(self.root/'home'), USERPROFILE=str(self.root/'home'),
                   HEART_PORTAL_SUPERVISED='1', RUST_LOG='debug')
        env.update(getattr(self, 'runtime_env', {}))
        initial_custom = getattr(self, 'initial_custom_script', None)
        if initial_custom:
            custom = self.root/'workspace'/'tools'
            custom.mkdir(parents=True, exist_ok=True)
            (custom/'server.py').write_text(initial_custom,encoding='utf-8')
            (custom/'mcp.toml').write_text('[[servers]]\nname="startup"\ncommand='+json.dumps([sys.executable,str(custom/'server.py')])+'\n',encoding='utf-8')
        self.log = (self.root/'runtime.log').open('wb')
        self.addCleanup(self.log.close)
        self.connection = self.reader = None
        self.sequence = itertools.count(1)
        self.notifications = []
        self.started_at = time.monotonic()
        self.process = subprocess.Popen([str(self.binary), '--config', str(config)], cwd=self.root,
            env=env, stdout=self.log, stderr=self.log,
            creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
        self.addCleanup(self.stop)
        deadline = time.monotonic() + 60
        while True:
            try:
                self.connection = socket.create_connection(('127.0.0.1', self.port), timeout=2)
                break
            except OSError:
                self.assertIsNone(self.process.poll(), 'Portal exited before listening')
                self.assertLess(time.monotonic(), deadline, 'Portal did not listen')
                time.sleep(0.1)
        self.connection.settimeout(15)
        self.reader = self.connection.makefile('rb')
        self.rpc('initialize', {})

    def stop(self):
        try:
            if self.process.poll() is None and self.connection:
                self.rpc('tools/call', {'name':'portal_restart','arguments':{}})
                self.process.wait(timeout=15)
        except (OSError, AssertionError, subprocess.TimeoutExpired):
            pass
        finally:
            if self.process.poll() is None:
                if os.name == 'nt':
                    subprocess.run(['taskkill','/PID',str(self.process.pid),'/T','/F'],
                        capture_output=True, timeout=10, creationflags=subprocess.CREATE_NO_WINDOW)
                else:
                    self.process.terminate()
                self.process.wait(timeout=10)
            if self.reader: self.reader.close()
            if self.connection: self.connection.close()

    def rpc(self, method, params, raw=False):
        sequence = next(self.sequence)
        self.connection.sendall((json.dumps(dict(jsonrpc='2.0',id=sequence,method=method,params=params))+'\n').encode())
        while True:
            line = self.reader.readline()
            self.assertTrue(line, 'Portal connection closed')
            reply = json.loads(line)
            if 'id' not in reply:
                self.notifications.append(reply.get('method'))
                continue
            self.assertEqual(reply['id'], sequence)
            if raw:
                return reply
            self.assertFalse(reply.get('error'), reply.get('error'))
            return reply['result']

    def call(self, name, args=None):
        return self.rpc('tools/call', {'name':name,'arguments':args or {}})

    def value(self, name, args=None):
        result = self.call(name,args)
        self.assertFalse(result.get('isError'))
        return json.loads(result['content'][0]['text'])

    def install(self, name='sample'):
        directory = self.kits/name
        directory.mkdir()
        (directory/'fixture.py').write_text(FIXTURE, encoding='utf-8')
        (directory/'code.txt').write_text('one', encoding='utf-8')
        (directory/'.env').write_text('PORTAL_FIXTURE_TOKEN=fixture-credential\n', encoding='utf-8')
        manifest = {'name':name,'version':'1','command':[sys.executable,'fixture.py'],'eager':True,
            'tools':[{'name':'ping','description':'fixture','params':{'type':'object'}}],
            'provision':{'env':[{'name':'PORTAL_FIXTURE_TOKEN','required':True}]}}
        (directory/'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
        return directory

    def test_invalid_manifest_cannot_transfer_running_kit_to_another_directory(self):
        original = self.install()
        self.value('portal_kits_reload')
        first = self.value('sample_ping')
        generation = self.status()['diagnostics']['generation']
        (original/'manifest.json').write_text('{', encoding='utf-8')
        replacement = self.install('replacement')
        (replacement/'code.txt').write_text('replacement', encoding='utf-8')
        manifest = json.loads((replacement/'manifest.json').read_text(encoding='utf-8'))
        manifest['name'] = 'sample'
        (replacement/'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
        for arguments in [{}, {'kit':'sample'}]:
            report = self.call('portal_kits_reload', arguments)['structuredContent']['kits']
            self.assertEqual(report['reloaded'], [])
            self.assertIn('sample', report['retained_invalid'])
            self.assertEqual(self.value('sample_ping'), first)
            self.assertEqual(self.status()['diagnostics']['generation'], generation)

    def test_kit_path_launches_local_shim_and_preserves_other_kit(self):
        import shlex
        directory = self.install()
        self.install('other')
        self.value('portal_kits_reload')
        other_pid = self.value('other_ping')['pid']
        bin_dir = directory/'private bin'
        bin_dir.mkdir()
        if os.name == 'nt':
            launcher = bin_dir/'portal-fixture-runtime.cmd'
            launcher.write_text('@echo off\n"'+sys.executable+'" "%~dp0..\\fixture.py"\n', encoding='utf-8')
        else:
            launcher = bin_dir/'portal-fixture-runtime'
            launcher.write_text('#!/bin/sh\nexec '+shlex.quote(sys.executable)+' '+shlex.quote(str(directory/'fixture.py'))+'\n', encoding='utf-8')
            launcher.chmod(0o700)
        with (directory/'.env').open('a', encoding='utf-8') as env:
            env.write("PATH='"+str(bin_dir)+"'\nPATHEXT=.CMD\n")
        manifest = json.loads((directory/'manifest.json').read_text(encoding='utf-8'))
        manifest['command'] = ['portal-fixture-runtime']
        (directory/'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
        self.value('portal_kits_reload', {'kit':'sample'})
        self.assertTrue(self.value('sample_ping')['configured'])
        self.assertEqual(self.value('other_ping')['pid'], other_pid)
        # Replacing PATH removes the runtime; do not use the old process or host PATH.
        (directory/'.env').write_text('PORTAL_FIXTURE_TOKEN=fixture-credential\nPATH=\nPATHEXT=.CMD\n', encoding='utf-8')
        self.value('portal_kits_reload', {'kit':'sample'})
        self.assertEqual(self.status()['status'], 'unhealthy')
        self.assertTrue(self.call('sample_ping')['isError'])
        self.assertEqual(self.value('other_ping')['pid'], other_pid)

    def test_tools_reload_does_not_reload_running_kit(self):
        directory = self.install()
        self.value('portal_kits_reload')
        first = self.value('sample_ping')
        self.assertTrue(first['configured'])
        (directory/'code.txt').write_text('two', encoding='utf-8')
        result = self.call('portal_tools_reload')
        self.assertFalse(result.get('isError'))
        second = self.value('sample_ping')
        self.assertEqual(second['marker'], 'one')
        self.assertEqual(first['pid'], second['pid'])
        self.assertEqual(self.value('portal_status')['portal']['pid'], self.process.pid)
        self.assertIn('No custom tools found', result['content'][0]['text'])
        self.value('portal_kits_reload', {'kit':'sample'})
        third = self.value('sample_ping')
        self.assertEqual(third['marker'], 'two')
        self.assertNotEqual(second['pid'], third['pid'])

    def status(self, name='sample'):
        return next(kit for kit in self.value('portal_status')['kits']['items'] if kit['name']==name)

    def until(self, check):
        deadline = time.monotonic() + 12
        while True:
            result = check()
            if result: return result
            self.assertLess(time.monotonic(), deadline, 'Automatic refresh did not converge')
            time.sleep(0.1)

    def test_automatic_install_credentials_removal_and_list_notification(self):
        directory = self.install()
        self.until(lambda: self.value('portal_status')['kits']['loaded']==1)
        kit = self.status()
        self.assertEqual(kit['status'], 'not-started')
        self.assertEqual(kit['next_action'], 'call-tool')
        self.assertIsNone(kit['process_id'])
        first = self.value('sample_ping')
        self.assertTrue(first['configured'], 'Portal must inject .env without a kit dotenv reader')
        (directory/'.env').write_text('', encoding='utf-8')
        self.until(lambda: self.status()['status']=='needs-configuration')
        self.assertEqual(self.status()['next_action'], 'configure-kit')
        self.assertNotIn('sample_ping', [tool['name'] for tool in self.rpc('tools/list', {})['tools']])
        (directory/'.env').write_text('PORTAL_FIXTURE_TOKEN=fixture-credential\n', encoding='utf-8')
        self.until(lambda: self.status()['status']=='not-started')
        self.assertTrue(self.value('sample_ping')['configured'])
        # Windows locks the child process working directory. Release the idle
        # child before renaming/removing its directory; no Portal restart.
        self.value('portal_kits_reload', {'kit':'sample'})
        directory.rename(self.root/'uninstalled')
        self.until(lambda: self.value('portal_status')['kits']['loaded']==0)
        self.assertIn('notifications/tools/list_changed', self.notifications)
        self.assertEqual(self.value('portal_status')['portal']['pid'], self.process.pid)

    def test_misspelled_reload_target_does_not_reload_any_kit(self):
        self.install()
        self.value('portal_kits_reload')
        pid = self.value('sample_ping')['pid']
        before = self.value('portal_kits_status')
        for arguments in ({'name': 'sample'}, {'kit': 'sample', 'force': True}):
            reply = self.call('portal_kits_reload', arguments)
            self.assertTrue(reply.get('isError'), reply)
            self.assertEqual(self.value('portal_kits_status'), before)
        for arguments in ([], None, 'sample'):
            reply = self.rpc('tools/call', {'name': 'portal_kits_reload', 'arguments': arguments}, raw=True)
            self.assertEqual(reply['error']['code'], -32602)
            self.assertEqual(self.value('portal_kits_status'), before)
        self.assertEqual(self.value('sample_ping')['pid'], pid)

    def test_targeted_reload_preserves_other_process_and_invalid_manifest(self):
        directory = self.install()
        self.install('other')
        self.value('portal_kits_reload')
        original = self.value('sample_ping')
        other = self.value('other_ping')
        generation = self.status()['diagnostics']['generation']
        (directory/'code.txt').write_text('two', encoding='utf-8')
        self.assertEqual(self.value('sample_ping')['marker'], 'one')
        result = self.call('portal_kits_reload', {'kit':'sample'})
        self.assertEqual(result['structuredContent']['kits']['reloaded'], ['sample'])
        self.assertEqual(self.status()['status'], 'not-started')
        self.assertNotEqual(self.status()['diagnostics']['generation'], generation)
        replacement = self.value('sample_ping')
        self.assertNotEqual(replacement['pid'], original['pid'])
        self.assertEqual(replacement['marker'], 'two')
        self.assertEqual(self.value('other_ping')['pid'], other['pid'])
        manifest = directory/'manifest.json'
        valid = manifest.read_text(encoding='utf-8')
        manifest.write_text('{"partial":', encoding='utf-8')
        result = self.call('portal_kits_reload', {'kit':'other'})
        self.assertEqual(result['structuredContent']['kits']['retained_invalid'], ['sample'])
        self.assertEqual(self.value('sample_ping')['pid'], replacement['pid'])
        manifest.write_text(valid, encoding='utf-8')
        self.assertEqual(self.value('sample_ping')['pid'], replacement['pid'])

    def test_diagnostics_distinguish_tool_error_and_mcp_without_secret_values(self):
        directory = self.install()
        self.value('portal_kits_reload')
        first = self.value('sample_ping')
        self.assertTrue(self.call('sample_ping', {'fail':True})['isError'])
        status = self.value('portal_status')
        kit = status['kits']['items'][0]
        self.assertEqual(kit['status'], 'healthy')
        self.assertEqual(kit['process_id'], first['pid'])
        self.assertEqual(kit['diagnostics']['last_call']['outcome'], 'tool-error')
        self.assertIsNone(kit['diagnostics']['last_lifecycle_error'])
        self.assertEqual(kit['next_action'], 'inspect-tool-result')
        self.assertEqual(kit['service_authorization'], 'not-verified-by-portal')
        self.assertGreater(kit['diagnostics']['last_started_at_unix_ms'], 0)
        self.assertIn('unsupported-custom-tools-section', [w['code'] for w in status['config']['warnings']])
        self.assertEqual(status['capabilities']['kit_reload']['activation'], 'next-tool-call')
        self.assertFalse(status['capabilities']['custom_tools_config_path_override'])
        for secret in ['fixture-credential','private-tool-error-value','private-ignored-path','private-ignored-token']:
            self.assertNotIn(secret, json.dumps(status))
        self.value('sample_ping')
        self.assertEqual(self.status()['diagnostics']['last_call']['outcome'], 'success')
        # A server that exits during initialize is a transport/start failure,
        # distinct from a valid MCP isError tool response above.
        (directory/'fixture.py').write_text('raise SystemExit(1)\n', encoding='utf-8')
        self.value('portal_kits_reload', {'kit':'sample'})
        failed = self.call('sample_ping')
        self.assertTrue(failed['isError'])
        kit = self.status()
        self.assertEqual(kit['diagnostics']['last_lifecycle_error'], 'mcp-start-failed')
        self.assertIsNone(kit['process_id'])
        self.assertEqual(kit['next_action'], 'check-runtime-and-mcp')
        log = (self.root/'runtime.log').read_text(encoding='utf-8', errors='replace')
        for secret in ['fixture-credential','private-tool-error-value','private-ignored-path','private-ignored-token']:
            self.assertNotIn(secret, log)

    def test_inflight_call_finishes_without_overwriting_replacement_diagnostics(self):
        import concurrent.futures
        directory = self.install()
        self.value('portal_kits_reload')
        original = self.value('sample_ping')
        def old_call():
            with socket.create_connection(('127.0.0.1', self.port), timeout=10) as connection:
                with connection.makefile('rb') as reader:
                    for seq, method, params in [(1,'initialize',{}), (2,'tools/call',{'name':'sample_ping','arguments':{'wait':True,'fail':True}})]:
                        connection.sendall((json.dumps(dict(jsonrpc='2.0',id=seq,method=method,params=params))+'\n').encode())
                        while True:
                            reply = json.loads(reader.readline())
                            if reply.get('id')==seq: break
                    return reply['result']
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            old = executor.submit(old_call)
            self.until(lambda: (directory/'call-started').exists())
            self.value('portal_kits_reload', {'kit':'sample'})
            replacement = self.value('sample_ping')
            self.assertNotEqual(original['pid'], replacement['pid'])
            self.assertTrue(old.result(timeout=10)['isError'])
            current = self.status()
            self.assertEqual(current['process_id'], replacement['pid'])
            self.assertEqual(current['diagnostics']['last_call']['outcome'], 'success')


if __name__ == '__main__':
    unittest.main(verbosity=2)
