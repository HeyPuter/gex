/* PROVIDES [nt-trampolines] i386
 *
 * The i386 unix-call dispatcher (gex_kernel::win::tramp::UNIX_CALL_32,
 * what ntdll's __wine_unix_call_dispatcher points at), with the hottest
 * OpenGL calls written straight into the GL command ring. Wine's PE
 * opengl32 makes one unix call per GL function; Noita makes about 3,900 a
 * frame, 81% of them the seven below (and 9% glEnable/DisableClientState,
 * which also set the array's bit at GL_FAST_ARRAYS), and each was a trip through the NT
 * layer (the syscall, the unix-call dispatch, opengl32's Unix side) to
 * append 12 to 16 bytes to the ring. Here they are those bytes: the same
 * command the layer writes for them (gex_kernel::nt::opengl's `hot_one`).
 *
 * The ring is the layer's: it publishes the ring's address at GL_FAST_RING
 * once the device is open (0 before, and with GEX_NT_GL_FAST=0), and the
 * tail at GL_FAST_TAIL, whose top bit is the ring's lock (the layer holds it
 * while it writes a command or submits). Anything else -- another library,
 * another GL function, no ring yet, the lock held, a full ring -- is the
 * trampoline's own work: the UNIX_CALL syscall, made as the trampoline
 * makes it (`mov eax, UNIX_CALL; call 1f; ret 16; 1: int 0x80; ret`).
 *
 * stdcall (handle: u64, code, params), 16 bytes popped; returns NTSTATUS. */

/* gex_kernel::win::TRAMPOLINES and the i386 unix-call trampoline in it. */
#define TR 0x3f0000u
#define TR_RET16 (TR + 0x6au)  /* the `ret 16` the inner call returns to */
#define TR_INT80 (TR + 0x6du)  /* `int 0x80` */
#define TR_RET (TR + 0x6fu)    /* the `ret` after it */
#define UNIX_CALL_NR 0x7f000001u /* gex_kernel::win::nr::UNIX_CALL */

/* gex_kernel::nt::opengl: the fast path's words and the ring's shape. */
#define GL_FAST_RING (GP_KUSER + 0xfe0u)
#define GL_FAST_TAIL (GP_KUSER + 0xfe4u)
#define TAIL_BUSY 0x80000000u
/* The client arrays' flags (bit = array index) and, at ARRAYS_TEX_SHIFT,
 * the client active texture unit: the layer's `arrays[i].enabled`, which
 * it reads back before a draw. */
#define GL_FAST_ARRAYS (GP_KUSER + 0xfe8u)
#define ARRAYS_TEX_SHIFT 16
/* The array state the layer shares with the provider (gex_kernel::nt::
 * opengl::GL_FAST_STATE and the FS_ words): GL_ARRAY_BUFFER's binding,
 * whether a generic attribute reads client memory, then per fixed array
 * size, type, stride, pointer, buffer. 0 without one (x64, or no fast path). */
#define GL_FAST_STATE (GP_KUSER + 0xfecu)
#define FS_ARRAY_BUFFER 0u
#define FS_ATTRIB_CLIENT 4u
#define FS_ARRAYS 16u
#define FS_ARRAY_WORDS 5u
#define A_VERTEX 0
#define A_COLOR 1
#define A_TEXCOORD 2
#define TEX_UNITS 8
#define A_NORMAL 10
#define A_SECONDARY 11
#define A_FOG 12
#define RING_HEADER 32u
#define CMD_CAPACITY (8u << 20)
/* opengl32's unixlib handle ((3 << 16) | 1, gex_kernel::nt::unixcall). */
#define GL_HANDLE 0x30001u

/* opengl32's unix-call codes (`enum unix_funcs`, gex_kernel::nt::opengl::
 * table::FUNCS) and gex-gl's opcodes (gex_gl::proto::op). Checked against
 * both by gex-kernel's test `nt_unixcall_provider_constants`. */
