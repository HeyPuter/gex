#!/usr/bin/env python3
"""Kill leftover processes of *this project* whose cmdline contains a pattern.

    killmatch.py <pattern> [pattern...]      # ours only (default)
    killmatch.py --any <pattern> [...]       # anything matching

pkill's exit status kills the calling tool chain in this environment, so this
reads /proc instead. It also refuses to kill its own ancestors: the shell that
invoked this often has the pattern in its own command line -- a heredoc that
writes the very script being matched -- and killing it takes the whole run
down, which looks exactly like the command failing.

**And it refuses, by default, to kill anything that does not look like ours.**
This runs on somebody's desktop. `killmatch.py chromium` used to mean "SIGKILL
every process whose command line contains the word chromium", which is the
browser a person is reading with as surely as it is a leftover headless one --
and tests/gates/gpu-exclusive.sh printed that exact command as advice. A
process is ours if its command line carries one of OURS below; `--any` says
you mean it anyway.
"""
import os, sys, signal

# Markers this project's own processes carry: the headless browser it drives,
# its driver script, the CTS runners, and anything run out of its build or
# scratch directories.
OURS = ('--headless', 'ozone-platform=headless', 'drive.mjs', 'serve-dist.mjs',
        'glcts', 'deqp-gl', '/tmp/gex-', 'gex_wgpu', 'GALLIUM_DRIVER')

def ancestry(pid):
    """Every pid from `pid` up to init."""
    out = set()
    while pid and pid not in out:
        out.add(pid)
        try:
            with open(f'/proc/{pid}/stat', 'rb') as f:
                fields = f.read().rsplit(b') ', 1)[1].split()
            pid = int(fields[1])
        except (OSError, IndexError, ValueError):
            break
    return out

argv = sys.argv[1:]
any_process = '--any' in argv
pats = [a for a in argv if a != '--any' and a != '--']
if not pats:
    print(__doc__.strip())
    sys.exit(2)
spare = ancestry(os.getpid())
killed = 0
spared = 0
for entry in os.listdir('/proc'):
    if not entry.isdigit():
        continue
    pid = int(entry)
    if pid in spare:
        continue
    try:
        cmd = open(f'/proc/{pid}/cmdline', 'rb').read().decode('utf8', 'replace')
    except OSError:
        continue
    if any(p in cmd for p in pats):
        if not any_process and not any(m in cmd for m in OURS):
            spared += 1
            continue
        try:
            os.kill(pid, signal.SIGKILL)
            killed += 1
        except OSError:
            pass
print(f'killed {killed}')
if spared:
    print(f'spared {spared} that do not look like ours -- pass --any to '
          f'include them, but read the list first:')
    for entry in os.listdir('/proc'):
        if not entry.isdigit():
            continue
        try:
            cmd = open(f'/proc/{entry}/cmdline', 'rb').read().decode(
                'utf8', 'replace').replace('\0', ' ').strip()
        except OSError:
            continue
        if cmd and any(p in cmd for p in pats) and \
           not any(m in cmd for m in OURS):
            print(f'  {entry}: {cmd[:110]}')
