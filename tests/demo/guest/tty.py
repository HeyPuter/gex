# Terminals and fork, in the page (tests/demo/guest.sh). Exits non-zero on
# the first thing that is not as Linux has it.
import os, pty, select, time, sys

def check(what, ok, got):
    print(f"{'ok' if ok else 'FAIL'} {what}: {got}", flush=True)
    if not ok:
        sys.exit(1)

# An epoll set made before fork, waited on by the child.
r, w = os.pipe()
e = select.epoll()
e.register(r, select.EPOLLIN)
pid = os.fork()
if pid == 0:
    try:
        ev = e.poll(10)
        res = f'{ev} {os.read(r, 10)!r}'
        e.close()
    except BaseException as ex:
        res = f'error {ex!r}'
    open('/tmp/tty-epoll.txt', 'w').write(res)
    os._exit(0)
time.sleep(1)
os.write(w, b'ping')
os.waitpid(pid, 0)
got = open('/tmp/tty-epoll.txt').read()
check('epoll set inherited across fork', got == f"[({r}, 1)] b'ping'", got)

# A raw slave read, the way a full-screen program reads keys.
import tty
m, s = pty.openpty()
tty.setraw(s)
os.write(m, b'k')
check('raw pty read', os.read(s, 4) == b'k', 'k')

# ttyname, /dev/pts and /proc/self/fd agree.
name = os.ttyname(s)
check('ttyname', name.startswith('/dev/pts/'), name)
check('/dev/pts lists it', name.rsplit('/', 1)[1] in os.listdir('/dev/pts'), sorted(os.listdir('/dev/pts')))
check('/proc/self/fd names it', os.readlink(f'/proc/self/fd/{s}') == name, os.readlink(f'/proc/self/fd/{s}'))

# A shell on a pty: tty(1), and ps's TTY column from /proc/<pid>/stat.
pid, mfd = pty.fork()
if pid == 0:
    os.execvp('sh', ['sh', '-c', 'tty; ps -o tty=,comm='])
out = b''
end = time.time() + 20
while time.time() < end and out.count(b'\n') < 3:
    rr, _, _ = select.select([mfd], [], [], 0.3)
    if rr:
        try:
            out += os.read(mfd, 65536)
        except OSError:
            break
os.waitpid(pid, 0)
lines = out.decode(errors='replace').split()
check('tty(1) on a pty', bool(lines) and lines[0].startswith('/dev/pts/'), lines[:1])
check("ps's TTY column", 'pts/' in out.decode(errors='replace').split('\n', 1)[1], out.decode(errors='replace').strip().replace('\r', ''))
