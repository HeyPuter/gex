/* libgex, the snapshot flavour (build32.sh and build64.sh
 * with GEX_SNAPSHOT=1, over the libc's GEX_SNAPSHOT hooks): threads stopped
 * for a session snapshot and started again from one.
 *
 * The libc's system calls are the default build's, through gex.syscall:
 * the link's asyncify instruments what can reach it, and while a snapshot
 * is pending the host may unwind the thread's frames at a call that can
 * block (__gex_snap_blocks) instead of making it (it was not made: its
 * number and arguments stay in the thread's context). The unwind stops
 * at the thread's root, the thread parks until it goes on in this
 * session, and the rewind makes the call again. A restored session's
 * thread starts as a clone child on the same record, in the memory the
 * snapshot holds, and is rewound from there.
 *
 * The root is a wasm32 thread's driver (the libc's __gex_fork_drive:
 * drive32.c); for a wasm64 program's thread, the host's entry into the
 * program from the guest code it runs, which only the host's outermost run
 * of that thread's guest code makes, or, below no guest code, the libc's
 * run of a thread's function (entry64.c). */
#ifndef GEX_SNAP_H
#define GEX_SNAP_H
#include <stdint.h>

#define SNAP_IMPORT(name) __attribute__((__import_module__("gex"), __import_name__(name)))
SNAP_IMPORT("syscall") void gex_syscall(int, long long);
SNAP_IMPORT("snap_wanted") int gex_snap_wanted(void);
/* the thread's record, and its roots: SNAP_DRIVER, SNAP_ENTRY or both (0:
 * none) */
SNAP_IMPORT("snap_register") void gex_snap_register(void *, int);

#define SNAP_DRIVER 1
#define SNAP_ENTRY 2

unsigned long __gex_get_ctx(void);
unsigned long __gex_get_tp(void);

struct driver {
	int (*fn)(void *);
	void *arg;
	unsigned long sp;
};

/* A record: asyncify's data first ({cur, end}, 32-bit words: the module
 * asyncify instruments is a wasm32 one either way). At the park the
 * context's registers are kept (the parked call's number and arguments, its
 * PC), which a restored thread's start overwrites and its resume puts back;
 * and the start a restored thread is given, a clone child's words (the
 * libc's __clone: function, argument, thread pointer, a fresh TLS block or
 * not) at the record's top, on a stack below them that the start runs on
 * until the rewind is under way. `entry` and `start` are what the host
 * gives the restored thread's context (its PC, the image's thread entry,
 * and its stack pointer); `sp_at`, the host's, the program's stack pointer
 * at the call; `idx`, the host's, the function its entry called (a wasm64
 * program's). The host reads and writes the words at offsets 0 to 20, and
 * `nlv` and `lv`, its own as well.
 *
 * A wasm64 program's thread stopped inside guest code that Wine's call
 * runs (a window procedure's message loop) has a level for each of its
 * entries from the guest, outermost first: how the entry came (`route`: 0
 * the host's entry function, 1 the fault hook, 2 an entry into Wine's PE
 * module of the flavour, whose call and state the host keeps in the
 * record's upper half, with that module's asyncify window: the program's
 * then ends at the half), the function, the hook's arguments, the guest's
 * stack pointer then, and the program's stack
 * pointer at the call-out the level runs in (`sp`, none for the first).
 * `nlv` levels are in use. */
#define SNAP_LEVELS 16
struct snap_level {
	uint32_t route, idx, code, kind;
	uint32_t sp, pad;
	uint64_t addr, rsp;
};
struct snap {
	uint32_t cur, end;
	uint32_t entry, start;
	uint32_t sp_at, idx;
	uint32_t nlv, pad;
	struct snap_level lv[SNAP_LEVELS];
	uint64_t regs[16], pc;
	struct driver drv;
};

#define SNAP_SIZE (256 << 10)
#define SNAP_STACK 16384

/* The record's asyncify window, and the clone child's start words at its
 * top for a restored thread entering `resume`. */
void __gex_snap_init(struct snap *);
void __gex_snap_start(struct snap *, int (*resume)(void *));
/* The context's registers into the record, and back. */
void __gex_snap_keep(struct snap *);
void __gex_snap_back(struct snap *);
#endif