#define FN_glBindTexture 14u
#define FN_glDisable 72u
#define FN_glEnable 81u
#define FN_glActiveTexture 350u
#define FN_glUniform1i 2524u
#define FN_glUniform2f 2542u
#define FN_glUseProgram 2645u
#define FN_glVertexPointer 343u
#define FN_glColorPointer 60u
#define FN_glTexCoordPointer 298u
#define FN_glDrawArrays 74u
#define FN_glBlendFunc 16u
#define FN_glBlendFuncSeparate 463u
#define FN_glViewport 344u
#define FN_glTexParameteri 313u
#define FN_glUniform1f 2520u
#define FN_glEnableClientState 82u
#define FN_glDisableClientState 73u
#define OP_ENABLE 19u
#define OP_DISABLE 20u
#define OP_USE_PROGRAM 55u
#define OP_UNIFORM_1I 64u
#define OP_UNIFORM_2F 66u
#define OP_BIND_BUFFER 34u
#define OP_DRAW_ARRAYS 112u
#define OP_VERTEX_POINTER 187u
#define OP_COLOR_POINTER 188u
#define OP_TEX_COORD_POINTER 189u
#define OP_CLIENT_ACTIVE_TEXTURE 190u
#define OP_CLIENT_ARRAY 200u
#define OP_VIEWPORT 18u
#define OP_BLEND_FUNC 21u
#define OP_BLEND_FUNC_SEPARATE 37u
#define OP_UNIFORM_1F 65u
#define OP_TEX_PARAMETER_I 100u
#define OP_BIND_TEXTURE 98u
#define OP_ACTIVE_TEXTURE 99u
#define OP_ENABLE_CLIENT_STATE 185u
#define OP_DISABLE_CLIENT_STATE 186u

/* Append one command of `n` argument words (1 to 4) taken from the params
 * struct (after its TEB pointer). 1 when it is in the ring. */
