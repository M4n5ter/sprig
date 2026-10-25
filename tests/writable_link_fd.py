#!/usr/bin/env python3
"""Owned temporary-file hardlink/FD correctness regression, without mappings.
Rootful entrypoint uses ordinary supported CLI mounting and normal cleanup only.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def exercise(tree, events):
    source=tree/'held-fd.bin';alias=tree/'held-fd-alias.bin'
    payload=b'owned synthetic hardlink regression\n'*256
    fd=os.open(source,os.O_CREAT|os.O_EXCL|os.O_RDWR,0o600)
    try:
        assert os.write(fd,payload)==len(payload)
        os.fsync(fd)
        before=os.fstat(fd)
        assert os.pread(fd,len(payload),0)==payload
        events.append('open fd verified before hardlink')
        os.link(source,alias)
        events.append('ordinary hardlink created while fd remains open')
        assert os.pread(fd,len(payload),0)==payload
        assert os.pwrite(fd,b'AFTER',17)==5
        os.fsync(fd)
        expected=payload[:17]+b'AFTER'+payload[22:]
        assert alias.read_bytes()==source.read_bytes()==expected
        original=os.fstat(fd);linked=alias.stat();current=source.stat()
        assert (original.st_dev,original.st_ino)==(linked.st_dev,linked.st_ino)==(current.st_dev,current.st_ino)==(before.st_dev,before.st_ino)
        assert original.st_nlink==linked.st_nlink==current.st_nlink==2
        events.append('held fd pread/pwrite/fsync and alias identity passed')
    finally:os.close(fd)


def run(binary):
    if os.geteuid()!=0 or not Path('/proc/self/mountinfo').exists():
        raise RuntimeError('requires supported isolated rootful Linux runner')
    root=Path(tempfile.mkdtemp(prefix='sprig-link-fd-'))
    source=root/'source';source.mkdir();(source/'seed').write_text('synthetic\n')
    store=root/'store';commands=[];events=[];failure=None;remaining=[];uncertain=False
    def invoke(*args):
        nonlocal uncertain
        try:
            result=subprocess.run([str(binary),'--store',str(store),'--json',*args],cwd=root,capture_output=True,text=True,timeout=60)
        except subprocess.TimeoutExpired:
            uncertain=True
            raise RuntimeError('CLI outcome uncertain; retain root without further cleanup')
        commands.append({'command':list(args),'returncode':result.returncode,'stdout':result.stdout,'stderr':result.stderr})
        if result.returncode:raise RuntimeError('CLI command failed: '+str(args))
        value=json.loads(result.stdout)['result']
        if isinstance(value,dict) and value.get('mount_error'):raise RuntimeError('mount failed')
        return value
    try:
        invoke('init');invoke('--writable','import',str(source),'main');invoke('mount','main')
        info=invoke('show','main');assert info['mount_state']=='mounted'
        exercise(Path(info['path']),events)
    except BaseException as error:failure={'type':type(error).__name__,'message':str(error),'errno':getattr(error,'errno',None)}
    finally:
        if not uncertain:
            for pin in sorted((store/'native').glob('*/writable-base')):
                try:invoke('unmount-generation',pin.parent.name)
                except Exception as error:
                    remaining.append({'generation':pin.parent.name,'error':repr(error)})
                    if uncertain:break
        if uncertain:remaining.append({'error':'CLI outcome uncertain; cleanup blocked'})
        if str(root) in Path('/proc/self/mountinfo').read_text():remaining.append({'error':'mount still present'})
        logs={str(p.relative_to(store)):p.read_text(errors='replace') for p in (store/'native').glob('*/writable-*.log') if p.is_file() and not p.is_symlink()}
        result={'ok':failure is None and not remaining,'events':events,'commands':commands,'error':failure,'remainingMounts':remaining,'serverLogs':logs,'retainedRoot':str(root)}
        (root/'result.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
    return 0 if result['ok'] else 1

if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--binary',type=Path,required=True);a=p.parse_args()
    sys.exit(run(a.binary.resolve(strict=True)))
