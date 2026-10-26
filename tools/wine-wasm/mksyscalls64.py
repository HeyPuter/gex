#!/usr/bin/env python3
"""Write dlls/ntdll/syscall_wasm64.c for a 64-bit Wine tree (stdout).

    tools/wine-wasm/mksyscalls64.py WINE_SRC > WINE_SRC/dlls/ntdll/syscall_wasm64.c

Native PE ntdll on wasm64 defines each NT
system call as a C function that hands its arguments, as 64-bit words, to
gexrt64's __gexw_syscall64. wasm links a call to a function only at its
exact type (NtWaitForSingleObject is (i64, i32, i64) -> i32), so every stub
is written with the parameter types of its declaration in Wine's headers,
which clang reads here (its AST, for x86_64-w64-mingw32: the type names are
what matter, not their sizes). The stubs are checked against those
declarations when ntdll compiles.
"""
import json
import os
import re
import subprocess
import sys
import tempfile

HEADERS = ['<stdarg.h>', '"ntstatus.h"', None, '"windef.h"', '"winnt.h"', '"winbase.h"',
           '"winternl.h"', '"ddk/wdm.h"']

# Written in signal_wasm64.c: the calls that load a new guest state and
# NtQueryPerformanceCounter (no system call).
SPECIAL = {'NtContinue', 'NtContinueEx', 'NtSetContextThread', 'NtQueryPerformanceCounter'}


def syscalls(wine):
    """(id, name) of ALL_SYSCALLS's 64-bit list in ntsyscalls.h."""
    text = open(os.path.join(wine, 'dlls/ntdll/ntsyscalls.h')).read()
    block = text[text.index('#ifdef _WIN64'):text.index('#else', text.index('#ifdef _WIN64'))]
    return [(int(i, 16), n) for i, n in re.findall(r'SYSCALL_ENTRY\w*\(\s*(0x[0-9a-f]+),\s*(\w+),', block)]


def stubs(wine):
    """ALL_SYSCALL_STUBS: system calls Wine does not implement (no declaration)."""
    text = open(os.path.join(wine, 'dlls/ntdll/ntsyscalls.h')).read()
    return set(re.findall(r'SYSCALL_STUB\(\s*(\w+)\s*\)', text))


def declarations(wine):
    res = subprocess.check_output(['clang', '-print-resource-dir'], text=True).strip() + '/include'
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, 't.c')
        with open(src, 'w') as f:
            for h in HEADERS:
                f.write('#define WIN32_NO_STATUS\n' if h is None else '#include %s\n' % h)
        out = subprocess.check_output(
            ['clang', '--target=x86_64-w64-mingw32', '-mms-bitfields', '-fshort-wchar', '-nostdinc',
             '-I' + wine + '/include', '-I' + wine + '/include/msvcrt', '-isystem', res,
             '-D_WIN32', '-D__WINESRC__', '-fsyntax-only', '-w', '-Xclang', '-ast-dump=json',
             '-Xclang', '-ast-dump-filter=Nt', src], text=True)
    dec = json.JSONDecoder()
    pos, decls = 0, {}
    while True:
        while pos < len(out) and out[pos].isspace():
            pos += 1
        if pos >= len(out):
            break
        node, pos = dec.raw_decode(out, pos)
        if node.get('kind') != 'FunctionDecl' or node.get('name') in decls:
            continue
        params = [(p.get('name'), p['type']['qualType'], p['type'].get('desugaredQualType', p['type']['qualType']))
                  for p in node.get('inner', []) if p.get('kind') == 'ParmVarDecl']
        ftype = node['type']['qualType']
        decls[node['name']] = (ftype[:ftype.index('(')].strip(), params, node.get('variadic', False))
    return decls


def word(name, qual, desugared):
    """The C expression for a parameter as a 64-bit word."""
    if '*' in desugared or desugared.startswith('void *') or '(' in desugared:
        return '(ULONG_PTR)%s' % name
    if desugared.split()[0] in ('struct', 'union'):
        sys.exit('mksyscalls64: a structure by value (%s %s) needs writing by hand' % (qual, name))
    if desugared in ('float', 'double'):
        sys.exit('mksyscalls64: floating point (%s %s) needs writing by hand' % (qual, name))
    return '(ULONG64)%s' % name


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    wine = sys.argv[1]
    decls = declarations(wine)
    unimplemented = stubs(wine)
    out = [HEAD]
    for _, name in syscalls(wine):
        if name in SPECIAL:
            continue
        if name not in decls and name in unimplemented:
            # no declaration to match: the Unix side's stub raises
            # EXCEPTION_WINE_STUB whatever the arguments
            out.append('NTSTATUS WINAPI %s(void);\nNTSTATUS WINAPI %s(void)\n{\n'
                       '    return __gexw_syscall64( __id_%s, 0, 0 );\n}\n\n' % (name, name, name))
            continue
        if name not in decls:
            sys.exit('mksyscalls64: %s is not declared in the headers' % name)
        ret, params, variadic = decls[name]
        if variadic:
            sys.exit('mksyscalls64: %s is variadic' % name)
        if ret not in ('NTSTATUS', 'ULONG', 'BOOLEAN'):
            sys.exit('mksyscalls64: %s returns %s' % (name, ret))
        args = ', '.join('%s a%d' % (q, i) for i, (_, q, _) in enumerate(params)) or 'void'
        out.append('%s WINAPI %s( %s )\n{\n' % (ret, name, args))
        if params:
            out.append('    const ULONG64 w[%d] = { %s };\n' % (
                len(params), ', '.join(word('a%d' % i, q, d) for i, (_, q, d) in enumerate(params))))
            out.append('    return __gexw_syscall64( __id_%s, w, %d );\n}\n\n' % (name, len(params)))
        else:
            out.append('    return __gexw_syscall64( __id_%s, 0, 0 );\n}\n\n' % name)
    out.append(TAIL)
    sys.stdout.write(''.join(out))


HEAD = '''/*
 * NT system call stubs for native Wine on wasm64 under gex (wine64): each
 * hands its arguments, as 64-bit words, to gexrt64's __gexw_syscall64,
 * which sets up the program's state as Wine's x86-64 stubs leave it and
 * calls the Unix side's dispatcher.
 *
 * Generated by the gex tree's tools/wine-wasm/mksyscalls64.py from
 * ntsyscalls.h and the declarations in Wine's headers: wasm links a call
 * only at the callee's exact type, so each stub has its declaration's.
 * NtContinue, NtContinueEx, NtSetContextThread and
 * NtQueryPerformanceCounter are in signal_wasm64.c.
 */

#ifdef __wasm64__

#include <stdarg.h>

#include "ntstatus.h"
#define WIN32_NO_STATUS
#include "windef.h"
#include "winnt.h"
#include "winbase.h"
#include "winternl.h"
#include "ddk/wdm.h"
#include "ntsyscalls.h"

enum syscall_ids
{
#define SYSCALL_ENTRY(id,name,args) __id_##name = id,
#undef SYSCALL_ENTRY_NtQueryInformationProcess
#define SYSCALL_ENTRY_NtQueryInformationProcess SYSCALL_ENTRY
ALL_SYSCALLS
#undef SYSCALL_ENTRY
#undef SYSCALL_ENTRY_NtQueryInformationProcess
    __nb_syscalls
};

extern unsigned int __gexw_syscall64( unsigned int id, const ULONG64 *words, unsigned int n );

'''

TAIL = '''#endif  /* __wasm64__ */
'''

if __name__ == '__main__':
    main()
