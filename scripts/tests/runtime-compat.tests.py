#!/usr/bin/env python3
"""Offline regressions for local setup, installation delegation and shutdown."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get('PORTAL_TEST_BINARY', str(
    REPO / 'target/debug' / ('heart-portal.exe' if os.name == 'nt' else 'heart-portal')
))).resolve()


class RuntimeCompatibilityTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='portal-runtime-compat-')
        self.root = Path(temporary.name).resolve()
        self.addCleanup(temporary.cleanup)
        self.home = self.root / 'home'
        self.installed = self.home / '.heart-portal/runtime' / ('heart-portal.exe' if os.name == 'nt' else 'heart-portal')
        self.installed.parent.mkdir(parents=True)
        shutil.copy2(BINARY, self.installed)
        self.config = self.root / 'portal.toml'
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(('PORTAL_', 'HEART_PORTAL_'))}
        self.env.update(HOME=str(self.home), USERPROFILE=str(self.home))

    def test_local_subagent_setup_preserves_all_permission_modes(self):
        # An existing local executable stands in for pi: setup must only save
        # configuration, without installing packages or starting a model.
        for mode in ('read-only', 'supervised', 'auto-review', 'full-access'):
            with self.subTest(mode=mode):
                self.config.write_text(
                    'workspace="./workspace"\nkits_enabled=false\n[subagent]\n'
                    'enabled=false\nauto_install=false\ncommand=' + json.dumps([sys.executable]) +
                    '\n[security]\npermission_mode="' + mode + '"\n', encoding='utf-8')
                result = subprocess.run([str(self.installed), '--config', str(self.config), 'subagent-setup'],
                    input=json.dumps({'provider': 'openai', 'model': 'offline-fixture', 'thinking': 'medium'}),
                    env=self.env, cwd=self.root, text=True, capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                line = next(line for line in result.stdout.splitlines() if line.startswith('DESKTOP_SUBAGENT_RESULT='))
                response = json.loads(line.split('=', 1)[1])
                self.assertFalse(response.get('isError'), response)
                value = json.loads(response['content'][0]['text'])
                self.assertTrue(value['ok'])
                self.assertTrue(value['pi_installed'])
                self.assertIsNone(value['install_error'])
                saved = self.config.read_text(encoding='utf-8')
                self.assertIn('permission_mode="' + mode + '"', saved)
                self.assertIn('offline-fixture', saved)

    @unittest.skipUnless(sys.platform == 'darwin', 'exec delegation uses a shell fixture on macOS')
    def test_download_forwards_runtime_overrides_to_installed_binary(self):
        download = self.root / 'download' / 'heart-portal'
        download.parent.mkdir()
        shutil.copy2(BINARY, download)
        self.installed.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
        self.installed.chmod(0o700)
        self.config.write_text('[security]\npermission_mode="full-access"\n')
        for mode in ('read-only', 'supervised', 'auto-review', 'full-access'):
            overrides = ['--exec-enabled', 'false', '--kits-enabled', 'false', '--permission-mode', mode]
            result = subprocess.run([str(download), '--config', str(self.config), *overrides],
                env=self.env, cwd=self.root, text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.splitlines(), overrides + ['--config', str(self.config)])

    def test_shutdown_cancels_installer_and_its_descendants(self):
        marker = self.root / 'installer-started'
        escaped = self.root / 'installer-descendant-survived'
        script = self.root / 'installer.py'
        script.write_text(
            'import pathlib, subprocess, sys, time\n'
            'subprocess.Popen([sys.executable, "-c", ' + repr(
                'import pathlib,time; time.sleep(2); pathlib.Path(' + repr(str(escaped)) + ').write_text("escaped")') + '])\n'
            'pathlib.Path(' + repr(str(marker)) + ').write_text("started")\ntime.sleep(30)\n', encoding='utf-8')
        bin_dir = self.root / 'bin'
        bin_dir.mkdir()
        npm = bin_dir / ('npm.cmd' if os.name == 'nt' else 'npm')
        if os.name == 'nt':
            npm.write_text('@echo off\r\n"' + sys.executable + '" "' + str(script) + '"\r\n')
        else:
            import shlex
            npm.write_text('#!/bin/sh\nexec ' + shlex.join([sys.executable, str(script)]) + '\n')
            npm.chmod(0o700)
        self.config.write_text('bind="127.0.0.1:0"\nworkspace="./workspace"\nkits_enabled=false\n'
                               '[subagent]\nenabled=true\nauto_install=true\n')
        self.env.update(PATH=str(bin_dir), HEART_PORTAL_SUPERVISED='1')
        for command in ([], ['subagent-setup']):
            with self.subTest(command=command), (self.root / 'runtime.log').open('w') as log:
                marker.unlink(missing_ok=True)
                child = subprocess.Popen([str(self.installed), '--config', str(self.config), *command],
                                         env=self.env, cwd=self.root, stdin=subprocess.PIPE, stdout=log, stderr=log)
                child.stdin.write(b'{}')
                child.stdin.close()
                try:
                    deadline = time.monotonic() + 10
                    while not marker.exists() and child.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.05)
                    self.assertTrue(marker.exists(), (self.root / 'runtime.log').read_text())
                    child.terminate()
                    child.wait(timeout=3)
                    time.sleep(2.5)
                    self.assertFalse(escaped.exists(), 'Installer descendants must stop with Portal')
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait(timeout=5)



if __name__ == '__main__':
    unittest.main()
