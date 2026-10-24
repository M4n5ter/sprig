"""Generate/verify fail-closed execution guards around actual signing targets.

Preprocessed XML stays private. This script executes no MSBuild target. The
resulting BeforeTargets errors prevent signing/certificate tasks from running,
including when those tasks are dependencies of other signing targets.
"""
import argparse
import json
import re
import xml.etree.ElementTree as ET
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('xml', type=Path)
p.add_argument('report', type=Path)
p.add_argument('guard', type=Path)
p.add_argument('--verify', action='store_true')
a = p.parse_args()
if a.xml.stat().st_size > 64 * 1024 * 1024:
    raise SystemExit('Unexpected preprocessed project size')
root = ET.parse(a.xml).getroot()
local = lambda e: e.tag.rsplit('}', 1)[-1]
namespace = root.tag.split('}')[0][1:] if '}' in root.tag else ''
qualify = lambda name: '{' + namespace + '}' + name if namespace else name
for e in root.iter():
    if 'signmode' in e.attrib.get('TreatAsLocalProperty', '').lower():
        raise SystemExit('SignMode may not become a local property')
    for key in ('Properties', 'RemoveProperties', 'GlobalPropertiesToRemove', 'AdditionalProperties'):
        if 'signmode' in e.attrib.get(key, '').lower():
            raise SystemExit('Child project may change the global signing policy')
checks = []
for target in root.iter():
    if local(target) != 'Target':
        continue
    for task in target:
        name = local(task)
        command = task.attrib.get('Command', '')
        # Inf2Cat/MakeCat compute catalog hashes; they do not sign or create keys.
        dangerous = bool(re.search(r'cert|sign', name, re.I))
        if name.startswith('Assign') and 'cert' not in name.lower():
            dangerous = False
        if name == 'Exec':
            dangerous |= bool(re.search(r'signtool|makecert|certutil|new-selfsignedcertificate|\.pfx\b', command, re.I))
        if dangerous:
            target_name = target.attrib.get('Name', '')
            if not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9.]*', target_name):
                raise SystemExit('Unexpected signing target identifier')
            checks.append(dict(target=target_name, task=name))
names = sorted({entry['target'] for entry in checks})
if a.verify:
    definitions = {}
    for element in root.iter():
        if local(element) == 'Target':
            definitions[element.attrib.get('Name', '').lower()] = element  # final definition wins
    expected = ET.parse(a.guard).getroot()
    expected_names = set()
    for wanted in expected:
        name = wanted.attrib['Name']
        actual = definitions.get(name.lower())
        if actual is None or actual.attrib != wanted.attrib:
            raise SystemExit('Signing guard absent or overridden')
        children = list(actual)
        if len(children) != 1 or local(children[0]) != 'Error' or children[0].attrib != list(wanted)[0].attrib:
            raise SystemExit('Signing guard Error absent or conditional')
        expected_names.add(actual.attrib['BeforeTargets'])
    if expected_names != set(names):
        raise SystemExit('Signing target coverage changed after guard import')
else:
    if a.guard.exists():
        raise SystemExit('Guard destination already exists')
    guard_root = ET.Element(qualify('Project'))
    for i, name in enumerate(names):
        guard_name = 'SprigRejectSigningTarget_' + str(i)
        if any(local(e) == 'Target' and e.attrib.get('Name', '').lower() == guard_name.lower() for e in root.iter()):
            raise SystemExit('Guard target name collision')
        target = ET.SubElement(guard_root, qualify('Target'), Name=guard_name, BeforeTargets=name)
        ET.SubElement(target, qualify('Error'), Text='Build-only policy refuses signing/certificate target: ' + name)
    ET.ElementTree(guard_root).write(a.guard, encoding='utf-8', xml_declaration=True)
a.report.write_text(json.dumps(dict(signMode='Off', guardedTargets=names, checks=checks, verified=a.verify), indent=2))
print('Signing execution guards', 'verified' if a.verify else 'prepared', len(names))
