# Keys typed into nvim on a pty reach it (tests/demo/guest.sh): libuv opens
# the terminal non-blocking (FIONBIO), puts it in raw mode and reads it.
import os, pty, select, sys, time

path = '/tmp/guest-nvim.txt'
try:
    os.unlink(path)
except FileNotFoundError:
    pass
pid, m = pty.fork()
if pid == 0:
    os.environ['TERM'] = 'xterm-256color'
    os.execvp('nvim', ['nvim', '--clean', path])
out = b''

def drain(sec):
    global out
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.2)
        if r:
            try:
                out += os.read(m, 65536)
            except OSError:
                return

drain(15)
os.write(m, b'ihello')
drain(3)
os.write(m, b'\x1b')
drain(2)
os.write(m, b':wq\r')
drain(8)
try:
    got = open(path).read()
except OSError as e:
    got = repr(e)
ok = got == 'hello\n'
print(f"{'ok' if ok else 'FAIL'} nvim took typed keys: {got!r} (drew {len(out)} bytes)", flush=True)
sys.exit(0 if ok else 1)
