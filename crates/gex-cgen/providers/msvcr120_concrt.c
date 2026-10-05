/* PROVIDES msvcr120.dll i386
 *
 * The MSVC 2013 Concurrency Runtime surface programs and msvcp120 use
 * (std::mutex and std::condition_variable are built on critical_section and
 * _Condition_variable), on wasm atomics instead of ConcRT's scheduler, whose
 * real implementation spins and yields through its resource manager.
 * pewasm's port/src/noita_concrt.c, in the F_body form (docs/merge-plan.md
 * §9.9.2 step 1).
 *
 * Object state lives in the first words of each ConcRT object, which the
 * callers allocate at its real (larger) size. Waits are bounded at 1 ms so a
 * missed wake costs 1 ms; every wait loop polls the safepoint before it has
 * changed anything, so a thread stopped there restarts the call. */

#define COOPERATIVE_WAIT_TIMEOUT 0xffffffffu

/* The NT yield park's generation word (gex_kernel::nt::sync::YIELD_GEN, in
 * KUSER_SHARED_DATA's page past the structure). A thread spinning in
 * SwitchToThread/Sleep(0) parks on it; bumping it and notifying wakes the
 * spinner, which re-checks what it waits for. Kicked when a wait here first
 * blocks (this thread went idle: the work a spinner waits for may be done)
 * and when a wake signals a waiter. A host with no parked thread just sees
 * the word change. */
#define YIELD_GEN 0x7ffe0ff0u
/* How many threads are parked on it (gex_kernel::nt::sync::YIELD_PARKED): a
 * notify with nobody parked is a host futex call for nothing, and kicks come
 * with every notify and contended unlock. */
#define YIELD_PARKED 0x7ffe0ff4u
static void yield_kick(void) {
    __atomic_fetch_add(GA(YIELD_GEN), 1u, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(GA(YIELD_PARKED), __ATOMIC_SEQ_CST)) {
        gp_wake(YIELD_GEN, 0x7fffffffu);
    }
}

/* ---- critical_section: word 0 = owner (thread + 1), word 1 = waiters; not recursive ---- */

static int cs_try(int ctx, uint32_t cs) {
    return __sync_bool_compare_and_swap(GA(cs), 0u, gp_self(ctx));
}
/* Takes the lock; with `limit_ms` (not ~0) gives up after about that many
 * 1 ms waits. Polls only when `pc` is nonzero (before the caller changed
 * anything). */
static int cs_lock(int ctx, uint32_t cs, uint32_t limit_ms, uint64_t pc) {
    uint32_t waited = 0;
    for (unsigned spin = 0;; ++spin) {
        uint32_t held = __sync_val_compare_and_swap(GA(cs), 0u, gp_self(ctx));
        if (!held) {
            return 1;
        }
        if (spin < 64) {
            continue;
        }
        if (limit_ms != 0xffffffffu && waited >= limit_ms) {
            return 0;
        }
        if (pc) {
            gp_poll(ctx, pc);
        }
        if (!waited) {
            yield_kick();
        }
        __atomic_fetch_add(GA(cs + 4), 1u, __ATOMIC_SEQ_CST);
        gp_wait(cs, held, 1);
        __atomic_fetch_sub(GA(cs + 4), 1u, __ATOMIC_SEQ_CST);
        ++waited;
    }
}
static void cs_unlock(uint32_t cs) {
    __atomic_store_n(GA(cs), 0u, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(GA(cs + 4), __ATOMIC_SEQ_CST)) {
        gp_wake(cs, 1);
        yield_kick();
    }
}

