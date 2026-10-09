#!/usr/bin/env python3
"""apt's programs and methods as one gexcc program (docs/gexcc.md §6.1a).

    aptmulti.py BUILDDIR OUT

Each of apt's 20 programs links all of libapt-pkg (its static registries
reach most of it): about 10 MB of wasm each, 100 MB for the set, and a
separate module for the page to compile per program. Here each program's
main source is compiled again with `main` renamed, a dispatcher picks one by
the name it was run as (busybox's way), and the lot links once. BUILDDIR is
apt's CMake build tree (configured with CMAKE_EXPORT_COMPILE_COMMANDS);
the link line comes from the http method's (it has every library).

Installed as /usr/lib/apt/apt-multi with every program and method a symlink.
"""
import json, os, re, shlex, subprocess, sys

# (id, source under the source tree, what argv[0] names it)
PROGRAMS = [
    ('apt', 'cmdline/apt.cc', ['bin/apt']),
    ('apt_cache', 'cmdline/apt-cache.cc', ['bin/apt-cache']),
    ('apt_get', 'cmdline/apt-get.cc', ['bin/apt-get']),
    ('apt_mark', 'cmdline/apt-mark.cc', ['bin/apt-mark']),
    ('apt_config', 'cmdline/apt-config.cc', ['bin/apt-config']),
    ('apt_cdrom', 'cmdline/apt-cdrom.cc', ['bin/apt-cdrom']),
    ('apt_helper', 'cmdline/apt-helper.cc', ['apt-helper']),
    ('apt_extracttemplates', 'cmdline/apt-extracttemplates.cc', ['apt-extracttemplates']),
    ('internal_solver', 'cmdline/apt-internal-solver.cc', ['solvers/apt', 'solvers/solver3']),
    ('dump_solver', 'cmdline/apt-dump-solver.cc', ['solvers/dump', 'planners/dump']),
    ('internal_planner', 'cmdline/apt-internal-planner.cc', ['planners/apt']),
    ('m_file', 'methods/file.cc', ['methods/file']),
    ('m_copy', 'methods/copy.cc', ['methods/copy']),
    ('m_store', 'methods/store.cc', ['methods/store']),
    ('m_gpgv', 'methods/gpgv.cc', ['methods/gpgv']),
    ('m_cdrom', 'methods/cdrom.cc', ['methods/cdrom']),
    ('m_http', 'methods/http.cc', ['methods/http', 'methods/https']),
    ('m_mirror', 'methods/mirror.cc', ['methods/mirror', 'methods/mirror+http', 'methods/mirror+https',
                                      'methods/mirror+file', 'methods/mirror+copy']),
    ('m_rred', 'methods/rred.cc', ['methods/rred']),
]
# objects the http method links besides its main source
HTTP_EXTRA = ['methods/CMakeFiles/http.dir/basehttp.cc.o', 'methods/CMakeFiles/connectlib.dir/connect.cc.o',
              'methods/CMakeFiles/connectlib.dir/rfc2553emu.cc.o']

DISPATCH = r'''
// apt-multi: which of apt's programs to run, from the name it was run as
#include <cstring>
#include <cstdio>
%(decls)s
static const struct { const char *name; int (*fn)(int, const char **); } table[] = {
%(rows)s
};
int main(int argc, const char **argv) {
  const char *a0 = argc > 0 ? argv[0] : "";
  size_t n = strlen(a0);
  // longest suffix match on a path component boundary ("solvers/apt" before "apt")
  int best = -1; size_t bestlen = 0;
  for (size_t i = 0; i < sizeof table / sizeof *table; i++) {
    size_t l = strlen(table[i].name);
    if (l <= n && strcmp(a0 + n - l, table[i].name) == 0 && (l == n || a0[n - l - 1] == '/') && l > bestlen) {
      best = (int)i; bestlen = l;
    }
  }
  if (best < 0) {
    fprintf(stderr, "apt-multi: run as one of apt's programs, not as %%s\n", a0);
    return 127;
  }
  return table[best].fn(argc, argv);
}
'''


def main():
    build, out = sys.argv[1], sys.argv[2]
    src = os.path.dirname(os.path.abspath(build.rstrip('/')))
    cc = {os.path.normpath(e['file']): e for e in json.load(open(os.path.join(build, 'compile_commands.json')))}
    mdir = os.path.join(build, 'multi')
    os.makedirs(mdir, exist_ok=True)
    objs, decls, rows = [], [], []
    for ident, rel, names in PROGRAMS:
        e = cc[os.path.normpath(os.path.join(src, rel))]
        args = shlex.split(e['command'])
        i = args.index('-o')
        o = os.path.join(mdir, ident + '.o')
        args[i + 1] = o
        args.insert(1, f'-Dmain=apt_multi_{ident}')
        r = subprocess.run(args, cwd=e['directory'])
        if r.returncode:
            sys.exit(f'aptmulti: {rel} failed')
        objs.append(o)
        # the renamed main keeps its C++ signature (and so its mangled name):
        # declare it as written, call it through an adapter
        sig = re.search(r'^int main\((.*?)\)', open(os.path.join(src, rel)).read(), re.M)[1]
        if not sig.strip():
            decls.append(f'int apt_multi_{ident}();\n'
                         f'static int run_{ident}(int, const char **) {{ return apt_multi_{ident}(); }}')
        elif 'const' in sig:
            decls.append(f'int apt_multi_{ident}(int, const char **);\n'
                         f'static int run_{ident}(int c, const char **v) {{ return apt_multi_{ident}(c, v); }}')
        else:
            decls.append(f'int apt_multi_{ident}(int, char **);\n'
                         f'static int run_{ident}(int c, const char **v) {{ return apt_multi_{ident}(c, (char **)v); }}')
        # programs in /usr/bin are run by name from PATH: match the name alone
        rows += [f'  {{"{n[4:] if n.startswith("bin/") else n}", run_{ident}}},' for n in names]
    d = os.path.join(mdir, 'dispatch.cc')
    open(d, 'w').write(DISPATCH % {'decls': '\n'.join(decls), 'rows': '\n'.join(rows)})
    link = shlex.split(open(os.path.join(build, 'methods/CMakeFiles/http.dir/link.txt')).read())
    i = link.index('-o')
    cxx, rest = link[0], link[i + 2:]
    libs = [a for a in rest if not a.startswith('-Wl,-rpath')]
    # libapt-private before libapt-pkg (the cmdline programs need it)
    libs = [os.path.join(build, 'apt-private/libapt-private.a') if a == '../apt-pkg/libapt-pkg.a' else a for a in libs]
    libs.insert(libs.index(os.path.join(build, 'apt-private/libapt-private.a')) + 1,
                os.path.join(build, 'apt-pkg/libapt-pkg.a'))
    cflags = [a for a in link[1:i] if a.startswith('-') and not a.endswith('.o')]
    cmd = [cxx] + cflags + [d] + objs + [os.path.join(build, x) for x in HTTP_EXTRA] + ['-o', out] + libs
    r = subprocess.run(cmd, cwd=os.path.join(build, 'methods'))
    if r.returncode:
        sys.exit('aptmulti: link failed')
    for ident, rel, names in PROGRAMS:
        for n in names:
            print(n)


if __name__ == '__main__':
    main()
