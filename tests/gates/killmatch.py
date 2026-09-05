#!/usr/bin/env python3
"""Kill processes whose cmdline contains any argument.

pkill's exit status kills the calling tool chain in this environment, so this
reads /proc instead. It also refuses to kill its own ancestors: the shell that
invoked this often has the pattern in its own command line -- a heredoc that
writes the very script being matched -- and killing it takes the whole run
down, which looks exactly like the command failing.
"""
import os, sys, signal

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

pats = sys.argv[1:]
spare = ancestry(os.getpid())
killed = 0
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
        try:
            os.kill(pid, signal.SIGKILL)
            killed += 1
        except OSError:
            pass
print(f'killed {killed}')
