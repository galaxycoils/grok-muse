#!/usr/bin/env python3
"""Validate publisher-signed native acceptance receipts. Fixtures cannot certify a release."""
import argparse
import datetime
import hashlib
import json
import pathlib
import subprocess
import tempfile

TARGETS = {('macos', 'aarch64', 'native'), ('macos', 'x86_64', 'native'),
           ('windows', 'x86_64', 'native'), ('linux', 'x86_64', 'x11'), ('linux', 'x86_64', 'wayland')}
CHECKS = {'onboarding', 'permission_revocation', 'multi_monitor', 'approvals_handoff', 'restart_persistence',
          'accessibility', 'ime', 'clean_install', 'upgrade', 'manual_recovery', 'service_lifecycle', 'uninstall',
          'vm_offline_boot', 'vm_cli_boot', 'cli_claude', 'cli_codex', 'cli_grok', 'cli_cancellation',
          'cli_output_limit', 'cli_egress_denied', 'cli_credentials_isolated', 'real_vlm_qwen', 'real_vlm_smol',
          'api_meta', 'api_xai', 'citation_verification', 'patch_review_apply'}


def verify(raw, signature_hex, public_hex):
    key = bytes.fromhex(public_hex)
    signature = bytes.fromhex(signature_hex.strip())
    if len(key) != 32 or len(signature) != 64:
        raise ValueError('Invalid Ed25519 key/signature')
    with tempfile.TemporaryDirectory(prefix='gm-evidence-') as folder:
        root = pathlib.Path(folder)
        (root / 'key.der').write_bytes(bytes.fromhex('302a300506032b6570032100') + key)
        (root / 'receipt').write_bytes(raw)
        (root / 'signature').write_bytes(signature)
        subprocess.run(['openssl', 'pkeyutl', '-verify', '-pubin', '-keyform', 'DER', '-inkey', str(root / 'key.der'),
                        '-rawin', '-in', str(root / 'receipt'), '-sigfile', str(root / 'signature')], check=True, capture_output=True)


def validate(data, commit, root):
    if data.get('schema') != 1 or data.get('commit') != commit or data.get('source') != 'native':
        raise ValueError('Evidence must describe native execution of this exact commit')
    recorded = datetime.datetime.fromisoformat(data['recorded_at'].replace('Z', '+00:00'))
    if recorded.tzinfo is None:
        raise ValueError('Evidence timestamp requires a timezone')
    age = datetime.datetime.now(datetime.timezone.utc) - recorded
    if not datetime.timedelta(0) <= age <= datetime.timedelta(days=30):
        raise ValueError('Native evidence must be current (within 30 days)')
    catalog = json.loads((pathlib.Path(__file__).resolve().parent.parent / 'docs/release/acceptance-tasks.json').read_text())
    required_tasks = {task['id']: task['workflow'] for task in catalog}
    targets = set()
    for report in data.get('targets', []):
        target = tuple(report.get(key) for key in ('os', 'architecture', 'session'))
        if target not in TARGETS or target in targets:
            raise ValueError('Unknown or duplicate certification target')
        targets.add(target)
        if set(report.get('checks', {})) != CHECKS or any(v is not True for v in report['checks'].values()):
            raise ValueError(f'Missing or failed native checks for {target}')
        runs = report.get('runs', [])
        if len(runs) != 3:
            raise ValueError('Three consecutive native runs are required')
        for run in runs:
            tasks = run.get('tasks', [])
            if len(tasks) != 30 or len({task['id'] for task in tasks}) != 30:
                raise ValueError('Each run requires 30 unique tasks')
            if {task['id']: task.get('workflow') for task in tasks} != required_tasks:
                raise ValueError('Tasks must match the reviewed acceptance catalog')
            counts = {workflow: sum(t.get('workflow') == workflow for t in tasks) for workflow in ('research', 'coding', 'desktop')}
            if set(counts.values()) != {10} or any(type(t.get('success')) is not bool or type(t.get('unapproved_irreversible')) is not bool for t in tasks):
                raise ValueError('Need ten measured tasks per workflow with explicit outcomes')
            if sum(t['success'] for t in tasks) < 21 or any(t['unapproved_irreversible'] for t in tasks):
                raise ValueError('Quality/safety gate failed')
        limits = dict(idle_tree_mib=50, ui_tree_mib=120, capture_p99_ms=2, ax_p95_ms=30,
                      jpeg_p95_ms=15, dispatch_p95_ms=5, kill_ms=100, step_p50_ms=1500, step_p95_ms=4000)
        measured = report.get('measurements', {})
        if type(measured.get('samples')) is not int or measured['samples'] < 30:
            raise ValueError('At least 30 native measurement samples required')
        for key, limit in limits.items():
            value = measured.get(key)
            if type(value) not in (int, float) or not 0 < value < limit:
                raise ValueError(f'Native performance gate failed: {target} {key}')
        artifacts = report.get('artifacts', [])
        if not artifacts:
            raise ValueError('Evidence requires referenced logs/results')
        for artifact in artifacts:
            relative = pathlib.PurePosixPath(artifact['path'])
            if relative.is_absolute() or '..' in relative.parts or '\\' in str(relative):
                raise ValueError('Unsafe evidence artifact path')
            path = root.joinpath(*relative.parts)
            if path.is_symlink() or root.resolve() not in path.resolve().parents or not path.is_file():
                raise ValueError('Evidence artifact must be a contained regular file')
            if path.stat().st_size > 128 * 1024**2:
                raise ValueError('Evidence artifact exceeds size limit')
            with path.open('rb') as stream:
                digest = hashlib.file_digest(stream, 'sha256').hexdigest()
            if digest != artifact['sha256']:
                raise ValueError('Evidence artifact checksum mismatch')
    if targets != TARGETS:
        raise ValueError('Native evidence is missing one or more release targets')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', type=pathlib.Path, required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--public-key-hex', required=True)
    args = parser.parse_args()
    receipt = args.directory / 'evidence.json'
    if receipt.stat().st_size > 2 * 1024**2:
        parser.error('Evidence receipt exceeds 2 MiB')
    raw = receipt.read_bytes()
    verify(raw, (args.directory / 'evidence.sig').read_text(), args.public_key_hex)
    validate(json.loads(raw), args.commit, args.directory)
    print('Native release evidence verified for all targets. No fixture result was used.')


if __name__ == '__main__':
    main()
