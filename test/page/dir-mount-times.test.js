// A `--dir` mount's folders keep their host times, as its files do: a file
// manager in the guest listed every folder at 0 (January 1970).
import { mkdirSync, writeFileSync, utimesSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 180 };

const WHEN = 1_715_000_000;

const code = `#include <stdio.h>
#include <sys/stat.h>
int main(int argc, char **argv) {
  for (int i = 1; i < argc; i++) {
    struct stat st;
    if (stat(argv[i], &st)) { printf("%s: none\\n", argv[i]); continue; }
    printf("%s %lld\\n", argv[i], (long long)st.st_mtime);
  }
  return 0;
}
`;

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const prog = g.cc({ name: 'dir-mtime', code });
  const src = fileURLToPath(t.work('dir-mount-times'));
  mkdirSync(join(src, 'a/b'), { recursive: true });
  writeFileSync(join(src, 'a/b/f'), 'hi\n');
  for (const d of ['a/b', 'a', '.']) utimesSync(join(src, d), WHEN, WHEN);
  const r = await g.run(g.seed(), prog, ['/data', '/data/a', '/data/a/b'], { flags: ['--dir', `/data=${src}`], env: { GEX_SHARED: '1' } });
  t.eq('each folder has its host time', r.guest, `/data ${WHEN}\n/data/a ${WHEN}\n/data/a/b ${WHEN}`, tail(r.stderr, 3));
}
