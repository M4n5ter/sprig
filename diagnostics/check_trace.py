#!/usr/bin/env python3
"""Unprivileged CLI diagnostics acceptance; not a kernel/FUSE test."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
rows = []
with tempfile.TemporaryDirectory(prefix='sprig-trace-check-') as root:
    root = Path(root)
    source = root / 'source'
    source.mkdir()
    (source / 'file').write_text('retained bytes\n')
    store = root / 'store'

    def invoke(enabled, *args, failure=False):
        env = dict(os.environ)
        env.pop('SPRIG_LIFECYCLE_TRACE', None)
        if enabled is not None:
            env['SPRIG_LIFECYCLE_TRACE'] = enabled
        result = subprocess.run([str(binary), '--json', '--store', str(store), *args],
                                env=env, text=True, capture_output=True, timeout=30)
        assert (result.returncode != 0) == failure, result
        parsed = json.loads(result.stdout)
        events = []
        for line in result.stderr.splitlines():
            assert line.startswith('SPRIG_LIFECYCLE_TRACE '), line
            event = json.loads(line.removeprefix('SPRIG_LIFECYCLE_TRACE '))
            assert set(event) == {'version', 'pid', 'phase', 'elapsed_ns', 'retries', 'ok'}, event
            assert event['version'] == 1 and event['pid'] > 0
            assert type(event['elapsed_ns']) is int and event['elapsed_ns'] >= 0
            assert type(event['retries']) is int and event['retries'] >= 0
            assert event['ok'] is None or type(event['ok']) is bool
            events.append(event)
        if enabled != '1':
            assert not events
        rows.append({'arguments': args, 'trace': enabled, 'returncode': result.returncode,
                     'stdout': parsed, 'events': events})
        return result, parsed, events

    invoke(None, 'init')
    _, imported, _ = invoke(None, '--writable', 'import', str(source), 'main')
    baseline, _, _ = invoke(None, 'writable-status', 'main')
    enabled, _, events = invoke('1', 'writable-status', 'main')
    assert enabled.stdout == baseline.stdout and enabled.returncode == baseline.returncode
    assert any(e['phase'] == 'writable.core_open_replay' for e in events)
    invoke('0', 'writable-status', 'main')
    invoke('true', 'writable-status', 'main')
    checkpoint, _, _ = invoke(None, 'checkpoint', 'main')
    checked, _, events = invoke('1', 'checkpoint', 'main')
    assert checkpoint.stdout == checked.stdout and checkpoint.returncode == checked.returncode
    assert any(e['phase'] == 'store.checkpoint' for e in events)
    assert any(e['phase'] == 'writable.capture_snapshot' and e['ok'] is True for e in events)
    failed, _, _ = invoke(None, 'checkpoint', 'missing', failure=True)
    traced_failed, _, events = invoke('1', 'checkpoint', 'missing', failure=True)
    assert failed.stdout == traced_failed.stdout and failed.returncode == traced_failed.returncode
    assert any(e['phase'] == 'store.checkpoint' and e['ok'] is None for e in events)
    invoke('1', '--writable', 'restore', imported['result']['base_revision'], 'restored')

print(json.dumps({'ok': True, 'kernel_test': False,
                  'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                  'records': rows}, indent=2))
