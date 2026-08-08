// Does this engine give us the two-memory shape the kernel design rests on?
//
//   node web/test/multimemory.mjs
//
// The plan is one *global* kernel module -- the VFS, the process table, pipes,
// signals -- instantiated once per Worker but always against the same shared
// memory, so every process's every thread sees one kernel and a syscall is a
// direct call rather than a message. Each process's address space is a second,
// separate memory: shared between that process's threads, never between
// processes. That is what removes the address collisions the snapshot switcher
// exists to work around.
//
// It rests on three things this checks, cheaply, before any of it is built:
//
//   1. a module may *import* two memories at once,
//   2. one of them may be shared and the other not,
//   3. `memory.copy` may name both, so the kernel can reach a guest buffer
//      that is not in its own address space.
//
// The module is assembled by hand rather than compiled, because the question
// is about the engine and not about Rust: if V8 refuses this, no amount of
// toolchain work will help, and if it accepts it the rest is ordinary code.
const enc = (...bytes) => bytes.flat(Infinity);

/** LEB128, unsigned. Every length and index in a wasm binary is one. */
const uleb = (n) => {
  const out = [];
  do {
    let b = n & 0x7f;
    n >>>= 7;
    if (n) b |= 0x80;
    out.push(b);
  } while (n);
  return out;
};

const str = (s) => [...uleb(s.length), ...[...s].map((c) => c.charCodeAt(0))];
const section = (id, body) => [id, ...uleb(body.length), ...body];

// (module
//   (import "e" "kernel"  (memory 1 1 shared))   ; memory 0
//   (import "e" "process" (memory 1 1))          ; memory 1
//   (func (export "pull") (param i32 i32 i32)
//     local.get 0 local.get 1 local.get 2
//     memory.copy 0 1)                           ; dst=kernel, src=process
//   (func (export "push") (param i32 i32 i32)
//     local.get 0 local.get 1 local.get 2
//     memory.copy 1 0))                          ; dst=process, src=kernel
const wasm = new Uint8Array(
  enc(
    [0x00, 0x61, 0x73, 0x6d], [0x01, 0x00, 0x00, 0x00],
    // 1: types -- one (i32,i32,i32)->() shared by both functions
    section(1, enc(uleb(1), 0x60, uleb(3), 0x7f, 0x7f, 0x7f, uleb(0))),
    // 2: imports -- the order here is the memory index space
    section(2, enc(
      uleb(2),
      str('e'), str('kernel'), 0x02, 0x03, uleb(1), uleb(1),  // 0x03: max + shared
      str('e'), str('process'), 0x02, 0x01, uleb(1), uleb(1), // 0x01: max, not shared
    )),
    // 3: functions
    section(3, enc(uleb(2), uleb(0), uleb(0))),
    // 7: exports
    section(7, enc(
      uleb(2),
      str('pull'), 0x00, uleb(0),
      str('push'), 0x00, uleb(1),
    )),
    // 10: code. `memory.copy` is 0xfc 0x0a then dst and src memory indices --
    // the pair of bytes that only exists at all with multi-memory.
    section(10, enc(
      uleb(2),
      ...[[0, 1], [1, 0]].map(([dst, src]) => {
        const body = enc(uleb(0), 0x20, uleb(0), 0x20, uleb(1), 0x20, uleb(2),
                         0xfc, 0x0a, uleb(dst), uleb(src), 0x0b);
        return enc(uleb(body.length), body);
      }),
    )),
  ),
);

const fail = (why) => {
  console.log(`FAIL multimemory: ${why}`);
  process.exit(1);
};

let mod;
try {
  mod = await WebAssembly.compile(wasm);
} catch (err) {
  fail(`the engine will not compile a module with two imported memories: ${err.message}`);
}

const kernel = new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true });
const process1 = new WebAssembly.Memory({ initial: 1, maximum: 1 });
let inst;
try {
  inst = await WebAssembly.instantiate(mod, { e: { kernel, process: process1 } });
} catch (err) {
  fail(`the engine compiled it but would not instantiate it: ${err.message}`);
}

// A guest buffer in the process memory, pulled into the kernel's own address
// space -- which is the move every syscall that reads a guest pointer makes.
const k = new Uint8Array(kernel.buffer);
const p = new Uint8Array(process1.buffer);
const message = new TextEncoder().encode('a buffer the kernel cannot address');
p.set(message, 0x100);
inst.exports.pull(0x200, 0x100, message.length);
const pulled = new TextDecoder().decode(k.subarray(0x200, 0x200 + message.length));
if (pulled !== 'a buffer the kernel cannot address') {
  fail(`pull gave ${JSON.stringify(pulled)}`);
}

// And back the other way, which is a syscall's return value.
const reply = new TextEncoder().encode('and the answer going back');
k.set(reply, 0x300);
inst.exports.push(0x400, 0x300, reply.length);
const pushed = new TextDecoder().decode(p.subarray(0x400, 0x400 + reply.length));
if (pushed !== 'and the answer going back') {
  fail(`push gave ${JSON.stringify(pushed)}`);
}

// The point of the shared one: a second instance, as a second Worker would
// build, sees what the first wrote. Nothing is copied between them.
const second = await WebAssembly.instantiate(mod, {
  e: { kernel, process: new WebAssembly.Memory({ initial: 1, maximum: 1 }) },
});
second.exports.push(0x500, 0x300, reply.length);
const other = new Uint8Array(kernel.buffer);
if (new TextDecoder().decode(other.subarray(0x300, 0x300 + reply.length)) !== 'and the answer going back') {
  fail('a second instance did not see the first\'s kernel memory');
}

console.log('ok   multimemory (two imported memories, one shared, copy across both)');
