#!/usr/bin/env python3
"""Build an Ed25519-authenticated release manifest using an offline publisher key."""
import argparse
import datetime
import hashlib
import json
import pathlib
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--expires', required=True, help='RFC3339 UTC expiry, e.g. 2027-01-01T00:00:00Z')
    parser.add_argument('--asset', action='append', required=True, help='os:architecture:/path/to/asset')
    parser.add_argument('--private-key', type=pathlib.Path, required=True, help='Offline Ed25519 PEM key')
    parser.add_argument('--output-dir', type=pathlib.Path, required=True)
    args = parser.parse_args()
    expires = datetime.datetime.fromisoformat(args.expires.replace('Z', '+00:00'))
    if expires.tzinfo is None or expires <= datetime.datetime.now(datetime.timezone.utc):
        parser.error('Expiry must be a future time with a timezone')
    assets, identities = [], set()
    for spec in args.asset:
        os_name, architecture, filename = spec.split(':', 2)
        if os_name not in ('linux', 'macos', 'windows') or architecture not in ('x86_64', 'aarch64'):
            parser.error('Unsupported OS or architecture')
        path = pathlib.Path(filename)
        identity = (path.name, os_name, architecture)
        if identity in identities or not path.is_file() or path.is_symlink():
            parser.error('Duplicate asset or asset is not a regular file')
        identities.add(identity)
        size = path.stat().st_size
        if not 0 < size <= 2 * 1024**3:
            parser.error('Asset size must be between 1 byte and 2 GiB')
        with path.open('rb') as source:
            digest = hashlib.file_digest(source, 'sha256').hexdigest()
        assets.append(dict(name=path.name, os=os_name, architecture=architecture, size_bytes=size, sha256=digest))
    raw = (json.dumps(dict(schema=1, version=args.version, expires_at=args.expires, assets=assets), sort_keys=True, separators=(',', ':')) + '\n').encode()
    with tempfile.TemporaryDirectory(prefix='gm-sign-') as folder:
        folder = pathlib.Path(folder)
        manifest, signature = folder / 'manifest', folder / 'signature'
        manifest.write_bytes(raw)
        subprocess.run(['openssl', 'pkeyutl', '-sign', '-rawin', '-inkey', str(args.private_key), '-in', str(manifest), '-out', str(signature)], check=True)
        public = subprocess.check_output(['openssl', 'pkey', '-in', str(args.private_key), '-pubout', '-outform', 'DER'])
        if not public.startswith(bytes.fromhex('302a300506032b6570032100')) or len(public) != 44 or len(signature.read_bytes()) != 64:
            parser.error('The release key must be Ed25519')
        args.output_dir.mkdir(parents=True, exist_ok=True)
        (args.output_dir / 'grok-muse-release.json').write_bytes(raw)
        (args.output_dir / 'grok-muse-release.sig').write_text(signature.read_bytes().hex() + '\n')
        print('Publisher public key (pin independently in the release build): ' + public[-32:].hex())


if __name__ == '__main__':
    main()
