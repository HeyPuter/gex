// The basics under Node, for a fast loop: node web/runtime/test/node-basic.mjs BB.tar
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const sys = new System({ baseUrl: pathToFileURL(new URL('../../public/', import.meta.url).pathname).href, debug: { console: !!process.env.RT_CONSOLE, strace: !!process.env.RT_STRACE } });
const fs = new Filesystem(sys);
fs.addTar(readFileSync(process.argv[2]));
await sys.boot();
console.log('booted');
const env = { PATH: '/bin:/usr/bin' };
for (let i = 0; i < Number(process.env.RT_N || 3); i++) {
  const r = await sys.run(['sh', '-c', 'echo out; echo err >&2; exit 3'], { env });
  console.log(JSON.stringify(r));
}
await sys.shutdown();
process.exit(0);
