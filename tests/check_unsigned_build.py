"""Fail closed before Build: inspect actual imported signing task conditions.

This does not execute MSBuild targets or alter the toolchain/signature policy.
Only simple necessary AND terms with the immutable global SignMode are accepted.
"""
import argparse
import json
import re
import xml.etree.ElementTree as ET
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('xml', type=Path)
p.add_argument('report', type=Path)
a = p.parse_args()
if a.xml.stat().st_size > 64 * 1024 * 1024:
    raise SystemExit('Unexpected preprocessed project size')
root = ET.parse(a.xml).getroot()
local = lambda e: e.tag.rsplit('}', 1)[-1]

def disabled(condition):
    # Parse the complete deliberately tiny grammar, never search inside string
    # literals. Unknown syntax (functions, OR, NOT, unquoted atoms) fails closed.
    token = re.compile(r"\s*(?:('(?:[^']*)'|\"(?:[^\"]*)\")|(==|!=)|(\bAnd\b)|([()]))", re.I)
    tokens = []
    offset = 0
    while offset < len(condition):
        if not condition[offset:].strip():
            break
        match = token.match(condition, offset)
        if not match:
            return False
        tokens.append((match.lastindex, match.group(match.lastindex)))
        offset = match.end()
    pos = 0

    def term():
        nonlocal pos
        if pos < len(tokens) and tokens[pos] == (4, '('):
            pos += 1
            result = expression()
            if pos >= len(tokens) or tokens[pos] != (4, ')'):
                raise ValueError()
            pos += 1
            return result
        if pos + 2 >= len(tokens) or [x[0] for x in tokens[pos:pos+3]] != [1, 2, 1]:
            raise ValueError()
        left, op, right = tokens[pos:pos+3]
        pos += 3
        left, right = left[1][1:-1].lower(), right[1][1:-1].lower()
        if right == '$(signmode)':
            left, right = right, left
        if left != '$(signmode)' or right not in ('off', 'testsign', 'productionsign'):
            return None
        equal = right == 'off'
        return equal if op[1] == '==' else not equal

    def expression():
        nonlocal pos
        result = term()
        while pos < len(tokens) and tokens[pos][0] == 3:
            pos += 1
            rhs = term()
            result = False if result is False or rhs is False else (True if result is True and rhs is True else None)
        return result

    try:
        result = expression()
        return pos == len(tokens) and result is False
    except ValueError:
        return False

checks = []
for e in root.iter():
    if 'signmode' in e.attrib.get('TreatAsLocalProperty', '').lower():
        raise SystemExit('SignMode may not become a local property')
    for key in ('Properties', 'RemoveProperties', 'GlobalPropertiesToRemove', 'AdditionalProperties'):
        if 'signmode' in e.attrib.get(key, '').lower():
            raise SystemExit('Child project may change the global signing policy')
for target in root.iter():
    if local(target) != 'Target':
        continue
    for task in target:
        name = local(task)
        command = task.attrib.get('Command', '')
        dangerous = bool(re.search(r'cert|sign|inf2cat|makecat', name, re.I))
        if name.startswith('Assign') and 'cert' not in name.lower():
            dangerous = False  # AssignTargetPath etc. are not signing tasks.
        if name == 'Exec':
            dangerous |= bool(re.search(r'signtool|makecert|certutil|new-selfsignedcertificate|\.pfx\b', command, re.I))
        if not dangerous:
            continue
        condition = target.attrib.get('Condition', '')
        task_condition = task.attrib.get('Condition', '')
        safe = disabled(condition) or disabled(task_condition)
        checks.append(dict(target=target.attrib.get('Name'), task=name,
                           condition=condition, taskCondition=task_condition,
                           disabledByGlobalOff=safe))
a.report.write_text(json.dumps(dict(signMode='Off', checks=checks), indent=2))
if any(not item['disabledByGlobalOff'] for item in checks):
    raise SystemExit('Unproven signing/certificate path; Build was not started')
print('Signing preflight passed; inspected sensitive tasks:', len(checks))
