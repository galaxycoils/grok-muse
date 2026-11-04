"""Guest protocol boundaries; uses a temporary directory without running a VM."""
import base64
import importlib.util
import pathlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('gm_guest', pathlib.Path(__file__).with_name('agent.py'))
agent = importlib.util.module_from_spec(spec)
spec.loader.exec_module(agent)


class ExportTests(unittest.TestCase):
    def test_exports_and_symlink_rejection(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            (root / 'exports').mkdir()
            (root / 'exports' / 'change.patch').write_text('review me\n')
            exported = agent.export_artifacts(root)
            self.assertEqual(base64.b64decode(exported[0]['data']), b'review me\n')
            (root / 'exports' / 'escape').symlink_to('/etc/passwd')
            with self.assertRaises((ValueError, OSError)):
                agent.export_artifacts(root)

    def test_large_and_special_files_rejected(self):
        with tempfile.TemporaryDirectory() as folder:
            root = pathlib.Path(folder)
            (root / 'exports').mkdir()
            (root / 'exports' / 'huge').write_bytes(b'x' * (1024**2 + 1))
            with self.assertRaises(ValueError):
                agent.export_artifacts(root)


if __name__ == '__main__':
    unittest.main()
