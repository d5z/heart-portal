#!/usr/bin/env python3
"""Real foreground adoption, crash recovery, maintenance exclusion and explicit stop."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('e2e', Path(__file__).with_name('macos-upgrade-e2e.py'))
e2e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e2e)
manager = e2e.manager
BINARY = Path(os.environ.get('PORTAL_TEST_BINARY', REPO / 'target/debug/heart-portal')).resolve()


@unittest.skipUnless(sys.platform == 'darwin', 'requires macOS')
class SupervisorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='portal auto guardian 中文 ')
        profile = Path(self.temp.name).resolve()
        self.home_patch = patch.dict(os.environ, HOME=str(profile))
        self.home_patch.start()
        self.addCleanup(self.home_patch.stop)
        self.root = profile / '.heart-portal/runtime'
        self.root.mkdir(parents=True)
        self.target = self.root / 'heart-portal'
        shutil.copy2(BINARY, self.target)
        self.config = self.root / 'custom-config.toml'
        self.config.write_text('workspace = "./custom-workspace"\nkits_enabled = false\nbind = "127.0.0.1:0"\n'
                               '[subagent]\nenabled=false\n[tools]\nexec=false\n[security]\npermission_mode="read-only"\n')
        self.relay = e2e.Relay()
        self.link = f'http://127.0.0.1:{self.relay.port}/supervised/?token=private-fixture'
        self.log = open(self.root / 'foreground.log', 'ab')
        self.process = subprocess.Popen([str(self.target), '--config', str(self.config), '--connect', self.link, '--name', 'kept-name',
                                         '--permission-mode', 'full-access', '--exec-enabled', 'true', '--kits-enabled', 'false'],
                                        cwd=self.root, stdout=self.log, stderr=self.log)
        self.hello = self.relay.connect()
        self.before = self.relay.tool('portal_permissions')['status']

    def tearDown(self):
        manager.stop_supervisor(self.root)
        manager.stop_checkout(self.root)
        self.process.wait(timeout=15)
        self.relay.close()
        self.log.close()
        self.temp.cleanup()

    def test_adopts_original_pid_and_recovers_without_permission_owner_switch(self):
        self.assertEqual(self.before['pid'], self.process.pid)
        before = json.loads(self.relay.tool('portal_status')['content'][0]['text'])
        self.assertEqual(before['authorization']['permission_mode'], 'FullAccess')
        self.assertTrue(before['tools']['exec'])
        state = manager.supervisor_state(self.root)
        self.assertTrue(state)
        self.assertEqual(state['runtime']['pid'], self.process.pid)
        status = subprocess.run([str(self.target), 'status'], capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(status.stdout)['portal_pids'], [self.process.pid])
        self.assertNotIn('private-fixture', status.stdout)
        self.assertFalse(json.loads(status.stdout)['launchagent_loaded'])
        self.assertEqual(sorted(p.name for p in (self.root / '.portal-supervisor').iterdir()),
                         ['portal-macos-supervisor.py', 'portal-macos.py'])
        os.kill(self.process.pid, signal.SIGKILL)
        self.process.wait(timeout=10)
        self.assertEqual(self.relay.connect(), self.hello)
        after = self.relay.tool('portal_permissions')['status']
        after_status = json.loads(self.relay.tool('portal_status')['content'][0]['text'])
        self.assertEqual(after_status['authorization']['permission_mode'], 'FullAccess')
        self.assertTrue(after_status['tools']['exec'])
        self.assertNotEqual(after['pid'], self.before['pid'])
        self.assertEqual(after['permissions'], self.before['permissions'])
        self.relay.tool('portal_restart')
        self.assertEqual(self.relay.connect(), self.hello)
        restarted = self.relay.tool('portal_permissions')['status']
        self.assertEqual(json.loads(self.relay.tool('portal_status')['content'][0]['text'])['authorization']['permission_mode'], 'FullAccess')
        self.assertNotEqual(restarted['pid'], after['pid'])
        self.assertEqual(restarted['permissions'], self.before['permissions'])
        self.assertEqual(manager.checkout_pids(self.root), [restarted['pid']])

    def test_maintenance_blocks_respawn_and_stop_does_not_kill_its_caller(self):
        with manager.maintenance_lock(self.root):
            self.process.kill()
            self.process.wait(timeout=10)
            time.sleep(1)
            self.assertFalse(manager.checkout_pids(self.root))
            self.assertTrue(manager.supervisor_state(self.root))
            stopped = subprocess.run([str(self.target), 'stop'], capture_output=True, text=True)
            self.assertNotEqual(stopped.returncode, 0)
            self.assertIn('maintenance/upgrade is in progress', stopped.stderr)
        self.assertEqual(self.relay.connect(), self.hello)
        self.relay.tool('portal_permissions')
        stopped = subprocess.run([str(self.target), 'stop'], capture_output=True, text=True, timeout=35)
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertIn('supervisor stopped', stopped.stdout)
        time.sleep(.5)
        self.assertFalse(manager.checkout_pids(self.root))
        self.assertFalse(manager.supervisor_state(self.root))
        self.assertTrue(self.config.exists())

    def test_ctrl_c_stops_supervision_and_changed_identity_cannot_duplicate_installation(self):
        duplicate = subprocess.run([str(self.target), '--config', str(self.config), '--connect',
                                    self.link.replace('/supervised/', '/different/')],
                                   cwd=self.root, capture_output=True, text=True, timeout=15)
        self.assertNotEqual(duplicate.returncode, 0)
        self.assertIn('supervisor is already running', duplicate.stderr)
        self.assertEqual(manager.checkout_pids(self.root), [self.process.pid])
        self.process.send_signal(signal.SIGINT)
        self.process.wait(timeout=15)
        e2e.wait_for(lambda: not manager.supervisor_state(self.root), timeout=15)
        time.sleep(.5)
        self.assertFalse(manager.checkout_pids(self.root))


@unittest.skipUnless(sys.platform == 'darwin', 'requires macOS')
class SlowKitTests(unittest.TestCase):
    def test_slow_eager_kits_do_not_delay_upgrade_readiness(self):
        with tempfile.TemporaryDirectory(prefix='portal slow kits ') as temporary:
            root = Path(temporary).resolve()
            target = root / 'heart-portal'
            shutil.copy2(BINARY, target)
            kits = root / 'kits'
            for n in range(4):
                kit = kits / f'slow{n}'
                kit.mkdir(parents=True)
                script = kit / 'server.py'
                script.write_text('import os, pathlib, time\npathlib.Path(__file__).with_suffix(".started").write_text(str(os.getpid()))\ntime.sleep(60)\n')
                (kit / 'manifest.json').write_text(json.dumps({
                    'name': f'slow{n}', 'version': '1.0.0', 'eager': True,
                    'command': [sys.executable, str(script)],
                    'tools': [{'name': 'test', 'description': 'slow fixture', 'params': {'type': 'object'}}]}))
            profile = root / 'profile'
            config = profile / '.heart-portal/portal.toml'
            config.parent.mkdir(parents=True)
            config.write_text('workspace = "./workspace"\nbind = "127.0.0.1:0"\n'
                              f'kits_dir = {json.dumps(str(kits))}\n')
            with open(root / 'runtime.log', 'wb') as log:
                process = subprocess.Popen([str(target)], cwd=root, stdout=log, stderr=log,
                                           env=dict(os.environ, HOME=str(profile)))
                runtime = profile / '.heart-portal/runtime'
                try:
                    # Exercise the actual upgrade readiness predicate while
                    # four non-fatal 10-second kit timeouts are still pending.
                    nonce = e2e.wait_for(lambda: manager.saved(runtime, '.portal-launch-nonce'), timeout=8)
                    version = subprocess.check_output([str(target), '--version'], text=True).strip().split()[1]
                    e2e.worker.wait_ready(runtime, version, nonce, timeout=8)
                    self.assertEqual(process.poll(), None)
                    self.assertFalse((root / 'portal.toml').exists(), 'config must stay in the user directory')
                    e2e.wait_for(lambda: len(list(kits.glob('*/server.started'))) == 4, timeout=40)
                    self.assertIsNone(process.poll())
                    self.assertEqual(manager.checkout_pids(runtime), [process.pid])
                    self.assertFalse(list(root.glob('.portal-*')), 'download directory must stay clean')
                finally:
                    manager.stop_supervisor(runtime)
                    manager.stop_checkout(runtime)
                    process.wait(timeout=15)
                    for marker in kits.glob('*/server.started'):
                        pid = int(marker.read_text())
                        e2e.wait_for(lambda: manager.executable_path(pid) is None, timeout=5)


if __name__ == '__main__':
    unittest.main(verbosity=2)
