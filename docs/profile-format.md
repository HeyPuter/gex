# Profiles and session manifests

`gexc run --record DIR` (or `GEX_PROFILE=1`, which writes to
`$GEX_PROFILE_OUT`, default `./gex-profile`) writes two files when the run
ends:

- `DIR/profile.json` says how often code ran.
- `DIR/session.json` says what the run consisted of.

The function tier (`gexc build --profile`, M3) and `gex-eject` (M4/M5) read
them. The code is in `crates/gex-xlate/src/profile.rs` (data model, reader,
writer, merge), `crates/gex-emit/src/profile.rs` (instrumentation and the
counter memory layout) and `shims/native/src/profile.rs` (collection).

## The rule for consumers

**A profile may only ever change speed, never correctness.** Treat every
fact in it as a hint:

- The profile may be stale, truncated, from another program, or empty.
- A block, target or function it names may not exist in the binary you
  have, or may be split differently.
- A devirtualized call must be guarded.
- A cold path must still work.
- An unlisted syscall or file must still fail loudly, not misbehave.

## Keys

Code is named by `(object, offset)`.

- **object:** `sha256:<64 hex digits>` of the whole file. The same `libc.so`
  loaded by two programs, from two sysroots or under two paths is one object.
- **offset:** the object's **link-time virtual address**. That is the runtime
  address less the load bias: the numbering its symbol table, `.eh_frame` and
  `gex_ld::elf::load(bytes, 0)` use. For an `ET_EXEC` it equals the runtime
  address.
