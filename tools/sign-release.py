#!/usr/bin/env python3
"""Sign prebuilt release packages using protected-environment secrets, never test keys."""
import argparse
import datetime
import json
import hashlib
import os
import pathlib
import subprocess
import tempfile
import tomllib

TARGETS = (('macos','aarch64','dmg'), ('macos','x86_64','dmg'), ('linux','x86_64','tar.gz'), ('windows','x86_64','zip'))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory',type=pathlib.Path,required=True)
    args=parser.parse_args()
    version=tomllib.loads(pathlib.Path('Cargo.toml').read_text())['workspace']['package']['version']
    commit=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
    if subprocess.check_output(['git','status','--porcelain','--untracked-files=all']).strip():
        parser.error('Publisher signing requires a clean committed checkout')
    key_pem=os.environ.get('GM_RELEASE_SIGNING_KEY_PEM','')
    expected=os.environ.get('GM_RELEASE_PUBLIC_KEY_HEX','').lower()
    if not key_pem or len(bytes.fromhex(expected))!=32:
        parser.error('Protected publisher key and independently pinned public key are required')
    specs=[]
    for os_name,arch,ext in TARGETS:
        path=args.directory/f'GrokMuse-{version}-{os_name}-{arch}.{ext}'
        if not path.is_file() or path.is_symlink() or not 0 < path.stat().st_size <= 2*1024**3:
            parser.error('Package must be a bounded regular file')
        receipt=json.loads(pathlib.Path(str(path)+'.build.json').read_text())
        with path.open('rb') as source:
            if hashlib.file_digest(source,'sha256').hexdigest()!=receipt.get('sha256'):
                parser.error('Package bytes differ from their native build receipt')
        if (receipt.get('commit'),receipt.get('version'),receipt.get('os'),receipt.get('architecture'))!=(commit,version,os_name,arch):
            parser.error('All packages must be built from this exact commit/version/target')
        specs += ['--asset',f'{os_name}:{arch}:{path.resolve()}']
    with tempfile.TemporaryDirectory(prefix='gm-publisher-') as temp:
        key=pathlib.Path(temp)/'publisher.pem'
        key.touch(mode=0o600)
        key.write_text(key_pem)
        public=subprocess.check_output(['openssl','pkey','-in',str(key),'-pubout','-outform','DER'])
        if len(public)!=44 or public[:12]!=bytes.fromhex('302a300506032b6570032100') or public[-32:].hex()!=expected:
            parser.error('Publisher private key does not match pinned build key')
        expires=(datetime.datetime.now(datetime.timezone.utc)+datetime.timedelta(days=30)).isoformat().replace('+00:00','Z')
        subprocess.run(['python3','tools/make-release-manifest.py','--version',version,'--expires',expires,*specs,'--private-key',str(key),'--output-dir',str(args.directory)],check=True)
        for os_name,arch,ext in TARGETS:
            asset=args.directory/f'GrokMuse-{version}-{os_name}-{arch}.{ext}'
            subprocess.run(['openssl','pkeyutl','-sign','-rawin','-inkey',str(key),'-in',str(asset),'-out',str(asset)+'.sig'],check=True)
    print('Signed all four target packages and release manifest from '+commit)


if __name__=='__main__':
    main()
