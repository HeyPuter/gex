/**
 * Does each global kernel instance get thread-locals of its own?
 *
 * Every instance on the shared kernel memory starts with the same
 * `__stack_pointer`, and wasm-ld's start function sets `__tls_base` only in
 * the instance that initialises the memory: every other one had base 0, so
 * all later Workers shared one set of thread-locals (std's panic count and
 * `thread::current`, HashMap's keys) at address 0. `kernelThread` gives an
 * instance a block at the top of its stack slice. This makes two instances,
 * gives the second its slice, has it use its thread-locals (a lazy file: the
 * tmpfs's HashMap reads its keys) and checks the bytes moved in its own block
 * and not at address 0.
 *
 * Usage: node check-kernel-tls.mjs [gex_kernel.wasm]
 */
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { kernelThread } from './lazy-fetch.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const path = process.argv[2] || join(HERE, '..', '..', '..', 'web', 'public', 'gex', 'gex_kernel.x86.wasm');
const module = await WebAssembly.compile(await readFile(path));
const memory = new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true });
const proc = new WebAssembly.Memory({ initial: 1, maximum: 65535, shared: true });
const env = {
  memory,
  process: proc,
  lazy_fetch: () => 0, page_fs: () => -5,
  process64: new WebAssembly.Memory({ address: 'i64', initial: 0n, maximum: 0n, shared: true }),
};
const first = (await WebAssembly.instantiate(module, { env })).exports;
const area = first.k_stack_area() >>> 0;
const size = first.k_stack_size() >>> 0;
const second = (await WebAssembly.instantiate(module, { env })).exports;
if (!second.__tls_size || typeof second.__wasm_init_tls !== 'function') {
  console.log('check-kernel-tls: FAIL (the kernel exports no __tls_size/__wasm_init_tls: rebuild it)');
  process.exit(1);
}
const block = kernelThread(second, area + 2 * size);
const bytes = new Uint8Array(memory.buffer);
const at = (a) => bytes.slice(a, a + (second.__tls_size.value >>> 0)).join();
const [low0, own0] = [at(0), at(block)];
const text = new Uint8Array(proc.buffer);
text.set(new TextEncoder().encode('/lazy'), 16);
text.set(new TextEncoder().encode('http://x/y'), 64);
second.k_fs_add_lazy(16n, 5, 64n, 10, 100n);
const lowMoved = at(0) !== low0, ownMoved = at(block) !== own0;
const ok = !lowMoved && ownMoved;
console.log(`check-kernel-tls: ${ok ? 'ok' : 'FAIL'} (block at 0x${block.toString(16)}: ${ownMoved ? 'used' : 'unused'}; address 0 ${lowMoved ? 'written' : 'untouched'})`);
process.exit(ok ? 0 : 1);