/* ??0critical_section@Concurrency@@QAE@XZ */
static GEX_PROVIDER(cs_ctor) {
    (void)depth, (void)need;
    uint32_t cs = gp_reg(ctx, R_ECX);
    GW(cs) = 0;
    GW(cs + 4) = 0;
    return gp_return_eax(ctx, ret, 0, cs);
}
/* ??1critical_section@Concurrency@@QAE@XZ, and every destructor here */
static GEX_PROVIDER(nop_dtor) {
    (void)depth, (void)need;
    return gp_return(ctx, ret, 0);
}
/* ?lock@critical_section@Concurrency@@QAEXXZ */
static GEX_PROVIDER(cs_lock_) {
    (void)depth, (void)need;
    uint64_t pc = GP_SELF_PC();
    cs_lock(ctx, gp_reg(ctx, R_ECX), 0xffffffffu, pc);
    return gp_return(ctx, ret, 0);
}
/* ?try_lock@critical_section@Concurrency@@QAE_NXZ */
static GEX_PROVIDER(cs_try_lock) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 0, (uint32_t)cs_try(ctx, gp_reg(ctx, R_ECX)));
}
/* ?try_lock_for@critical_section@Concurrency@@QAE_NI@Z */
static GEX_PROVIDER(cs_try_lock_for) {
    (void)depth, (void)need;
    uint64_t pc = GP_SELF_PC();
    uint32_t got = (uint32_t)cs_lock(ctx, gp_reg(ctx, R_ECX), gp_arg(ctx, 0), pc);
    return gp_return_eax(ctx, ret, 4, got);
}
/* ?unlock@critical_section@Concurrency@@QAEXXZ */
static GEX_PROVIDER(cs_unlock_) {
    (void)depth, (void)need;
    cs_unlock(gp_reg(ctx, R_ECX));
    return gp_return(ctx, ret, 0);
}

/* ---- event: manual reset, word 0 = signaled, word 1 = waiters ---- */

/* ??0event@Concurrency@@QAE@XZ */
static GEX_PROVIDER(event_ctor) {
    (void)depth, (void)need;
    uint32_t e = gp_reg(ctx, R_ECX);
    GW(e) = 0;
    GW(e + 4) = 0;
    return gp_return_eax(ctx, ret, 0, e);
}
/* ?set@event@Concurrency@@QAEXXZ */
static GEX_PROVIDER(event_set) {
    (void)depth, (void)need;
    uint32_t e = gp_reg(ctx, R_ECX);
    __atomic_store_n(GA(e), 1u, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(GA(e + 4), __ATOMIC_SEQ_CST)) {
        gp_wake(e, 0x7fffffffu);
    }
    yield_kick();
    return gp_return(ctx, ret, 0);
}
/* ?reset@event@Concurrency@@QAEXXZ */
static GEX_PROVIDER(event_reset) {
    (void)depth, (void)need;
    __atomic_store_n(GA(gp_reg(ctx, R_ECX)), 0u, __ATOMIC_SEQ_CST);
    return gp_return(ctx, ret, 0);
}
/* ?wait@event@Concurrency@@QAEII@Z: 0 once set, COOPERATIVE_WAIT_TIMEOUT on timeout. */
static GEX_PROVIDER(event_wait) {
    (void)depth, (void)need;
    uint64_t pc = GP_SELF_PC();
    uint32_t e = gp_reg(ctx, R_ECX), timeout = gp_arg(ctx, 0), waited = 0;
    while (!__atomic_load_n(GA(e), __ATOMIC_SEQ_CST) && timeout && (timeout == 0xffffffffu || waited < timeout)) {
        gp_poll(ctx, pc);
        if (!waited) {
            yield_kick();
        }
        __atomic_fetch_add(GA(e + 4), 1u, __ATOMIC_SEQ_CST);
        gp_wait(e, 0, 1);
        __atomic_fetch_sub(GA(e + 4), 1u, __ATOMIC_SEQ_CST);
        ++waited;
    }
    uint32_t r = __atomic_load_n(GA(e), __ATOMIC_SEQ_CST) ? 0 : COOPERATIVE_WAIT_TIMEOUT;
    return gp_return_eax(ctx, ret, 4, r);
}

/* ---- _Condition_variable: word 0 = notification generation, word 1 = waiters ----
 * Waiters wake on any notification after they started waiting; users
 * re-check their predicate, so waking all is permitted. */

