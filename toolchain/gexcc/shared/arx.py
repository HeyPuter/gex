import sys, os
# extract every member of an ar archive under a unique name (musl's libc.a repeats basenames)
data = open(sys.argv[1], 'rb').read(); out = sys.argv[2]; assert data[:8] == b'!<arch>\n'
p = 8; names = None; k = 0
while p < len(data):
    h = data[p:p+60]; name = h[:16].decode().rstrip(); size = int(h[48:58]); body = data[p+60:p+60+size]
    p += 60 + size + (size & 1)
    if name in ('/', '/SYM64/'): continue
    if name == '//': names = body; continue
    if name.startswith('/') and names is not None:
        o = int(name[1:]); name = names[o:names.index(b'\n', o)].decode()
    name = name.rstrip('/')
    k += 1; open(os.path.join(out, '%04d-%s' % (k, os.path.basename(name))), 'wb').write(body)
