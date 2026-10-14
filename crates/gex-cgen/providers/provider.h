/* What every export provider (docs/merge-plan.md §9.9) includes after
 * gex_abi.h: the guest's registers and stack as an i386 callee sees them,
 * the return a provider makes, and waiting on a guest word.
 *
 * A provider body is an F_body (gex_body_t): it runs in place of the guest
 * function at an export's address, reached through the function table like
 * any guest function. Every caller reaches it through the table, so the
 * whole guest state is in the context on entry; a provider reads what the
 * calling convention passes, writes EAX (EDX), pops the return address (and
 * N bytes for thiscall/stdcall), and sets PC. It returns 1 when that return
 * is the one the caller predicted (`ret`), else 0 with everything written
 * back, as any F_body does. */
#ifndef GEX_PROVIDER_H
#define GEX_PROVIDER_H

/* Marks `fn` as providing `export` of the library the provider source is
 * for; the host binds it at that export's guest address. Taking the
 * function's address puts it in the module's element segment; the host reads
 * these lines from the source and binds each slot by its function's name. */
#define GEX_PROVIDE(fn, export) GEX_PROVIDE_(fn, __COUNTER__)
#define GEX_PROVIDE_(fn, n) GEX_PROVIDE__(fn, n)
#define GEX_PROVIDE__(fn, n) \
    __attribute__((export_name("gex_provide_" #n))) gex_body_t gex_provide_##n(void) { return fn; }

#define GEX_PROVIDER(name) int name(int ctx, int32_t depth, uint32_t need, uint64_t ret)

/* Puts `fn` in the module's element segment without binding it to an export
 * (the host finds it by name): an ELF provider's `gexp__ifunc`, below. */
#define GEX_TABLE(fn) GEX_PROVIDE_(fn, __COUNTER__)
/* Exports that bind together or not at all (`GEX_GROUP("malloc,free,...")`
 * on a line of its own): two allocators over one heap are fatal. The host
 * reads these lines from the source; in C it is nothing. */
#define GEX_GROUP(exports)

/* i386 registers in the context (64-bit slots, upper half zero). */
#define R_EAX 0
#define R_ECX 1
#define R_EDX 2
#define R_EBX 3
#define R_ESP 4
GEX_INLINE uint32_t gp_reg(int ctx, unsigned r) { return (uint32_t)GEX_C64(GEX_CTX_XN(r)); }
GEX_INLINE void gp_set(int ctx, unsigned r, uint32_t v) { GEX_C64(GEX_CTX_XN(r)) = v; }
/* A 32-bit guest word (guest VA == linear-memory offset). */
#define GW(a) (*(gex_u32 *)GEX_P((uint32_t)(a)))
#define GA(a) ((gex_a32 *)GEX_P((uint32_t)(a)))
/* Stack argument `i` (0-based, after the return address). */
GEX_INLINE uint32_t gp_arg(int ctx, unsigned i) { return GW(gp_reg(ctx, R_ESP) + 4 + 4 * i); }

/* Return to the guest: pop the return address and `pop` bytes of arguments. */
GEX_INLINE int gp_return(int ctx, uint64_t ret, unsigned pop) {
    uint32_t sp = gp_reg(ctx, R_ESP);
    uint32_t ra = GW(sp);
    gp_set(ctx, R_ESP, sp + 4 + pop);
    GEX_C64(GEX_CTX_PC) = ra;
    return ra == ret;
}
GEX_INLINE int gp_return_eax(int ctx, uint64_t ret, unsigned pop, uint32_t eax) {
    gp_set(ctx, R_EAX, eax);
    return gp_return(ctx, ret, pop);
}

/* A double stack argument at byte offset `off` past the return address. */
GEX_INLINE double gp_argf64(int ctx, unsigned off) {
    return gex_f64(*(gex_u64 *)GEX_P(gp_reg(ctx, R_ESP) + 4 + off));
}
/* xmm`n`'s low double, and setting it (the high half kept, as movsd does). */
GEX_INLINE double gp_xmm_f64(int ctx, unsigned n) { return gex_f64(GEX_C64(GEX_CTX_VN(n))); }
GEX_INLINE void gp_set_xmm_f64(int ctx, unsigned n, double v) { GEX_C64(GEX_CTX_VN(n)) = gex_b64(v); }

/* Push `v` onto the x87 stack as `fld m64` does (exactly: a double is an
 * extended value): the i386 ABI's floating-point return in ST0. */
GEX_INLINE void gp_push_st0_raw(int ctx, uint64_t sig, uint32_t se) {
    uint32_t top = (GEX_C32(GEX_CTX_X87_TOP) - 1u) & 7u;
    GEX_C32(GEX_CTX_X87_TOP) = top;
    GEX_C64(GEX_CTX_X87 + 16u * top) = sig;
    GEX_C16(GEX_CTX_X87 + 16u * top + 8u) = (uint16_t)se;
    GEX_C32(GEX_CTX_X87_TAGS) |= 1u << top;
    GEX_C32(GEX_CTX_X87_SW) &= ~0x200u; /* C1: no stack overflow */
}
GEX_INLINE void gp_push_st0(int ctx, double v) {
    uint64_t b = gex_b64(v), m = b & 0xfffffffffffffull, sig;
    uint32_t sign = (uint32_t)(b >> 63) << 15, e = (uint32_t)(b >> 52) & 0x7ff, se;
    if (e == 0x7ff) {
        se = sign | 0x7fff;
        sig = 0x8000000000000000ull | (m << 11);
    } else if (e == 0) {
        if (m == 0) {
            se = sign;
            sig = 0;
        } else {
            uint32_t s = (uint32_t)__builtin_clzll(m);
            sig = m << s;
            se = sign | (15372u - s);
        }
    } else {
        se = sign | (e - 1023u + 16383u);
        sig = 0x8000000000000000ull | (m << 11);
    }
    gp_push_st0_raw(ctx, sig, se);
}

/* This guest thread, never 0. */
GEX_INLINE uint32_t gp_self(int ctx) { return (uint32_t)GEX_C64(GEX_CTX_TID) + 1u; }

/* Wait while the word at `a` holds `v`, at most `ms` milliseconds (wasm
 * atomic wait on the shared guest memory); a store plus gp_wake ends it. */
GEX_INLINE void gp_wait(uint32_t a, uint32_t v, uint32_t ms) {
    (void)__builtin_wasm_memory_atomic_wait32((int *)GEX_P(a), (int)v, (int64_t)ms * 1000000);
}
GEX_INLINE void gp_wake(uint32_t a, uint32_t count) {
    (void)__builtin_wasm_memory_atomic_notify((int *)GEX_P(a), count);
}
/* Block this thread for `ms` (a wait nothing wakes: a word only it uses,
 * in its context's scratch). */
GEX_INLINE void gp_sleep(int ctx, uint32_t ms) {
    uint32_t w = (uint32_t)ctx + GEX_CTX_SCRATCH;
    gp_wait(w, GW(w), ms);
}
/* Inside a wait loop, before the provider has changed any guest state: a
 * safepoint, which may stop the thread (exit, a signal) and resume it at
 * the provider's own entry, `pc`. */
GEX_INLINE void gp_poll(int ctx, uint64_t pc) { gex_safepoint(ctx, (int64_t)pc); }
/* The address this body was entered at: every way into a provider (a call
 * or jump through the table, a unit's stub, tier-1's bridge, the root) sets
 * it. Read it on entry, before anything calls out. */
#define GP_SELF_PC() GEX_C64(GEX_CTX_LAST_TARGET)

/* ---- falling back to the guest's own code (a `lifted` provider) ----
 * A provider whose first line says `lifted` has the image's own body of
 * every export it binds kept beside it, as a lifted twin (gex_abi::ftab::TWIN).
 * GP_LIFTED(self) runs that body instead of the provider, as if the call had
 * gone there: for the cases a provider cannot match bit for bit (another
 * x87 control word, an error path that calls back into the library). Use it
 * in a provider body before it has written anything, `self` being
 * GP_SELF_PC() read on entry. The lookup is a host call: slow paths only. A
 * unit without the twin (clang wanted a shadow stack for it) traps. */
__attribute__((import_module("gex"), import_name("ftab_twin"))) int32_t gex_ftab_twin(int64_t addr);
#define GP_LIFTED(self)                                                    \
    do {                                                                   \
        uint32_t gp_self_ = (self);                                        \
        int32_t gp_twin_ = gex_ftab_twin((int64_t)gp_self_);               \
        if (!gp_twin_) {                                                   \
            __builtin_trap();                                              \
        }                                                                  \
        GEX_C64(GEX_CTX_LAST_TARGET) = gp_self_;                           \
        GEX_MUSTTAIL return GEX_FTAB(gp_twin_)(ctx, depth, need, ret);     \
    } while (0)

/* ---- the slow path's view of the library a provider serves ----
 * For rare cases (errors) that must do what the library at hand does: the
 * library is found from the provider's own export address, so one provider
 * serves Microsoft's and Wine's builds of a DLL. */

/* The base of the PE image holding `addr` (its exports are inside it): the
 * 64 KiB-aligned "MZ" at or below it whose SizeOfImage covers it; 0 if none
 * within 64 MiB. */
GEX_INLINE uint32_t gp_module_base(uint32_t addr) {
    for (uint32_t b = addr & ~0xffffu, n = 0; n < 1024 && b; b -= 0x10000u, ++n) {
        if (*(gex_u16 *)GEX_P(b) != 0x5a4d) {
            continue;
        }
        uint32_t pe = b + GW(b + 0x3c);
        if (GW(pe) == 0x4550 && addr - b < GW(pe + 24 + 56)) {
            return b;
        }
    }
    return 0;
}
/* Wine's own build of a DLL ("Wine builtin DLL" after the DOS header).
 * (Constants, not strings: a provider module carries no data.) */
GEX_INLINE int gp_wine_builtin(uint32_t base) {
    return GW(base + 0x40) == 0x656e6957u && GW(base + 0x44) == 0x69756220u && GW(base + 0x48) == 0x6e69746cu &&
           GW(base + 0x4c) == 0x4c4c4420u;
}
/* The address of the export of the image at `base` whose name is the `len`
 * (at most 8) bytes of `name` (little-endian: "_errno" is
 * 0x6f6e7272655full), or 0. */
GEX_INLINE uint32_t gp_export(uint32_t base, uint64_t name, uint32_t len) {
    uint32_t pe = base + GW(base + 0x3c), dir = GW(pe + 24 + 96);
    if (!dir) {
        return 0;
    }
    uint32_t e = base + dir, nn = GW(e + 24), af = base + GW(e + 28), an = base + GW(e + 32), ao = base + GW(e + 36);
    for (uint32_t i = 0; i < nn; ++i) {
        uint32_t n = base + GW(an + 4 * i), k = 0;
        while (k < len && *(gex_u8 *)GEX_P(n + k) == (uint8_t)(name >> (8 * k))) {
            ++k;
        }
        if (k == len && !*(gex_u8 *)GEX_P(n + k)) {
            return base + GW(af + 4 * *(gex_u16 *)GEX_P(ao + 2 * i));
        }
    }
    return 0;
}
/* Call the guest function at `fn` with no arguments (cdecl) and return its
 * EAX. Pushes a return address of its own, so the call returns here; a
 * callee that leaves instead (longjmp, an exception unwinding past this
 * frame) is beyond a provider's slow path and traps. */
GEX_INLINE uint32_t gp_call0(int ctx, int32_t depth, uint32_t fn) {
    uint32_t sp = gp_reg(ctx, R_ESP) - 4, ra = fn ^ 1u;
    GW(sp) = ra;
    gp_set(ctx, R_ESP, sp);
    GEX_C64(GEX_CTX_LAST_TARGET) = fn;
    if (!GEX_FTAB(gex_ftab_idx(fn))(ctx, depth + 256, 0, ra)) {
        __builtin_trap();
    }
    return gp_reg(ctx, R_EAX);
}
/* errno = `code`, through the library's own `_errno()` (its per-thread
 * slot). `self` is the provider's entry address (GP_SELF_PC on entry). */
GEX_INLINE void gp_set_errno(int ctx, int32_t depth, uint32_t self, uint32_t code) {
    uint32_t base = gp_module_base(self), f = base ? gp_export(base, 0x6f6e7272655full, 6) : 0;
    if (f) {
        GW(gp_call0(ctx, depth, f)) = code;
    }
}
#define GP_EDOM 33u

/* ---- SysV x86-64: ELF providers (docs/elf-providers.md) ----
 * A provider for a Linux library (first line `PROVIDES libc.so.6 x86_64`) binds
 * exports by versioned name (`memcpy@@GLIBC_2.14`, or a bare name for the
 * default version). Integer and pointer arguments come in rdi, rsi, rdx,
 * rcx, r8, r9, then the stack above the return address; the result goes in
 * rax (xmm0); the caller pops. */
#define R_RAX 0
#define R_RCX 1
#define R_RDX 2
#define R_RSP 4
#define R_RSI 6
#define R_RDI 7
#define R_R8 8
#define R_R9 9
GEX_INLINE uint64_t gp64_reg(int ctx, unsigned r) { return GEX_C64(GEX_CTX_XN(r)); }
GEX_INLINE void gp64_set(int ctx, unsigned r, uint64_t v) { GEX_C64(GEX_CTX_XN(r)) = v; }
/* A guest pointer as a C pointer: tier-1's guard (an address past the guest
 * memory traps rather than wrapping into it). */
#define GP64_P(a) gex_gb((uint64_t)(a))
#define GW64(a) (*(gex_u64 *)GP64_P(a))
/* Integer argument `i` (0-based). (A switch, not a table: a provider module
 * carries no data.) */
GEX_INLINE uint64_t gp64_arg(int ctx, unsigned i) {
    switch (i) {
    case 0: return gp64_reg(ctx, R_RDI);
    case 1: return gp64_reg(ctx, R_RSI);
    case 2: return gp64_reg(ctx, R_RDX);
    case 3: return gp64_reg(ctx, R_RCX);
    case 4: return gp64_reg(ctx, R_R8);
    case 5: return gp64_reg(ctx, R_R9);
    default: return GW64(gp64_reg(ctx, R_RSP) + 8u + 8u * (i - 6u));
    }
}
/* Return to the guest: pop the return address. */
GEX_INLINE int gp64_return(int ctx, uint64_t ret) {
    uint64_t sp = gp64_reg(ctx, R_RSP), ra = GW64(sp);
    gp64_set(ctx, R_RSP, sp + 8u);
    GEX_C64(GEX_CTX_PC) = ra;
    return ra == ret;
}
GEX_INLINE int gp64_return_rax(int ctx, uint64_t ret, uint64_t rax) {
    gp64_set(ctx, R_RAX, rax);
    return gp64_return(ctx, ret);
}
/* Call the guest function at `fn` (SysV x86-64) with up to six integer
 * arguments from inside a provider, on the calling guest thread: below
 * `sp_top` (the provider's own scratch on the guest stack stays above it),
 * with a return address of its own (`fn ^ 1`, an address nothing else
 * returns to), through the function table, until the guest returns there.
 * The guest's RSP is as it was afterwards; the result is RAX. */
GEX_INLINE uint64_t gp64_call6(int ctx, int32_t depth, uint64_t fn, uint64_t sp_top, uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3,
                               uint64_t a4, uint64_t a5) {
    uint64_t rsp0 = gp64_reg(ctx, R_RSP), ra = fn ^ 1u;
    uint64_t sp = ((sp_top - 128u) & ~15ull) - 8u;
    GW64(sp) = ra;
    gp64_set(ctx, R_RDI, a0);
    gp64_set(ctx, R_RSI, a1);
    gp64_set(ctx, R_RDX, a2);
    gp64_set(ctx, R_RCX, a3);
    gp64_set(ctx, R_R8, a4);
    gp64_set(ctx, R_R9, a5);
    gp64_set(ctx, R_RAX, 0); /* no vector arguments (varargs) */
    gp64_set(ctx, R_RSP, sp);
    for (uint64_t pc = fn;;) {
        GEX_C64(GEX_CTX_LAST_TARGET) = pc;
        if (GEX_FTAB(gex_ftab_idx(pc))(ctx, depth + 1, 0, ra)) {
            break;
        }
        pc = GEX_C64(GEX_CTX_PC);
        if (pc == ra) {
            break;
        }
    }
    gp64_set(ctx, R_RSP, rsp0);
    return gp64_reg(ctx, R_RAX);
}
#define GP64_MAX(a, b) ((a) > (b) ? (a) : (b))
/* For a provider's slow paths, the library's own functions by name (a
 * stateful provider: the names are data). The ELF image holding `addr`: the page-aligned "\x7fELF" at or below it
 * (64 MiB at most), 0 if none. */
GEX_INLINE uint64_t gp64_elf_base(uint64_t addr) {
    for (uint64_t b = addr & ~0xfffull, n = 0; n < 16384 && b; b -= 0x1000, ++n) {
        if (*(gex_u32 *)GP64_P(b) == 0x464c457fu) {
            return b;
        }
    }
    return 0;
}

/* The address of dynamic symbol `name` of the image at `base` (its
 * DT_SYMTAB, DT_STRTAB and DT_GNU_HASH's symbol count), or 0. */
GEX_INLINE uint64_t gp64_elf_sym(uint64_t base, const char *name) {
    uint64_t ph = base + GW64(base + 0x20);
    unsigned phn = *(gex_u16 *)GP64_P(base + 0x38);
    uint64_t dyn = 0, vbias = 0;
    int first = 1;
    for (unsigned i = 0; i < phn; ++i, ph += 56) {
        uint32_t type = *(gex_u32 *)GP64_P(ph);
        if (type == 1 && first) {
            vbias = base - (GW64(ph + 16) & ~0xfffull); /* the first PT_LOAD's place */
            first = 0;
        }
        if (type == 2) {
            dyn = GW64(ph + 16);
        }
    }
    if (!dyn) {
        return 0;
    }
    dyn += vbias;
    uint64_t symtab = 0, strtab = 0, gnuhash = 0;
    for (uint64_t d = dyn; GW64(d); d += 16) {
        uint64_t tag = GW64(d), val = GW64(d + 8);
        /* The dynamic linker relocates these in place; before it has,
         * they are link-time addresses. */
        if (val < base) {
            val += vbias;
        }
        if (tag == 6) {
            symtab = val;
        } else if (tag == 5) {
            strtab = val;
        } else if (tag == 0x6ffffef5) {
            gnuhash = val;
        }
    }
    if (!symtab || !strtab || !gnuhash) {
        return 0;
    }
    /* The symbol count: past the last bucket's chain. */
    uint32_t nbuckets = *(gex_u32 *)GP64_P(gnuhash), symoffset = *(gex_u32 *)GP64_P(gnuhash + 4);
    uint32_t bloom = *(gex_u32 *)GP64_P(gnuhash + 8);
    uint64_t buckets = gnuhash + 16 + 8ull * bloom, chain = buckets + 4ull * nbuckets;
    uint32_t last = 0;
    for (uint32_t i = 0; i < nbuckets; ++i) {
        last = GP64_MAX(last, *(gex_u32 *)GP64_P(buckets + 4ull * i));
    }
    uint32_t nsyms = last;
    if (last >= symoffset) {
        while (!(*(gex_u32 *)GP64_P(chain + 4ull * (nsyms - symoffset)) & 1)) {
            nsyms++;
        }
        nsyms++;
    }
    for (uint32_t i = 0; i < nsyms; ++i) {
        uint64_t sym = symtab + 24ull * i;
        uint32_t n = *(gex_u32 *)GP64_P(sym);
        uint64_t v = GW64(sym + 8);
        if (!v) {
            continue;
        }
        const char *q = name;
        uint64_t k = strtab + n;
        while (*q && (*(const unsigned char *)GP64_P(k)) == (unsigned char)*q) {
            ++q, ++k;
        }
        if (!*q && !(*(const unsigned char *)GP64_P(k))) {
            return v + vbias;
        }
    }
    return 0;
}

/* An IFUNC's resolver (the symbol's address, which the dynamic linker calls
 * for every relocation against it): the host binds this body there and the
 * provided function one byte further on, the "implementation" this returns.
 * Every PLT and GOT caller, and the library's own IRELATIVE slots, then land
 * on the provider. A source with IFUNC exports says `GEX_ELF_IFUNC`. */
#define GEX_ELF_IFUNC                                                            \
    static GEX_PROVIDER(gexp__ifunc) {                                           \
        (void)depth, (void)need;                                                 \
        return gp64_return_rax(ctx, ret, (uint64_t)GP_SELF_PC() + 1u);           \
    }                                                                            \
    GEX_TABLE(gexp__ifunc)

#endif