GEX_INLINE int gl_put(uint32_t op, uint32_t params, uint32_t n) {
    uint32_t ring = __atomic_load_n(GA(GL_FAST_RING), __ATOMIC_ACQUIRE);
    if (!ring) {
        return 0;
    }
    uint32_t len = 8u + 4u * n;
    uint32_t t = __atomic_load_n(GA(GL_FAST_TAIL), __ATOMIC_ACQUIRE);
    if ((t & TAIL_BUSY) || t + len > CMD_CAPACITY) {
        return 0;
    }
    if (!__atomic_compare_exchange_n(GA(GL_FAST_TAIL), &t, t | TAIL_BUSY, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        return 0;
    }
    uint32_t at = ring + RING_HEADER + t;
    GW(at) = op;
    GW(at + 4u) = len;
    for (uint32_t i = 0; i < n; ++i) {
        GW(at + 8u + 4u * i) = GW(params + 4u + 4u * i);
    }
    __atomic_store_n(GA(GL_FAST_TAIL), t + len, __ATOMIC_RELEASE);
    return 1;
}

/* glEnableClientState/glDisableClientState: the array's bit in
 * GL_FAST_ARRAYS, and the command, as the layer does both. -1 for an array
 * the layer does not know (it reports it). */
GEX_INLINE int array_bit(uint32_t cap, uint32_t w) {
    switch (cap) {
    case 0x8074u: return A_VERTEX;
    case 0x8076u: return A_COLOR;
    case 0x8078u: return A_TEXCOORD + (int)((w >> ARRAYS_TEX_SHIFT) & 15u);
    case 0x8075u: return A_NORMAL;
    case 0x845eu: return A_SECONDARY;
    case 0x8457u: return A_FOG;
    default: return -1;
    }
}
GEX_INLINE int client_state(uint32_t params, int on) {
    uint32_t w = GW(GL_FAST_ARRAYS);
    int bit = array_bit(GW(params + 4u), w);
    if (bit < 0 || ((w >> ARRAYS_TEX_SHIFT) & 15u) >= TEX_UNITS) {
        return 0;
    }
    if (!gl_put(on ? OP_ENABLE_CLIENT_STATE : OP_DISABLE_CLIENT_STATE, params, 1)) {
        return 0;
    }
    GW(GL_FAST_ARRAYS) = on ? (w | (1u << bit)) : (w & ~(1u << bit));
    return 1;
}

/* glVertexPointer, glColorPointer, glTexCoordPointer into client memory
 * (no GL_ARRAY_BUFFER bound): pure state, which the layer would only have
 * recorded. Written into the shared state, where the layer reads it back
 * before it uses the arrays and where the fast draw below reads it. With a
 * buffer bound the layer sends the pointer command, so that is its job. */
GEX_INLINE int set_pointer(uint32_t code, uint32_t params) {
    uint32_t st = __atomic_load_n(GA(GL_FAST_STATE), __ATOMIC_ACQUIRE);
    if (!st || GW(st + FS_ARRAY_BUFFER) != 0) {
        return 0;
    }
    uint32_t i;
    if (code == FN_glVertexPointer) {
        i = A_VERTEX;
    } else if (code == FN_glColorPointer) {
        i = A_COLOR;
    } else {
        uint32_t unit = (GW(GL_FAST_ARRAYS) >> ARRAYS_TEX_SHIFT) & 15u;
        if (unit >= TEX_UNITS) {
            return 0;
        }
        i = A_TEXCOORD + unit;
    }
    uint32_t at = st + FS_ARRAYS + 4u * FS_ARRAY_WORDS * i;
    GW(at) = GW(params + 4u);       /* size */
    GW(at + 4u) = GW(params + 8u);  /* type */
    GW(at + 8u) = GW(params + 12u); /* stride */
    GW(at + 12u) = GW(params + 16u); /* pointer */
    GW(at + 16u) = 0u;              /* buffer */
    return 1;
}

/* gex_kernel::nt::opengl's element_bytes. */
GEX_INLINE uint32_t element_bytes(int32_t size, uint32_t ty) {
    uint32_t comp;
    switch (ty) {
    case 0x1400u: case 0x1401u: comp = 1; break;
    case 0x1402u: case 0x1403u: case 0x140bu: comp = 2; break;
    case 0x1404u: case 0x1405u: case 0x1406u: case 0x140cu: comp = 4; break;
    case 0x140au: comp = 8; break;
    default: return 0;
    }
    uint32_t n = size == 0x80e1 ? 4u : size < 0 ? 0u : size > 4 ? 4u : (uint32_t)size;
    return comp * n;
}

/* glDrawArrays, as the layer does it: each enabled client-memory array's
 * bytes as an op::CLIENT_ARRAY (texture coordinates with their unit made
 * the client active texture first), GL_ARRAY_BUFFER and the client active
 * texture put back, then the draw. Only the vertex, colour and texture
 * coordinate arrays; anything else (a normal, secondary colour or fog array
 * in client memory, a generic attribute in client memory, a ring without
 * room) is the layer's. */
#define MAX_CLIENT_BYTES (1u << 20)
GEX_INLINE int draw_arrays(uint32_t params) {
    uint32_t ring = __atomic_load_n(GA(GL_FAST_RING), __ATOMIC_ACQUIRE);
    uint32_t st = __atomic_load_n(GA(GL_FAST_STATE), __ATOMIC_ACQUIRE);
    if (!ring || !st || GW(st + FS_ATTRIB_CLIENT) != 0) {
        return 0;
    }
    uint32_t mode = GW(params + 4u);
    int32_t first = (int32_t)GW(params + 8u), count = (int32_t)GW(params + 12u);
    uint32_t w = GW(GL_FAST_ARRAYS);
    uint32_t unit = (w >> ARRAYS_TEX_SHIFT) & 15u;
    if (unit >= TEX_UNITS) {
        return 0;
    }
    uint32_t vertices = first >= 0 && count > 0 ? (uint32_t)first + (uint32_t)count : 0u;
    /* What it will write, and that it may. */
    uint32_t len = 20u, any = 0;
    for (uint32_t i = 0; i <= A_FOG; ++i) {
        if (!(w >> i & 1u)) {
            continue;
        }
        uint32_t at = st + FS_ARRAYS + 4u * FS_ARRAY_WORDS * i;
        if (GW(at + 16u) != 0 || GW(at + 12u) == 0) {
            continue; /* in a buffer, or no pointer: nothing to upload */
        }
        if (i >= A_NORMAL || !vertices) {
            return 0;
        }
        uint32_t elem = element_bytes((int32_t)GW(at), GW(at + 4u));
        int32_t stride = (int32_t)GW(at + 8u);
        if (!elem || stride < 0) {
            return 0;
        }
        uint64_t n = (uint64_t)(vertices - 1u) * (stride > 0 ? (uint32_t)stride : elem) + elem;
        if (n > MAX_CLIENT_BYTES) {
            return 0;
        }
        len += (i >= A_TEXCOORD ? 12u : 0u) + 8u + 4u * 7u + (((uint32_t)n + 3u) & ~3u);
        any |= 1u << i;
    }
    if (any) {
        len += 16u + 12u;
    }
    uint32_t t = __atomic_load_n(GA(GL_FAST_TAIL), __ATOMIC_ACQUIRE);
    if ((t & TAIL_BUSY) || t + len > CMD_CAPACITY) {
        return 0;
    }
    if (!__atomic_compare_exchange_n(GA(GL_FAST_TAIL), &t, t | TAIL_BUSY, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        return 0;
    }
    uint32_t at = ring + RING_HEADER + t;
    for (uint32_t i = 0; i <= A_TEXCOORD + TEX_UNITS - 1u; ++i) {
        if (!(any >> i & 1u)) {
            continue;
        }
        uint32_t s = st + FS_ARRAYS + 4u * FS_ARRAY_WORDS * i;
        uint32_t op = i == A_VERTEX ? OP_VERTEX_POINTER : i == A_COLOR ? OP_COLOR_POINTER : OP_TEX_COORD_POINTER;
        if (i >= A_TEXCOORD) {
            GW(at) = OP_CLIENT_ACTIVE_TEXTURE, GW(at + 4u) = 12u, GW(at + 8u) = 0x84c0u + (i - A_TEXCOORD);
            at += 12u;
        }
        /* Checked above: an element size, a stride >= 0, and at most
         * MAX_CLIENT_BYTES. (Recomputed: a provider has no stack.) */
        uint32_t elem = element_bytes((int32_t)GW(s), GW(s + 4u)), stride = GW(s + 8u);
        uint32_t n = (vertices - 1u) * (stride ? stride : elem) + elem, padded = (n + 3u) & ~3u;
        GW(at) = OP_CLIENT_ARRAY;
        GW(at + 4u) = 8u + 4u * 7u + padded;
        GW(at + 8u) = i;      /* slot */
        GW(at + 12u) = op;    /* the pointer command */
        GW(at + 16u) = 3u;    /* its words before the pointer */
        GW(at + 20u) = GW(s); /* size, type, stride */
        GW(at + 24u) = GW(s + 4u);
        GW(at + 28u) = GW(s + 8u);
        GW(at + 32u) = n;
        __builtin_memmove(GEX_P(at + 36u), GEX_P(GW(s + 12u)), n);
        for (uint32_t k = n; k < padded; ++k) {
            *(uint8_t *)GEX_P(at + 36u + k) = 0;
        }
        at += 8u + 4u * 7u + padded;
    }
    if (any) {
        GW(at) = OP_BIND_BUFFER, GW(at + 4u) = 16u, GW(at + 8u) = 0x8892u, GW(at + 12u) = GW(st + FS_ARRAY_BUFFER);
        GW(at + 16u) = OP_CLIENT_ACTIVE_TEXTURE, GW(at + 20u) = 12u, GW(at + 24u) = 0x84c0u + unit;
        at += 28u;
    }
    GW(at) = OP_DRAW_ARRAYS, GW(at + 4u) = 20u;
    GW(at + 8u) = mode, GW(at + 12u) = (uint32_t)first, GW(at + 16u) = (uint32_t)count;
    __atomic_store_n(GA(GL_FAST_TAIL), t + len, __ATOMIC_RELEASE);
    return 1;
}

static GEX_PROVIDER(p_unix_call) {
    (void)need;
#ifndef GEX_KUSER_GUARD
    /* The fast path is the NT layer's: only its opengl publishes the ring
     * at GL_FAST_RING. Under W3's fold GL goes through Wine's opengl32 Unix
     * side, nothing publishes these words, and Windows' old page they were
     * in is not there: every call is the trampoline's own work below, which
     * is what a ring of 0 always made it. */
    if (gp_arg(ctx, 0) == GL_HANDLE && gp_arg(ctx, 1) == 0) {
        uint32_t code = gp_arg(ctx, 2), params = gp_arg(ctx, 3), op = 0, n = 0;
        switch (code) {
        case FN_glEnable: op = OP_ENABLE, n = 1; break;
        case FN_glDisable: op = OP_DISABLE, n = 1; break;
        case FN_glActiveTexture: op = OP_ACTIVE_TEXTURE, n = 1; break;
        case FN_glUseProgram: op = OP_USE_PROGRAM, n = 1; break;
        case FN_glBindTexture: op = OP_BIND_TEXTURE, n = 2; break;
        case FN_glUniform1i: op = OP_UNIFORM_1I, n = 2; break;
        case FN_glUniform2f: op = OP_UNIFORM_2F, n = 3; break;
        /* The next most frequent in Noita's frame after the seven above
         * (GEX_NT_GL_STATS: 39, 8, 9, 12 and 14 a frame), all commands
         * with no layer state, as the layer's `hot_one` writes them. */
        case FN_glBlendFunc: op = OP_BLEND_FUNC, n = 2; break;
        case FN_glBlendFuncSeparate: op = OP_BLEND_FUNC_SEPARATE, n = 4; break;
        case FN_glViewport: op = OP_VIEWPORT, n = 4; break;
        case FN_glTexParameteri: op = OP_TEX_PARAMETER_I, n = 3; break;
        case FN_glUniform1f: op = OP_UNIFORM_1F, n = 2; break;
        case FN_glVertexPointer:
        case FN_glColorPointer:
        case FN_glTexCoordPointer:
            if (params >= 0x10000u && set_pointer(code, params)) {
                return gp_return_eax(ctx, ret, 16, 0);
            }
            break;
        case FN_glDrawArrays:
            if (params >= 0x10000u && draw_arrays(params)) {
                return gp_return_eax(ctx, ret, 16, 0);
            }
            break;
        case FN_glEnableClientState:
        case FN_glDisableClientState:
            if (params >= 0x10000u && client_state(params, code == FN_glEnableClientState)) {
                return gp_return_eax(ctx, ret, 16, 0);
            }
            break;
        default: break;
        }
        if (n && params >= 0x10000u && gl_put(op, params, n)) {
            return gp_return_eax(ctx, ret, 16, 0);
        }
    }
#endif
    /* The trampoline: its inner call's return address pushed, the syscall
     * made at its `int 0x80` (a call that does not come straight back
     * leaves from inside gex_syscall, the guest at TR_RET with this very
     * stack), then its `ret` and `ret 16`. */
    (void)depth;
    uint32_t sp = gp_reg(ctx, R_ESP) - 4u;
    GW(sp) = TR_RET16;
    gp_set(ctx, R_ESP, sp);
    gp_set(ctx, R_EAX, UNIX_CALL_NR);
    GEX_C64(GEX_CTX_PC) = TR_RET;
    gex_syscall(ctx, (int64_t)TR_INT80);
    gp_set(ctx, R_ESP, gp_reg(ctx, R_ESP) + 4u);
    return gp_return(ctx, ret, 16);
}

GEX_PROVIDE(p_unix_call, "unix_call_32")
