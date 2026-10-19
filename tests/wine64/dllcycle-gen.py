#!/usr/bin/env python3
# dllcycle-gen.py N OUT.c: a DLL source with N distinct small functions run in a row by its export
# `work` (dllcycle.c's plugins: code that must be translated, a page or more per few functions).
# dllcycle-gen.py N --expect CALLS: what dllcycle.c prints as x after CALLS calls of `work` from x = 1.
import sys
n = int(sys.argv[1])
if sys.argv[2] == '--expect':
    M = 0xffffffff
    x = 1
    for _ in range(int(sys.argv[3])):
        for i in range(n):
            x = (x * (2 * i + 3) + i * 7 + 1) & M
            if x & (1 << (i % 16)): x ^= x >> (i % 13 + 1)
            else: x = (x + i * 31 + 5) & M
            k = 0
            while k < (x & 3):
                x = ((x << 3) ^ (x >> (i % 7 + 2)) ^ i) & M
                k += 1
    print(x)
    sys.exit(0)
out = ['#include <windows.h>', 'static volatile unsigned sink;']
for i in range(n):
    out.append(f'__attribute__((noinline)) static unsigned f{i}(unsigned x) {{ x = x * {2*i+3}u + {i*7+1}u; '
               f'if (x & {1 << (i % 16)}u) x ^= x >> {i % 13 + 1}; else x += {i*31+5}u; '
               f'for (int k = 0; k < (int)(x & 3); k++) x = (x << 3) ^ (x >> {i % 7 + 2}) ^ {i}u; return x; }}')
out.append('__declspec(dllexport) unsigned work(unsigned x) {')
out += [f' x = f{i}(x);' for i in range(n)]
out.append(' sink = x; return x; }')
out.append('BOOL WINAPI DllMain(HINSTANCE h, DWORD r, void *p) { return TRUE; }')
open(sys.argv[2], 'w').write('\n'.join(out) + '\n')
