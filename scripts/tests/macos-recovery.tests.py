#!/usr/bin/env python3
"""Default-session recovery through the real CLI, without changing login/TCC setup."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('fixtures', Path(__file__).with_name('macos-upgrade.tests.py'))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
manager, read, wait_for = fixtures.manager, fixtures.read, fixtures.wait_for
BINARY = Path(os.environ.get('PORTAL_TEST_BINARY', REPO / 'target/debug/heart-portal')).resolve()


@unittest.skipUnless(sys.platform == 'darwin', 'requires macOS')
class SessionRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="portal recovery 中文 ' ")
        profile = Path(self.temp.name).resolve()
        home_patch = patch.dict(os.environ, HOME=str(profile))
        home_patch.start()
        self.addCleanup(home_patch.stop)
        self.root = profile / '.heart-portal/runtime'
        self.root.mkdir(parents=True)
        self.target = self.root / 'heart-portal'
        shutil.copy2(BINARY, self.target)
        version = subprocess.check_output([str(BINARY), '--version'], text=True).strip().split()[1]
        major, minor, patch_version = map(int, version.split('.'))
        self.candidate_version = f'{major}.{minor}.{patch_version + 1}'
        self.config = self.root / 'custom.toml'
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        self.config.write_text(f'workspace = "./workspace"\nkits_enabled = false\nbind = "127.0.0.1:{port}"\n')
        (self.root / '.portal-python').write_text(sys.executable)
        (self.root / '.portal-launch-nonce').write_text('fixture')
        self.processes = []
        self.log = open(self.root / 'test.log', 'ab')

    def tearDown(self):
        for process in self.processes:
            if process.poll() is None and manager.executable_path(process.pid) != self.target:
                process.kill()
                process.wait(timeout=10)
        manager.stop_supervisor(self.root)
        manager.stop_checkout(self.root)
        for process in self.processes:
            process.wait(timeout=15)
        self.log.close()
        self.temp.cleanup()

    def start(self):
        process = subprocess.Popen([str(self.target), '--config', str(self.config), '--name', 'original-name'],
                                   cwd=self.root, stdout=self.log, stderr=self.log)
        self.processes.append(process)
        return process

    def stage(self, name):
        stage = self.root / '.portal-upgrades' / name
        stage.mkdir(parents=True)
        for filename in ('portal-macos.py', 'portal-macos-upgrade.py'):
            shutil.copy2(REPO / 'scripts' / filename, stage)
        fixtures.upgrade.write_json(stage / 'request.json', {
            'root': str(self.root), 'target': str(self.target), 'version': self.candidate_version, 'parent_pid': 0,
            'worker_plist': str(stage / 'unused.plist')})
        return stage

    def orphan(self, stage):
        fixtures.upgrade.write_json(self.root / '.portal-upgrade.json', {
            'stage': str(stage), 'restart_mode': 'supervisor'})

    def ready(self, process):
        def check():
            if process.poll() is not None:
                self.fail((self.root / 'test.log').read_text())
            return read(self.root / '.portal-ready.json')
        return wait_for(check)

    def test_normal_start_restores_and_executes_old_bytes_with_same_pid_and_arguments(self):
        stage = self.stage('lost-session')
        fixtures.LaunchdTests.build(self, stage / 'previous', '0.8.0')
        previous = (stage / 'previous').read_bytes()
        self.orphan(stage)
        process = self.start()
        ready = self.ready(process)
        self.assertEqual(ready['version'], '0.8.0')
        self.assertEqual(ready['pid'], process.pid)
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertEqual(read(stage / 'result.json')['state'], 'rolled_back')
        self.assertFalse((self.root / '.portal-upgrade.json').exists())
        argv = subprocess.check_output(['/bin/ps', '-p', str(process.pid), '-o', 'command='], text=True)
        self.assertIn(str(self.config), argv)
        self.assertIn('original-name', argv)

    def test_new_session_recovers_then_normal_start_attaches_one_guardian(self):
        stage = self.stage('new-session')
        shutil.copy2(self.target, stage / 'previous')
        self.orphan(stage)
        process = self.start()
        ready = self.ready(process)
        self.assertEqual(ready['pid'], process.pid)
        self.assertTrue(manager.supervisor_state(self.root))
        self.assertEqual(manager.checkout_pids(self.root), [process.pid])
        self.assertEqual(read(stage / 'result.json')['state'], 'rolled_back')
        self.assertFalse((self.root / '.portal-upgrade.json').exists())
        self.assertIn('Recovering interrupted upgrade', (self.root / 'test.log').read_text())

    def test_active_worker_lock_prevents_startup_recovery(self):
        stage = self.stage('active-worker')
        shutil.copy2(self.target, stage / 'previous')
        self.orphan(stage)
        with manager.maintenance_lock(self.root):
            process = self.start()
            self.assertNotEqual(process.wait(timeout=60), 0)
            self.assertTrue((self.root / '.portal-upgrade.json').exists())
            self.assertFalse((stage / 'result.json').exists())

    def test_unarmed_guardian_survives_transaction_and_accepts_rollback_restart(self):
        fixtures.LaunchdTests.build(self, self.target, '0.8.0')
        (self.root / '.portal-launch-nonce').unlink()
        original = self.start()  # Fixture stays alive without publishing readiness.
        wait_for(lambda: manager.process_identity(original.pid))
        watcher = subprocess.Popen([sys.executable, str(REPO / 'scripts/portal-macos-supervisor.py'), 'watch'],
                                   stdin=subprocess.PIPE, stdout=self.log, stderr=self.log)
        self.processes.append(watcher)
        watcher.stdin.write(json.dumps({'root': str(self.root), 'target': str(self.target),
            'runtime_pid': original.pid, 'token': 'unarmed-test', 'arguments': [], 'cwd': str(self.root)}).encode())
        watcher.stdin.close()
        wait_for(lambda: manager.supervisor_state(self.root))
        stage = self.stage('unarmed')
        with manager.maintenance_lock(self.root):
            self.orphan(stage)
            original.kill()
            original.wait(timeout=10)
            time.sleep(.5)
            self.assertIsNone(watcher.poll(), 'Unarmed guardian exited during an active transaction')
            fixtures.upgrade.write_json(self.root / '.portal-supervisor-restart.json', {
                'transaction': stage.name, 'id': 'rollback', 'owner': 'unarmed-test'})
            ready = wait_for(lambda: read(self.root / '.portal-ready.json'))
            self.assertNotEqual(ready['pid'], original.pid)
            (self.root / '.portal-upgrade.json').unlink()
        self.assertTrue(manager.supervisor_state(self.root))

    def test_initial_bind_failure_does_not_become_background_retry_loop(self):
        import re
        port = int(re.search(r'127.0.0.1:(\d+)', self.config.read_text())[1])
        with socket.socket() as occupied:
            occupied.bind(('127.0.0.1', port))
            occupied.listen()
            process = self.start()
            self.assertNotEqual(process.wait(timeout=60), 0)
            wait_for(lambda: not manager.supervisor_state(self.root))
            time.sleep(.5)
            self.assertFalse(manager.checkout_pids(self.root))

    def test_startup_rejects_recovery_worker_outside_installation(self):
        with tempfile.TemporaryDirectory() as outside:
            stage = Path(outside).resolve()
            (stage / 'portal-macos-upgrade.py').write_text('raise RuntimeError("must not execute")')
            self.orphan(stage)
            process = self.start()
            self.assertNotEqual(process.wait(timeout=60), 0)
            log = (self.root / 'test.log').read_text()
            self.assertIn('outside this installation', log)
            self.assertNotIn('must not execute', log)

    def start_upgrade(self, launch_error=False):
        stage = self.stage('failed-launch')
        fixtures.LaunchdTests.build(self, stage / 'candidate', self.candidate_version, broken=True)
        # Isolate lifecycle from signing credentials. There is no production
        # signature bypass; the real-signature suite covers candidate trust.
        wrapper = stage / 'fixture.py'
        wrapper.write_text('''import runpy
worker = runpy.run_path(__file__.replace('fixture.py', 'portal-macos-upgrade.py'))
scope = worker['run'].__globals__
scope['verify_signatures'] = lambda *args: True
''' + ('''original_restart = scope['restart']
def restart(root, domain, plist, mode, expected=None, nonce=None):
    if expected == CANDIDATE_VERSION:
        scope['manager'].binary_path(root).chmod(0o600)
    return original_restart(root, domain, plist, mode, expected, nonce)
scope['restart'] = restart
'''.replace('CANDIDATE_VERSION', repr(self.candidate_version)) if launch_error else '') + "raise SystemExit(worker['main']())\n")
        process = subprocess.Popen([sys.executable, str(wrapper)], cwd=stage,
                                   stdout=self.log, stderr=self.log, start_new_session=True)
        self.processes.append(process)
        return stage, process

    def check_failed_candidate(self, launch_error):
        original = self.start()
        self.ready(original)
        owner = manager.supervisor_state(self.root)['owner']
        previous = self.target.read_bytes()
        stage, process = self.start_upgrade(launch_error)
        result = wait_for(lambda: read(stage / 'result.json'), timeout=60)
        self.assertEqual(result['state'], 'rolled_back')
        self.assertFalse(result['restart_required'])
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertFalse((self.root / '.portal-upgrade.json').exists())
        self.assertEqual(manager.supervisor_state(self.root)['owner'], owner)
        self.assertEqual(len(manager.checkout_pids(self.root)), 1)
        self.assertEqual(process.wait(timeout=60), 0)

    def test_pre_ready_candidate_exit_rolls_back_through_original_guardian(self):
        self.check_failed_candidate(False)

    def test_exec_failure_keeps_original_guardian_available_for_rollback(self):
        self.check_failed_candidate(True)

    def test_lost_guardian_during_upgrade_finishes_rollback_and_allows_normal_start(self):
        original = self.start()
        self.ready(original)
        previous = self.target.read_bytes()
        stage, process = self.start_upgrade()
        wait_for(lambda: read(self.root / '.portal-upgrade-status.json').get('state') == 'verifying')
        manager.stop_supervisor(self.root)
        result = wait_for(lambda: read(stage / 'result.json'), timeout=60)
        self.assertEqual(result['state'], 'rolled_back')
        self.assertTrue(result['restart_required'])
        self.assertEqual(self.target.read_bytes(), previous)
        self.assertFalse((self.root / '.portal-upgrade.json').exists())
        process.wait(timeout=60)
        restarted = self.start()
        self.assertEqual(self.ready(restarted)['pid'], restarted.pid)
        self.assertTrue(manager.supervisor_state(self.root))


if __name__ == '__main__':
    unittest.main(verbosity=2)
