# A Wayland client whose window holds a subsurface with an empty input
# region, the way Wine shows a window's GL picture: the pointer over the
# subsurface belongs to the window under it, in the window's coordinates.
#
# The window (xdg_toplevel "inputregion") is W x H light grey; the subsurface
# is SW x SH dark at SX, SY with an empty input region. Prints a line per
# pointer event -- "enter parent|sub X Y", "motion X Y", "button parent|sub
# X Y" -- with X, Y in surface units of the surface the pointer is on, and
# "ready" once both are shown. Exits after the first button release, or after
# argv[1] seconds (default 120).
#
# Raw wire protocol, no libraries: the demo's python has no Wayland binding.
import mmap, os, socket, struct, sys, time

W, H, SX, SY, SW, SH = 240, 160, 40, 30, 120, 80
deadline = time.time() + float(sys.argv[1] if len(sys.argv) > 1 else 120)

path = os.environ.get('WAYLAND_DISPLAY', 'wayland-0')
if not path.startswith('/'):
    path = os.path.join(os.environ.get('XDG_RUNTIME_DIR', '/tmp'), path)
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)

next_id = [2]
def new():
    i = next_id[0]; next_id[0] += 1; return i

def string(s):
    b = s.encode() + b'\0'
    return struct.pack('<I', len(b)) + b + b'\0' * (-len(b) % 4)

def send(obj, op, args=b'', fds=()):
    msg = struct.pack('<II', obj, (8 + len(args)) << 16 | op) + args
    if fds:
        socket.send_fds(sock, [msg], list(fds))
    else:
        sock.sendall(msg)

buf = b''
def events():
    """Every event that has arrived, as (object, opcode, payload)."""
    global buf
    data = sock.recv(65536)
    if not data:
        sys.exit('inputregion: the compositor hung up')
    buf += data
    while len(buf) >= 8:
        obj, w = struct.unpack_from('<II', buf)
        size = w >> 16
        if len(buf) < size:
            break
        yield obj, w & 0xffff, buf[8:size]
        buf = buf[size:]

registry = new()
send(1, 1, struct.pack('<I', registry))       # wl_display.get_registry
cb = new()
send(1, 0, struct.pack('<I', cb))             # wl_display.sync
globals_ = {}
done = False
while not done:
    for obj, op, p in events():
        if obj == registry and op == 0:
            name, n = struct.unpack_from('<II', p)
            iface = p[8:8 + n - 1].decode()
            (ver,) = struct.unpack_from('<I', p, 8 + n + (-n % 4))
            globals_[iface] = (name, ver)
        elif obj == cb:
            done = True

def bind(iface, ver):
    name, have = globals_[iface]
    i = new()
    send(registry, 0, struct.pack('<I', name) + string(iface) + struct.pack('<II', min(ver, have), i))
    return i

compositor = bind('wl_compositor', 4)
subcompositor = bind('wl_subcompositor', 1)
shm = bind('wl_shm', 1)
wm = bind('xdg_wm_base', 1)
seat = bind('wl_seat', 5)

def surface():
    i = new(); send(compositor, 0, struct.pack('<I', i)); return i

# The pools stay mapped: the compositor reads a buffer where its client has
# the pool mapped, as it does for every client.
pools = []
def buffer(w, h, grey):
    fd = os.memfd_create('inputregion')
    size = w * h * 4
    os.ftruncate(fd, size)
    m = mmap.mmap(fd, size)
    m.write(bytes((grey, grey, grey, 255)) * (w * h))
    pools.append(m)
    pool = new()
    send(shm, 0, struct.pack('<II', pool, size), fds=(fd,))        # create_pool(id, fd, size)
    os.close(fd)
    b = new()
    send(pool, 0, struct.pack('<IiiiiI', b, 0, w, h, w * 4, 1))     # create_buffer, XRGB8888
    return b

parent = surface()
xdg = new(); send(wm, 2, struct.pack('<II', xdg, parent))          # get_xdg_surface
top = new(); send(xdg, 1, struct.pack('<I', top))                  # get_toplevel
send(top, 2, string('inputregion'))                                # set_title
send(parent, 6)                                                    # commit, for the configure

sub = surface()
subsurface = new()
send(subcompositor, 1, struct.pack('<III', subsurface, sub, parent))   # get_subsurface
send(subsurface, 1, struct.pack('<ii', SX, SY))                        # set_position
send(subsurface, 5)                                                    # set_desync
region = new()
send(compositor, 1, struct.pack('<I', region))                     # create_region: empty
send(sub, 5, struct.pack('<I', region))                            # set_input_region
send(region, 0)                                                    # destroy: the surface has a copy
pointer = new()
send(seat, 0, struct.pack('<I', pointer))                          # get_pointer

shown = False
on = None
names = {parent: 'parent', sub: 'sub'}
fx = lambda v: v / 256
while time.time() < deadline:
    for obj, op, p in events():
        if obj == wm and op == 0:                                  # ping
            send(wm, 3, p[:4])
        elif obj == xdg and op == 0:                               # configure
            send(xdg, 4, p[:4])                                    # ack_configure
            if not shown:
                send(sub, 1, struct.pack('<Iii', buffer(SW, SH, 40), 0, 0))
                send(sub, 2, struct.pack('<iiii', 0, 0, SW, SH))
                send(sub, 6)
                send(parent, 1, struct.pack('<Iii', buffer(W, H, 200), 0, 0))
                send(parent, 2, struct.pack('<iiii', 0, 0, W, H))
                send(parent, 6)
                shown = True
                print('ready', flush=True)
        elif obj == pointer and op == 0:                           # enter
            _serial, s, x, y = struct.unpack_from('<IIii', p)
            on = s
            print(f'enter {names.get(s, s)} {fx(x):.0f} {fx(y):.0f}', flush=True)
            last = (fx(x), fx(y))
        elif obj == pointer and op == 2:                           # motion
            _t, x, y = struct.unpack_from('<Iii', p)
            last = (fx(x), fx(y))
            print(f'motion {last[0]:.0f} {last[1]:.0f}', flush=True)
        elif obj == pointer and op == 3:                           # button
            _serial, _t, _b, state = struct.unpack_from('<IIII', p)
            print(f'button {names.get(on, on)} {last[0]:.0f} {last[1]:.0f} {"down" if state else "up"}', flush=True)
            if not state:
                sys.exit(0)
        elif obj == 1 and op == 0:                                 # wl_display.error
            o, code = struct.unpack_from('<II', p)
            sys.exit(f'inputregion: protocol error {code} on {o}')