/* ??0_Condition_variable@details@Concurrency@@QAE@XZ */
static GEX_PROVIDER(cond_ctor) {
    (void)depth, (void)need;
    uint32_t c = gp_reg(ctx, R_ECX);
    GW(c) = 0;
    GW(c + 4) = 0;
    return gp_return_eax(ctx, ret, 0, c);
}
/* Unlocks `cs`, waits for a notification (at most `timeout` ms, ~0 for
 * none), relocks. No safepoint once `cs` is released: the call cannot be
 * restarted from its entry after that. */
static int cond_wait(int ctx, uint32_t c, uint32_t cs, uint32_t timeout) {
    uint32_t gen = __atomic_load_n(GA(c), __ATOMIC_SEQ_CST), waited = 0;
    cs_unlock(cs);
    int signaled;
    while (!(signaled = __atomic_load_n(GA(c), __ATOMIC_SEQ_CST) != gen) && (timeout == 0xffffffffu || waited < timeout)) {
        if (!waited) {
            yield_kick();
        }
        __atomic_fetch_add(GA(c + 4), 1u, __ATOMIC_SEQ_CST);
        gp_wait(c, gen, 1);
        __atomic_fetch_sub(GA(c + 4), 1u, __ATOMIC_SEQ_CST);
        ++waited;
    }
    cs_lock(ctx, cs, 0xffffffffu, 0);
    return signaled;
}
/* ?wait@_Condition_variable@details@Concurrency@@QAEXAAVcritical_section@3@@Z */
static GEX_PROVIDER(cond_wait_) {
    (void)depth, (void)need;
    cond_wait(ctx, gp_reg(ctx, R_ECX), gp_arg(ctx, 0), 0xffffffffu);
    return gp_return(ctx, ret, 4);
}
/* ?wait_for@_Condition_variable@details@Concurrency@@QAE_NAAVcritical_section@3@I@Z */
static GEX_PROVIDER(cond_wait_for) {
    (void)depth, (void)need;
    uint32_t r = (uint32_t)cond_wait(ctx, gp_reg(ctx, R_ECX), gp_arg(ctx, 0), gp_arg(ctx, 1));
    return gp_return_eax(ctx, ret, 8, r);
}
/* ?notify_one@... and ?notify_all@_Condition_variable@details@Concurrency@@QAEXXZ */
static GEX_PROVIDER(cond_notify) {
    (void)depth, (void)need;
    uint32_t c = gp_reg(ctx, R_ECX);
    __atomic_fetch_add(GA(c), 1u, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(GA(c + 4), __ATOMIC_SEQ_CST)) {
        gp_wake(c, 0x7fffffffu);
    }
    yield_kick();
    return gp_return(ctx, ret, 0);
}

/* ---- scheduler queries, yielding and tasks ---- */

/* _Context::_Yield and _UnderlyingYield are not provided: the guest's own
 * (SwitchToThread, a host yield) measured better than a 1 ms wait, for
 * world loading (447 s against 468 s) and steady state (18.2 against 16.7
 * world fps; tier-1, two paired runs each, docs/noita-perf.md). */

/* ?wait@Concurrency@@YAXI@Z (cdecl, milliseconds) */
static GEX_PROVIDER(wait_ms) {
    (void)depth, (void)need;
    uint64_t pc = GP_SELF_PC();
    for (uint32_t ms = gp_arg(ctx, 0), k = 0; k < ms; ++k) {
        gp_poll(ctx, pc);
        if (!k) {
            yield_kick();
        }
        gp_sleep(ctx, 1);
    }
    return gp_return(ctx, ret, 0);
}
/* _GetConcurrency, _CurrentScheduler::_Id, CurrentScheduler::GetNumberOfVirtualProcessors */
static GEX_PROVIDER(one) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 0, 1);
}
/* Context::IsCurrentTaskCollectionCanceling */
static GEX_PROVIDER(false_) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 0, 0);
}
/* _Trace_agents (cdecl varargs) */
static GEX_PROVIDER(noop) {
    (void)depth, (void)need;
    return gp_return(ctx, ret, 0);
}
/* ScheduleTask(proc, data), cdecl: runs the task to completion on the
 * calling thread (one virtual processor is a valid schedule). The frame
 * [ret, proc, data] becomes [ret, data] for proc, a cdecl function of one
 * argument, which returns straight to the caller; the caller then pops its
 * two argument words as usual. */
