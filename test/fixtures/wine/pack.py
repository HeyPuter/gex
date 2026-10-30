"""Pack trees for a page: the small files, directories and links into one
tar, every bigger file as a lazy entry the page fetches when it is first read.

    python3 pack.py OUT.tar OUT-lazy.json BASE_REL ROOT=AT[,LINK] ...

ROOT is laid at AT in the guest. A lazy file's URL is BASE_REL/LINK/RELPATH,
relative to the list (LINK is a name in the work directory standing for ROOT,
default AT's last part). Files of LIMIT bytes or less (default 64 KiB) go in
the tar.
"""
import json, os, sys, tarfile

def whole_seconds(info):
    # Whole seconds, as the host's stat says them: a fraction in a pax header
    # was rounded up by the reader, and Wine compares wine.inf's time with
    # the one the prefix recorded (.update-timestamp) to the second.
    info.mtime = int(info.mtime)
    return info


LIMIT = int(os.environ.get('LIMIT', 65536))
out, lazy_out, base = sys.argv[1:4]
files = []
with tarfile.open(out, 'w', format=tarfile.PAX_FORMAT) as tar:
    for spec in sys.argv[4:]:
        root, at = spec.split('=', 1)
        at, _, link = at.partition(',')
        link = link or at.rstrip('/').rsplit('/', 1)[-1]
        at = at.rstrip('/')
        for d, dirs, names in os.walk(root):
            dirs.sort()
            rel = os.path.relpath(d, root)
            rel = '' if rel == '.' else rel
            for n in sorted(dirs + names):
                p = os.path.join(d, n)
                r = os.path.join(rel, n)
                g = f'{at}/{r}'.lstrip('/')
                st = os.lstat(p)
                if os.path.islink(p) or os.path.isdir(p) or st.st_size <= LIMIT:
                    tar.add(p, arcname=g, recursive=False, filter=whole_seconds)
                else:
                    files.append([f'/{g}', st.st_size, f'{link}/{r}', st.st_mode & 0o7777])
            # A followed link to a directory is walked as itself: no.
            dirs[:] = [x for x in dirs if not os.path.islink(os.path.join(d, x))]
with open(lazy_out, 'w') as f:
    json.dump({'base': base, 'files': files}, f)
print(len(files), sum(x[1] for x in files))
