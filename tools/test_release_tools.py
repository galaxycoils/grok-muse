"""Protocol/tool regressions. Synthetic inputs here do not certify a release."""
import copy
import datetime
import hashlib
import json
import pathlib
import runpy
import subprocess
import tempfile
import unittest

ROOT=pathlib.Path(__file__).resolve().parent.parent
EVIDENCE=runpy.run_path(str(ROOT/'tools/verify-release-evidence.py'))
PACKAGE=runpy.run_path(str(ROOT/'tools/package-release.py'))


class EvidenceTests(unittest.TestCase):
    def fixture(self, root):
        (root/'log.txt').write_text('synthetic tool regression fixture, not native evidence\n')
        checksum=hashlib.sha256((root/'log.txt').read_bytes()).hexdigest()
        catalog=json.loads((ROOT/'docs/release/acceptance-tasks.json').read_text())
        tasks=[dict(id=item['id'],workflow=item['workflow'],success=True,unapproved_irreversible=False) for item in catalog]
        metrics=dict(samples=30,idle_tree_mib=30,ui_tree_mib=100,capture_p99_ms=1,ax_p95_ms=10,jpeg_p95_ms=5,
                     dispatch_p95_ms=1,kill_ms=10,step_p50_ms=100,step_p95_ms=500)
        reports=[dict(os=os_name,architecture=arch,session=session,checks={key:True for key in EVIDENCE['CHECKS']},
                      runs=[dict(tasks=copy.deepcopy(tasks)) for _ in range(3)],measurements=metrics,
                      artifacts=[dict(path='log.txt',sha256=checksum)]) for os_name,arch,session in EVIDENCE['TARGETS']]
        return dict(schema=1,commit='a'*40,source='native',recorded_at=datetime.datetime.now(datetime.timezone.utc).isoformat(),targets=reports)

    def test_required_targets_catalog_safety_and_metrics(self):
        with tempfile.TemporaryDirectory() as folder:
            root=pathlib.Path(folder)
            data=self.fixture(root)
            EVIDENCE['validate'](data,'a'*40,root)
            mutations=[lambda d:d.update(source='fixture'),lambda d:d.update(commit='b'*40),
                       lambda d:d['targets'].pop(),lambda d:d['targets'][0]['runs'].pop(),
                       lambda d:d['targets'][0]['runs'][0]['tasks'][0].update(unapproved_irreversible=True),
                       lambda d:d['targets'][0]['runs'][0]['tasks'][0].update(id='unknown-task'),
                       lambda d:d['targets'][0]['checks'].update(vm_cli_boot=False),
                       lambda d:d['targets'][0]['measurements'].update(kill_ms=150),
                       lambda d:d['targets'][0]['artifacts'][0].update(path='../outside')]
            for mutate in mutations:
                with self.subTest(mutate=mutate):
                    changed=copy.deepcopy(data);mutate(changed)
                    with self.assertRaises(ValueError):EVIDENCE['validate'](changed,'a'*40,root)
            (root/'log.txt').write_text('changed')
            with self.assertRaises(ValueError):EVIDENCE['validate'](data,'a'*40,root)

    def test_signatures_and_manifest_names(self):
        with tempfile.TemporaryDirectory() as folder:
            root=pathlib.Path(folder)
            key=root/'test-only.pem'
            subprocess.run(['openssl','genpkey','-algorithm','ED25519','-out',str(key)],check=True,capture_output=True)
            public=subprocess.check_output(['openssl','pkey','-in',str(key),'-pubout','-outform','DER'])[-32:].hex()
            raw=b'fixture receipt'
            source=root/'receipt';source.write_bytes(raw)
            signature=root/'receipt.sig'
            subprocess.run(['openssl','pkeyutl','-sign','-rawin','-inkey',str(key),'-in',str(source),'-out',str(signature)],check=True,capture_output=True)
            signature_hex=signature.read_bytes().hex()
            EVIDENCE['verify'](raw,signature_hex,public)
            with self.assertRaises(subprocess.CalledProcessError):EVIDENCE['verify'](b'changed',signature_hex,public)
            asset=root/'GrokMuse-0.3.0-linux-x86_64.tar.gz';asset.write_bytes(b'synthetic-package')
            expires=(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(days=1)).isoformat()
            command=['python3',str(ROOT/'tools/make-release-manifest.py'),'--version','0.3.0','--expires',expires,'--asset',f'linux:x86_64:{asset}','--private-key',str(key),'--output-dir',str(root/'signed')]
            subprocess.run(command,check=True,capture_output=True)
            manifest=(root/'signed/grok-muse-release.json').read_bytes()
            EVIDENCE['verify'](manifest,(root/'signed/grok-muse-release.sig').read_text(),public)
            self.assertEqual(json.loads(manifest)['assets'][0]['sha256'],hashlib.sha256(asset.read_bytes()).hexdigest())
            other=root/'wrong-name.tar.gz';other.write_bytes(b'wrong')
            command[command.index('--asset')+1]=f'linux:x86_64:{other}'
            self.assertNotEqual(subprocess.run(command,capture_output=True).returncode,0)

    def test_exact_platform_names(self):
        self.assertEqual(PACKAGE['archive_name']('0.3.0','linux','x86_64'),'GrokMuse-0.3.0-linux-x86_64.tar.gz')
        with self.assertRaises(ValueError):PACKAGE['archive_name']('0.3.0','windows','aarch64')
        with self.assertRaises(ValueError):PACKAGE['archive_name']('../bad','macos','aarch64')


if __name__=='__main__':
    unittest.main()
