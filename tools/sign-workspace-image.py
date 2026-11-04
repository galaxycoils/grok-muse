#!/usr/bin/env python3
"""Authenticate a provisioned offline qcow2 image; this does not certify a boot or CLI profile."""
import argparse
import hashlib
import json
import pathlib
import subprocess
import tempfile


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source,'sha256').hexdigest()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image',type=pathlib.Path,required=True)
    parser.add_argument('--architecture',choices=['aarch64','x86_64'],required=True)
    parser.add_argument('--firmware',type=pathlib.Path)
    parser.add_argument('--private-key',type=pathlib.Path,required=True)
    parser.add_argument('--expected-public-key-hex',required=True)
    parser.add_argument('--output-dir',type=pathlib.Path,required=True)
    args=parser.parse_args()
    for path in [args.image,args.private_key]+([args.firmware] if args.firmware else []):
        if not path.is_file() or path.is_symlink():
            parser.error('Inputs must be regular files, not symlinks')
    if args.architecture=='aarch64' and not args.firmware:
        parser.error('ARM images require authenticated UEFI firmware')
    info=json.loads(subprocess.check_output(['qemu-img','info','-f','qcow2','--output=json',str(args.image)]))
    if info.get('format')!='qcow2' or info.get('backing-filename') or info.get('full-backing-filename') or info.get('encrypted'):
        parser.error('Only standalone unencrypted qcow2 images without host backing files are accepted')
    public=subprocess.check_output(['openssl','pkey','-in',str(args.private_key),'-pubout','-outform','DER'])
    if len(public)!=44 or public[:12]!=bytes.fromhex('302a300506032b6570032100') or public[-32:].hex()!=args.expected_public_key_hex.lower():
        parser.error('Image publisher must match the independently pinned Ed25519 public key')
    manifest=dict(version=1,architecture=args.architecture,sha256=digest(args.image))
    if args.firmware:
        manifest['firmware_sha256']=digest(args.firmware)
    raw=(json.dumps(manifest,sort_keys=True,separators=(',',':'))+'\n').encode()
    with tempfile.TemporaryDirectory(prefix='gm-image-sign-') as directory:
        directory=pathlib.Path(directory)
        source,signature=directory/'manifest.json',directory/'manifest.sig'
        source.write_bytes(raw)
        subprocess.run(['openssl','pkeyutl','-sign','-rawin','-inkey',str(args.private_key),'-in',str(source),'-out',str(signature)],check=True)
        (directory/'public.der').write_bytes(public)
        subprocess.run(['openssl','pkeyutl','-verify','-pubin','-keyform','DER','-inkey',str(directory/'public.der'),'-rawin','-in',str(source),'-sigfile',str(signature)],check=True)
        args.output_dir.mkdir(parents=True,exist_ok=True)
        # Immutable publication names; failed attempts must be reviewed explicitly.
        with (args.output_dir/'image-manifest.json').open('xb') as target:
            target.write(raw)
        with (args.output_dir/'image-manifest.sig').open('x') as target:
            target.write(signature.read_bytes().hex()+'\n')
    print('Offline image authenticated. Native boot, firmware licensing and isolation remain separate release gates.')


if __name__=='__main__':
    main()
