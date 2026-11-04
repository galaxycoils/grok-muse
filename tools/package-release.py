#!/usr/bin/env python3
"""Package one native release target. Production signing never falls back to ad-hoc keys."""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import zipfile

BINARIES = ('gm', 'gm-daemon', 'gm-cu-helper', 'gm-ui')


def run(*args):
    subprocess.run([str(arg) for arg in args], check=True)


def native_target():
    os_name = dict(Darwin='macos', Linux='linux', Windows='windows')[platform.system()]
    arch = dict(arm64='aarch64', aarch64='aarch64', AMD64='x86_64', x86_64='x86_64')[platform.machine()]
    return os_name, arch


def archive_name(version, os_name, arch):
    if (os_name, arch) not in {('macos', 'aarch64'), ('macos', 'x86_64'), ('linux', 'x86_64'), ('windows', 'x86_64')}:
        raise ValueError('Unsupported release target')
    if not version or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-+' for c in version):
        raise ValueError('Invalid version')
    ext = dict(macos='dmg', windows='zip', linux='tar.gz')[os_name]
    return f'GrokMuse-{version}-{os_name}-{arch}.{ext}'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=pathlib.Path('target/release'))
    parser.add_argument('--output-dir', type=pathlib.Path, required=True)
    parser.add_argument('--apple-identity', default=os.environ.get('GM_APPLE_SIGNING_IDENTITY'))
    parser.add_argument('--notary-profile', default=os.environ.get('GM_APPLE_NOTARY_PROFILE'))
    parser.add_argument('--windows-thumbprint', default=os.environ.get('GM_WINDOWS_CERT_THUMBPRINT'))
    args = parser.parse_args()
    os_name, arch = native_target()
    name = archive_name(args.version, os_name, arch)
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    if subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=all']).strip():
        parser.error('Release packages must be built from a clean committed checkout')
    suffix = '.exe' if os_name == 'windows' else ''
    inputs = [args.bin_dir / (binary + suffix) for binary in BINARIES]
    if any(not path.is_file() or path.is_symlink() for path in inputs):
        parser.error('All four regular native binaries must exist before packaging')
    if os_name == 'macos' and (not args.apple_identity or args.apple_identity == '-' or not args.notary_profile):
        parser.error('Developer ID signing identity and notarization profile are required')
    if os_name == 'windows' and not args.windows_thumbprint:
        parser.error('A Windows signing certificate thumbprint is required')
    args.output_dir.mkdir(parents=True, exist_ok=True)
    output = args.output_dir / name
    if output.exists():
        parser.error('Output already exists; refusing to replace a release artifact')
    with tempfile.TemporaryDirectory(prefix='gm-release-') as folder:
        root = pathlib.Path(folder) / 'GrokMuse'
        root.mkdir()
        docs = root / 'docs'
        if pathlib.Path('docs').is_symlink() or any(path.is_symlink() for path in pathlib.Path('docs').rglob('*')):
            parser.error('Documentation symlinks cannot be included in release packages')
        shutil.copytree('docs', docs)
        for path in ('LICENSE', 'THIRD_PARTY_NOTICES.md', 'README.md', 'CHANGELOG.md'):
            if pathlib.Path(path).is_symlink():
                parser.error('Release notices must not be symlinks')
            shutil.copy2(path, root / path)
        (root / 'source.json').write_text(json.dumps(dict(version=args.version, commit=commit, os=os_name, architecture=arch), sort_keys=True) + '\n')
        destination = root / 'bin'
        destination.mkdir()
        for source in inputs:
            shutil.copy2(source, destination / source.name)
        if os_name == 'macos':
            for app_name, executable, identifier, binaries in (
                ('GrokMuse', 'gm-ui', 'co.anysphere.grok-muse.ui', ('gm', 'gm-daemon', 'gm-ui')),
                ('GrokMuseCUHelper', 'gm-cu-helper', 'co.anysphere.grok-muse.cu-helper', ('gm-cu-helper',)),
            ):
                app = root / (app_name + '.app')
                macos = app / 'Contents' / 'MacOS'
                macos.mkdir(parents=True)
                info = dict(CFBundleIdentifier=identifier, CFBundleName=app_name, CFBundleExecutable=executable,
                            CFBundleVersion=args.version, CFBundleShortVersionString=args.version, CFBundlePackageType='APPL',
                            NSAccessibilityUsageDescription='Inspect and control explicitly authorized desktop targets.',
                            NSScreenCaptureUsageDescription='Observe explicitly authorized desktop tasks.',
                            NSCalendarsUsageDescription='Read calendar events for opted-in routines.',
                            NSRemindersUsageDescription='Read reminders for opted-in routines.')
                if app_name == 'GrokMuseCUHelper':
                    info['LSUIElement'] = True
                (app / 'Contents' / 'Info.plist').write_bytes(plistlib.dumps(info))
                for binary in binaries:
                    target = macos / binary
                    shutil.copy2(destination / binary, target)
                    run('codesign', '--force', '--options', 'runtime', '--timestamp', '--entitlements', 'entitlements.plist', '--sign', args.apple_identity, target)
                run('codesign', '--force', '--options', 'runtime', '--timestamp', '--sign', args.apple_identity, app)
                run('codesign', '--verify', '--deep', '--strict', app)
            shutil.rmtree(destination)
            (root / 'Applications').symlink_to('/Applications')
            run('hdiutil', 'create', '-volname', 'Grok-Muse', '-srcfolder', root, '-ov', '-format', 'UDZO', output)
            run('codesign', '--force', '--timestamp', '--sign', args.apple_identity, output)
            run('xcrun', 'notarytool', 'submit', output, '--keychain-profile', args.notary_profile, '--wait')
            run('xcrun', 'stapler', 'staple', output)
            run('xcrun', 'stapler', 'validate', output)
        elif os_name == 'windows':
            for binary in BINARIES:
                target = destination / (binary + '.exe')
                run('signtool.exe', 'sign', '/sha1', args.windows_thumbprint, '/fd', 'SHA256', '/tr', 'http://timestamp.digicert.com', '/td', 'SHA256', target)
                run('signtool.exe', 'verify', '/pa', '/all', target)
            with zipfile.ZipFile(output, 'x', compression=zipfile.ZIP_DEFLATED) as archive:
                for path in sorted(root.rglob('*')):
                    if path.is_file():
                        archive.write(path, path.relative_to(root.parent))
        else:
            # Record runtime system dependencies; unresolved libraries block packaging.
            dependencies = []
            for binary in BINARIES:
                result = subprocess.run(['ldd', str(destination / binary)], capture_output=True, text=True, check=True)
                if 'not found' in result.stdout:
                    raise RuntimeError('Unresolved native dependencies')
                dependencies.append(binary + '\n' + result.stdout)
            (root / 'runtime-dependencies.txt').write_text('\n'.join(dependencies))
            with tarfile.open(output, 'x:gz') as archive:
                archive.add(root, arcname='GrokMuse')
    with output.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    (args.output_dir / (name + '.sha256')).write_text(f'{digest}  {name}\n')
    (args.output_dir / (name + '.build.json')).write_text(json.dumps(dict(commit=commit, version=args.version, os=os_name, architecture=arch, sha256=digest), sort_keys=True) + '\n')
    print(output)


if __name__ == '__main__':
    main()