static GEX_PROVIDER(schedule_task) {
    uint32_t sp = gp_reg(ctx, R_ESP);
    uint32_t proc = GW(sp + 4);
    GW(sp + 4) = GW(sp + 8);
    GEX_C64(GEX_CTX_LAST_TARGET) = proc;
    GEX_MUSTTAIL return GEX_FTAB(gex_ftab_idx(proc))(ctx, depth, need, ret);
}

GEX_PROVIDE(cs_ctor, "??0critical_section@Concurrency@@QAE@XZ")
GEX_PROVIDE(nop_dtor, "??1critical_section@Concurrency@@QAE@XZ")
GEX_PROVIDE(cs_lock_, "?lock@critical_section@Concurrency@@QAEXXZ")
GEX_PROVIDE(cs_try_lock, "?try_lock@critical_section@Concurrency@@QAE_NXZ")
GEX_PROVIDE(cs_try_lock_for, "?try_lock_for@critical_section@Concurrency@@QAE_NI@Z")
GEX_PROVIDE(cs_unlock_, "?unlock@critical_section@Concurrency@@QAEXXZ")
GEX_PROVIDE(event_ctor, "??0event@Concurrency@@QAE@XZ")
GEX_PROVIDE(nop_dtor, "??1event@Concurrency@@QAE@XZ")
GEX_PROVIDE(event_set, "?set@event@Concurrency@@QAEXXZ")
GEX_PROVIDE(event_reset, "?reset@event@Concurrency@@QAEXXZ")
GEX_PROVIDE(event_wait, "?wait@event@Concurrency@@QAEII@Z")
GEX_PROVIDE(cond_ctor, "??0_Condition_variable@details@Concurrency@@QAE@XZ")
GEX_PROVIDE(nop_dtor, "??1_Condition_variable@details@Concurrency@@QAE@XZ")
GEX_PROVIDE(cond_wait_, "?wait@_Condition_variable@details@Concurrency@@QAEXAAVcritical_section@3@@Z")
GEX_PROVIDE(cond_wait_for, "?wait_for@_Condition_variable@details@Concurrency@@QAE_NAAVcritical_section@3@I@Z")
GEX_PROVIDE(cond_notify, "?notify_one@_Condition_variable@details@Concurrency@@QAEXXZ")
GEX_PROVIDE(cond_notify, "?notify_all@_Condition_variable@details@Concurrency@@QAEXXZ")
GEX_PROVIDE(wait_ms, "?wait@Concurrency@@YAXI@Z")
GEX_PROVIDE(one, "?_GetConcurrency@details@Concurrency@@YAIXZ")
GEX_PROVIDE(one, "?_Id@_CurrentScheduler@details@Concurrency@@SAIXZ")
GEX_PROVIDE(one, "?GetNumberOfVirtualProcessors@CurrentScheduler@Concurrency@@SAIXZ")
GEX_PROVIDE(false_, "?IsCurrentTaskCollectionCanceling@Context@Concurrency@@SA_NXZ")
GEX_PROVIDE(noop, "?_Trace_agents@Concurrency@@YAXW4Agents_EventType@1@_JZZ")
GEX_PROVIDE(schedule_task, "?_ScheduleTask@_CurrentScheduler@details@Concurrency@@SAXP6AXPAX@Z0@Z")
GEX_PROVIDE(schedule_task, "?ScheduleTask@CurrentScheduler@Concurrency@@SAXP6AXPAX@Z0@Z")
