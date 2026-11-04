#!/usr/bin/env python3
"""Every script in the tree is reached from a make target, or is a public tool.
Called by: make lint.

    scripts/lint-scripts.py [-v]

A script is a tracked file that is executable, ends in .sh, .py or .mjs, or is
a Makefile or .mk, outside third_party, crates/*/src, the page's and runtime's module sources
(page/src, page/runtime/src, hosts/browser/js's modules) and test/ (run.mjs's own
tree). It passes when one of these holds:
  - the Makefile names it;
  - a file that is reached names it (its path from the tree's root, its path
    from the naming file's directory, or its file name when no other tracked
    file has that name), transitively from the Makefile;
  - scripts/public-tools lists it (a tool other repos or users call by path);
  - a cargo test names it (crates/*/tests/*.rs: fixture builders).
A Makefile counts as named when its directory is (make -C DIR), and a Python
module when a script beside it imports it. Executables with binary content
(test fixtures) are not scripts.
Exit 1, with the list, when any script passes none. -v prints each script's
first caller.
"""
import os, re, subprocess, sys

root = subprocess.run(['git', 'rev-parse', '--show-toplevel'], capture_output=True, text=True,
                      check=True).stdout.strip()
os.chdir(root)
files = subprocess.run(['git', 'ls-files', '-s'], capture_output=True, text=True,
                       check=True).stdout.splitlines()
mode = {}
for line in files:
    m, _, _, path = line.split(None, 3)
    mode[path] = m

SOURCE = re.compile(r'^(third_party/|crates/[^/]+/src/|page/src/|page/runtime/src/|test/)')
def is_script(p):
    if SOURCE.match(p):
        return False
    if p.startswith('hosts/browser/js/') and not os.path.basename(p).startswith('check-'):
        return False
    if p.endswith(('.sh', '.py', '.mjs', '.mk')) or (os.path.basename(p) == 'Makefile' and p != 'Makefile'):
        return True
    if mode[p] != '100755':
        return False
    with open(p, 'rb') as f:
        return b'\0' not in f.read(4096)  # an executable fixture (.exe, .elf) is no script

scripts = sorted(p for p in mode if is_script(p))
names = {}
for p in mode:
    names.setdefault(os.path.basename(p), []).append(p)

def read(p):
    try:
        with open(p, encoding='utf-8', errors='replace') as f:
            return f.read()
    except (IsADirectoryError, FileNotFoundError):
        return ''

def named_in(text, by, s):
    if s in text:
        return True
    if os.path.basename(s) == 'Makefile' and re.search(r'(^|[\s/])' + re.escape(os.path.dirname(s)) + r'\b', text):
        return True  # make -C DIR
    stem = os.path.basename(s)[:-3]
    if s.endswith('.py') and by.endswith('.py') and os.path.dirname(s) == os.path.dirname(by) \
            and re.search(r'^\s*(import|from)\s+' + re.escape(stem) + r'\b', text, re.M):
        return True  # a module beside its importer
    rel = os.path.relpath(s, os.path.dirname(by) or '.')
    if not rel.startswith('..') and re.search(r'(^|[\s"\'/=(])' + re.escape(rel) + r'\b', text):
        return True
    b = os.path.basename(s)
    return len(names[b]) == 1 and len(b) > 4 and re.search(r'\b' + re.escape(b) + r'\b', text) is not None

public = [l.split()[0] for l in read('scripts/public-tools').splitlines()
          if l.strip() and not l.startswith('#')]
reached = {}
todo = ['Makefile']
for p in public:
    reached[p] = 'scripts/public-tools'
    todo.append(p)
for p in mode:
    if re.match(r'crates/[^/]+/tests/[^/]+\.rs$', p):
        todo.append(p)
seen = set()
while todo:
    by = todo.pop()
    if by in seen:
        continue
    seen.add(by)
    text = read(by)
    for s in scripts:
        if s not in reached and s != by and named_in(text, by, s):
            reached[s] = by
            todo.append(s)

bad = [s for s in scripts if s not in reached]
if '-v' in sys.argv:
    for s in scripts:
        print(f'{s}: {reached.get(s, "UNREACHED")}')
for s in bad:
    print(f'lint-scripts: {s}: no make target, no caller reached from one, not a public tool')
print(f'lint-scripts: {len(scripts) - len(bad)} of {len(scripts)} scripts reached'
      + ('' if not bad else f', {len(bad)} not'))
sys.exit(1 if bad else 0)