- **`anon`:** code with no file behind it (a guest JIT's, a test image) is
  recorded under the pseudo-object `anon`, with the absolute address as its
  offset. It is not comparable across runs.

## `profile.json` (format `gex-profile`, version 1)

```json
{ "format": "gex-profile", "version": 1, "isa": "aarch64", "runs": 1,
  "objects": [ {
    "id": "sha256:…",
    "paths": ["/lib/ld-musl-aarch64.so.1"],
    "blocks":   [[off, end, n, call, jump, ret, taken], …],
    "indirect": [{"off": o, "call": false, "total": n, "replaced": n,
                  "targets": [["sha256:…", off, n], …], "other": n}, …],
    "returns":  [[off, expected, unexpected, unchecked], …],
    "interior": [[off, containing_block_off, dynamic_entries], …],
    "syscalls": [[off, func_obj, func_off, nr, "name", n], …]
  } ] }
```

Numbers are unsigned decimal. A reader rejects any other `format` or
`version`.

**Array fields:**

- **`blocks`**, one row per tier-1 block that ran:
  - `end` is one past the block's last instruction.
  - `n` counts every time the block started running, exactly.
  - `call`, `jump` and `ret` count how many of those entries arrived by a
    call (direct or indirect), an indirect jump, or a return.
  - `n - call - jump - ret` arrived by a direct branch or fall-through.
  - `taken` is a conditional branch's taken count. Not-taken is
    `n - taken`.
- **`indirect`**, one entry per indirect jump or call site (`br`/`blr`,
  `jmp`/`call` through a register or memory). `off` is the start of the block
  that ends in the branch.
  - `total` is exact.
  - `targets` holds at most four entries, hottest first.
  - `other` is `total` minus the listed targets.
  - `replaced` is `0` when the site had at most four targets, and then the
    counts are exact and complete.
  - Otherwise the table ran Metwally's space-saving algorithm over four slots.
    Every target with more than a quarter of the traffic is present, and
    counts may be over-estimated by at most the smallest count shown.
- **`returns`**, one row per return site (`ret`). Each return is checked
  against a per-thread shadow stack that every call pushes:
  - `expected`: went to the address the matching call pushed.
  - `unexpected`: went elsewhere (`longjmp`, coroutine switches, a signal
    handler returning through the trampoline, a `ret` used as a jump).
  - `unchecked`: had nothing to compare with, because the shadow stack was
    empty (a forked child, the first frame of a thread) or had wrapped past
    262,144 frames.
- **`interior`**: blocks that start strictly inside another block of the same
  object and were entered dynamically (call, jump or return), with the count.
  These are the dispatches into the middle of known code. Once function
  boundaries are known (S2), the stronger question is dynamic entries that
  are not a function start. The per-block `call`/`jump`/`ret` counts answer it.
- **`syscalls`**, one row per `(syscall instruction, enclosing function,
  number)`:
  - `off` is the `svc`/`syscall` instruction.
  - `func` is the callee of the innermost call on the shadow stack: the
    function containing the instruction, or `null` if unknown.
  - `nr` is the canonical (AArch64 generic) number. An x86-64 guest's numbers
    are translated first.

**Derived facts:**

- **Function entry counts:** a block's `call` column. Every call site,
  direct or indirect, announces its target, and the target's first block
  counts it.
- **Merging:** `gexc profile merge -o OUT IN...` adds profiles together. Objects
  match by hash, so libc's counts from different programs combine. `runs`
  says how many runs went in.
  - `gexc profile merge --split DIR IN...` keeps a per-object store:
    `DIR/sha256-<hex>.json`, each merged into what is already there, so the
    libc profile accumulates across every program that loads it.
  - Merging a profile of another ISA fails.
- **Summary:** `gexc profile show [--top N] FILE`.

## `session.json` (format `gex-session`, version 1)

```json
{ "format": "gex-session", "version": 1, "isa": "aarch64",
  "argv": [...], "env": [...], "cwd": "/"|null, "sysroot": "…"|null,
  "objects":  [{"id": "sha256:…", "path": "/lib/libc.so", "host_path": "…"|null,
                "size": n, "role": "exe"|"interp"|"library"}],
  "programs": [{"pid": 1, "exe": "…", "argv": [...], "env": [...]}],
  "processes": n, "threads": n,
  "syscalls": [{"nr": 64, "name": "write", "n": 12}],
  "files":    [{"path": "/etc/passwd", "read": true, "write": false,
                "create": false, "directory": false, "ok": 1, "failed": 0,
                "errno": null}],
  "exit": "status 0" | "signal 11" | null }
```

- **`objects`** is the loaded closure: the executable, the interpreter, and
  every file-backed executable mapping (libraries, including `dlopen`ed ones).
  - Paths are guest paths.
  - `host_path` is where gex read the file from, when a sysroot is in use.
- **`programs`** lists the first program and every `execve`, in order, with
  its argv and environment.
- **`syscalls`** are canonical numbers with counts, over the whole process
  tree.
- **`files`** lists every path passed to `open`, `openat`, `creat` or
  `execve`:
  - Relative paths are resolved against the working directory.
  - A path relative to another descriptor is written `<fd N>/path`.
  - Failed opens are kept (with the last `errno`), because a program that
    probes for a missing file needs it to stay missing.

## Collection: where the counters live

With profiling on, every tier-1 module imports a second wasm memory, `gex.profile`,
as memory index 1 (the guest's memory is always memory 0). The host creates
one per store, which means one per guest thread. So:

- Counters are exact without atomics: one thread writes each.
- The guest cannot address them.
- The guest's own address space and layout are untouched, so profiling
  cannot change what a program observes.

`crates/gex-emit/src/profile.rs` has the byte layout:

- a thread header (entry-kind slot, shadow stack);
- one self-describing region per module instance. The region holds a header,
  block descriptors and records. Its base is a module-defined global, set by
  the exported `gexprofinit` after instantiation, so cached modules stay
  valid.

The host reads the regions back when a thread or process ends, and on demand:

- `GEX_PROFILE_EVERY=<seconds>` rewrites the files periodically;
- creating `DIR/dump` rewrites them once.

**Limitations:**

- **Tier-1 only.** Profiling together with the function tier (`--cgen`) is
  refused.
- **Native host only.** The browser shim does not yet provide `gex.profile`,
  so a profiled module fails to instantiate there. `merge` of profiled units
  is not supported.
- **Instrumentation shapes the translation.** With profiling off, modules are
  byte-identical to a build without this code.
