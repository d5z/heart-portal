#!/usr/bin/env python3
"""Metadata bounds and diagnostic privacy using temporary profiles and loopback MCP."""
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


fixture = load('kit_fixture', Path(__file__).with_name('kit-lifecycle.tests.py'))
manager = load('macos_metadata', REPO / 'scripts/portal-macos.py') if os.name != 'nt' else None


@unittest.skipIf(os.name == 'nt', 'Unix metadata helper; native Windows coverage is in windows-private-state.tests.ps1')
class MetadataTests(unittest.TestCase):
    def test_metadata_rejects_oversize_links_special_files_and_concurrent_growth(self):
        with tempfile.TemporaryDirectory(prefix='portal-metadata-review-') as tmp:
            root = Path(tmp)
            path = root / '.portal-name'
            path.write_bytes(b'x' * manager.METADATA_LIMIT)
            self.assertEqual(len(manager.saved(root, path.name)), manager.METADATA_LIMIT)
            path.write_bytes(b'x' * (manager.METADATA_LIMIT + 1))
            with self.assertRaises(ValueError):
                manager.saved(root, path.name)
            # The bytes may grow after fstat. Even a stale small size cannot
            # authorize an unbounded read of the opened object.
            with patch.object(manager.os, 'fstat', return_value=SimpleNamespace(st_mode=stat.S_IFREG, st_size=1)):
                with self.assertRaises(ValueError):
                    manager.metadata_bytes(path)
            link = root / 'link'
            link.symlink_to(path)
            with self.assertRaises(OSError):
                manager.metadata_bytes(link)
            fifo = root / 'fifo'
            os.mkfifo(fifo)
            with self.assertRaises(ValueError):
                manager.metadata_bytes(fifo)
            path.write_text('正常配置', encoding='utf-8')
            self.assertEqual(manager.saved(root, path.name), '正常配置')


class PublicDiagnosticsTests(fixture.KitLifecycleTests):
    expose_host_details = False

    def test_host_details_are_private_by_default(self):
        status = self.value('portal_status')
        self.assertFalse(status['capabilities']['host_details_visible'])
        self.assertIsNone(status['portal']['pid'])
        self.assertIsNone(status['portal']['executable'])
        for name in ('path', 'workspace', 'kits_directory', 'user_directory', 'custom_tools_config'):
            self.assertIsNone(status['config'][name])
        self.assertNotIn(str(self.root), json.dumps(status))
        self.assertTrue(status['portal']['build_id'])

    def test_rejected_auth_urls_are_never_returned_by_setup_or_status(self):
        directory = self.install()
        path = directory / 'manifest.json'
        manifest = json.loads(path.read_text())
        for url in ('http://example.test/login', 'javascript:alert(1)'):
            manifest['provision']['auth'] = {'required': False, 'methods': [
                {'id': 'login', 'provider': 'kit', 'url': url}]}
            path.write_text(json.dumps(manifest))
            self.value('portal_kits_reload')
            setup = self.value('portal_kits_setup', {'kit': 'sample'})
            self.assertEqual(setup['auth']['methods'][0]['status'], 'invalid')
            self.assertIsNone(setup['auth']['methods'][0]['url'])
            self.assertNotIn(url, json.dumps(self.value('portal_kits_status')))
            self.assertNotIn(url, json.dumps(setup))


class StandalonePermissionsTests(fixture.KitLifecycleTests):
    tools_enabled = True

    def test_standalone_defaults_to_full_access_without_client_registration(self):
        authorization = self.value('portal_status')['authorization']
        self.assertEqual(authorization['permission_mode'], 'FullAccess')
        self.assertEqual(authorization['command_backend'], 'host')
        self.assertEqual(authorization['custom_mcp_startup'], 'automatic')
        self.assertFalse(authorization['os_sandbox'])
        outside = self.root / 'outside.txt'
        for tool, arguments in [
            ('portal_file_write', {'path': str(outside), 'content': 'standalone-marker'}),
            ('portal_file_edit', {'path': str(outside), 'old_text': 'standalone', 'new_text': 'edited'}),
        ]:
            result = self.call(tool, arguments)
            self.assertFalse(result.get('isError'), result)
        self.assertEqual(outside.read_text(), 'edited-marker')
        self.assertIn('edited-marker', json.dumps(self.call('portal_file_read', {'path': str(outside)})))
        self.assertIn('edited-marker', json.dumps(self.call('portal_search', {'path': str(outside), 'pattern': 'marker'})))
        result = self.call('portal_exec', {'command': 'echo host-marker > shell.txt', 'workdir': str(self.root)})
        self.assertFalse(result.get('isError'), result)
        self.assertEqual((self.root / 'shell.txt').read_text().strip(), 'host-marker')


if __name__ == '__main__':
    suite = unittest.TestSuite(cls(name) for cls in (MetadataTests, PublicDiagnosticsTests, StandalonePermissionsTests)
                               for name in cls.__dict__ if name.startswith('test_'))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
