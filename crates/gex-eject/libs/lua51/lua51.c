/* lua51.dll (LuaJIT 2.0.4's API) as a gex stateful export provider, on PUC
 * Lua 5.1.5 (docs/eject-pe.md §5). Ported from pewasm's port/src/noita_lua.c.
 *
 * Each export is an F_body (gex_body_t) the host binds at the export's guest
 * address: every caller -- noita.exe's IAT calls, GetProcAddress pointers --
 * reaches it through the function table. The C API is cdecl: arguments on
 * the guest stack, the result in EAX (ST0 for a lua_Number), the caller
 * pops. Every export lua51.dll has and this file does not provide is bound
 * to gexp__missing, which stops the run naming it.
 *
 * Lua's memory is guest memory (the provider's data and heap are guest
 * pages from the NT allocator), so every pointer Lua hands out -- a
 * lua_State, a string, lua_topointer -- is a guest address as it is, and
 * the guest reads what it points at directly.
 *
 * Guest lua_CFunctions are stored tagged (bit 31 of the guest address, as
 * pewasm does) and called back through the function table with the cdecl
 * convention by nl_call_cfunction, on the guest thread's context and stack.
 * A Lua error raised under them unwinds the guest frames too (ldo.c:
 * gex_provider_throw), and nl_guest_restore puts back the guest registers
 * those frames left behind. */
#include "gex_abi.h"
#include "provider.h"
#include "lua/lua.h"
#include "lua/lauxlib.h"
#include "lua/lualib.h"
#include <string.h>

/* An export: `name` is both the C function (gexp_<name>, which the host
 * finds in the element segment by its name) and the export it binds. */
#define LUA_EXPORT(name)                                                                                               \
    static GEX_PROVIDER(gexp_##name);                                                                                  \
    __attribute__((export_name("gexp_ptr_" #name))) gex_body_t gexp_ptr_##name(void) { return gexp_##name; }          \
    static GEX_PROVIDER(gexp_##name)

__attribute__((import_module("gex"), import_name("provider_missing"))) void gex_provider_missing(int addr);
__attribute__((import_module("gex"), import_name("provider_ctx"))) int gex_provider_ctx(void);
__attribute__((import_module("gex"), import_name("provider_log"))) void gex_provider_log(const char *p, int n);

/* What every unprovided export is bound to. */
static GEX_PROVIDER(gexp__missing) {
    (void)depth, (void)need, (void)ret;
    gex_provider_missing((int)GP_SELF_PC());
    __builtin_trap();
}
__attribute__((export_name("gexp_ptr__missing"))) gex_body_t gexp_ptr__missing(void) { return gexp__missing; }

/* lua51.dll's entry point (DllMain, stdcall, three arguments): the library
 * it would initialise -- LuaJIT's, and the C runtime under it -- is not the
 * one that runs, so there is nothing to do. */
static GEX_PROVIDER(gexp__entry) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 12, 1);
}
__attribute__((export_name("gexp_ptr__entry"))) gex_body_t gexp_ptr__entry(void) { return gexp__entry; }

#define ARG(i) gp_arg(ctx, (i))
#define STATE(i) ((lua_State *)(uintptr_t)ARG(i))
#define STR(i) ((const char *)(uintptr_t)ARG(i))
#define RET(v) return gp_return_eax(ctx, ret, 0, (uint32_t)(v))
#define RETV() return gp_return(ctx, ret, 0)
#define UNUSED() (void)need
static double arg_double(int ctx, unsigned i) { return gp_argf64(ctx, 4 * i); }
static int ret_double(int ctx, uint64_t ret, double v) {
    gp_push_st0(ctx, v);
    return gp_return(ctx, ret, 0);
}

/* ---- the guest thread a Lua call runs on ------------------------------- */

/* Per guest thread (by context address): the call depth of the innermost
 * export call, which a guest C function called from Lua runs under. */
#define THREADS 2048
static struct {
    int ctx, depth;
} threads[THREADS];
static unsigned slot_of(int ctx) {
    unsigned h = ((unsigned)ctx >> 6) & (THREADS - 1);
    for (unsigned i = 0; i < THREADS; ++i) {
        unsigned s = (h + i) & (THREADS - 1);
        int have = __atomic_load_n(&threads[s].ctx, __ATOMIC_ACQUIRE);
        if (have == ctx) {
            return s;
        }
        if (have == 0) {
            int z = 0;
            if (__atomic_compare_exchange_n(&threads[s].ctx, &z, ctx, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE) || z == ctx) {
                return s;
            }
        }
    }
    __builtin_trap();
}
typedef struct {
    unsigned slot;
    int depth;
} nl_enter_t;
static nl_enter_t enter(int ctx, int32_t depth) {
    unsigned s = slot_of(ctx);
    nl_enter_t saved = { s, threads[s].depth };
    threads[s].depth = depth;
    return saved;
}
static void leave(nl_enter_t e) { threads[e.slot].depth = e.depth; }
#define ENTER() nl_enter_t entered_ = enter(ctx, depth)
#define LEAVE() leave(entered_)

/* The guest state a longjmp past guest frames abandons: the registers a
 * cdecl callee preserves and the stack pointer, the SEH chain, the x87 top,
 * and the export depth. */
void nl_guest_save(nl_guest_frame *f) {
    int ctx = gex_provider_ctx();
    f->ctx = ctx;
    for (unsigned r = 0; r < 8; ++r) {
        f->regs[r] = gp_reg(ctx, r);
    }
    f->teb_except = GW((uint32_t)GEX_C64(GEX_CTX_TPIDR));
    f->x87_top = GEX_C32(GEX_CTX_X87_TOP);
    f->depth = threads[slot_of(ctx)].depth;
}
void nl_guest_restore(const nl_guest_frame *f) {
    int ctx = f->ctx;
    /* EBX, ESP, EBP, ESI, EDI: what the frames that were skipped may have
     * changed and their callers rely on. */
    for (unsigned r = 3; r < 8; ++r) {
        gp_set(ctx, r, f->regs[r]);
    }
    GW((uint32_t)GEX_C64(GEX_CTX_TPIDR)) = f->teb_except;
    GEX_C32(GEX_CTX_X87_TOP) = f->x87_top;
    threads[slot_of(ctx)].depth = (int)f->depth;
}

#define GUEST_TAG 0x80000000u
static lua_CFunction guest_function(uint32_t address) {
    return address ? (lua_CFunction)(uintptr_t)(address | GUEST_TAG) : NULL;
}
/* A guest x86 function's address with its tag: never a valid table index
 * (the function table is far smaller), so native and guest C functions
 * cannot be confused. */
int nl_call_cfunction(lua_State *L, lua_CFunction f) {
    uintptr_t value = (uintptr_t)f;
    if (!(value & GUEST_TAG)) {
        return f(L);
    }
    uint32_t fn = (uint32_t)(value & ~GUEST_TAG);
    int ctx = gex_provider_ctx();
    int32_t depth = threads[slot_of(ctx)].depth + 1;
    /* cdecl: push L and a return address of our own, call, pop L. */
    uint32_t sp = gp_reg(ctx, R_ESP) - 8, ra = fn ^ 1u;
    GW(sp) = ra;
    GW(sp + 4) = (uint32_t)(uintptr_t)L;
    gp_set(ctx, R_ESP, sp);
    GEX_C64(GEX_CTX_LAST_TARGET) = fn;
    if (!GEX_FTAB(gex_ftab_idx(fn))(ctx, depth, 0, ra)) {
        /* The guest left by another way than returning (an exception
         * unwinding past Lua): beyond what Lua's frames can follow. */
        static const char m[] = "lua51 provider: a guest C function called from Lua did not return\n";
        gex_provider_log(m, sizeof m - 1);
        __builtin_trap();
    }
    gp_set(ctx, R_ESP, gp_reg(ctx, R_ESP) + 4);
    return (int)gp_reg(ctx, R_EAX);
}

void nl_unprotected(lua_State *L) {
    const char *msg = lua_tostring(L, -1);
    static const char m[] = "lua51 provider: unprotected Lua error: ";
    gex_provider_log(m, sizeof m - 1);
    if (msg) {
        gex_provider_log(msg, (int)strlen(msg));
    }
    gex_provider_log("\n", 1);
    __builtin_trap();
}

/* ---- state -------------------------------------------------------------- */

extern void *realloc(void *, size_t);
extern void free(void *);
static void *allocate(void *ud, void *block, size_t old, size_t size) {
    (void)ud, (void)old;
    if (size == 0) {
        free(block);
        return NULL;
    }
    return realloc(block, size);
}
static int panic(lua_State *L) {
    nl_unprotected(L);
    return 0;
}
extern int nl_open_bit(lua_State *L);
extern int nl_open_jit(lua_State *L);
extern int nl_open_ffi(lua_State *L);

LUA_EXPORT(luaL_newstate) {
    UNUSED();
    (void)depth;
    lua_State *L = lua_newstate(allocate, NULL);
    if (L) {
        lua_atpanic(L, panic);
    }
    RET(L);
}
LUA_EXPORT(lua_close) {
    UNUSED();
    ENTER();
    lua_close(STATE(0));
    LEAVE();
    RETV();
}
static void open_library(lua_State *L, const char *name, lua_CFunction open) {
    lua_pushcfunction(L, open);
    lua_pushstring(L, name);
    lua_call(L, 1, 0);
}
LUA_EXPORT(luaL_openlibs) {
    UNUSED();
    ENTER();
    lua_State *L = STATE(0);
    luaL_openlibs(L);
    open_library(L, "bit", nl_open_bit);
    open_library(L, "jit", nl_open_jit);
    /* LuaJIT preloads ffi rather than opening it. */
    lua_getglobal(L, "package");
    lua_getfield(L, -1, "preload");
    lua_pushcfunction(L, nl_open_ffi);
    lua_setfield(L, -2, "ffi");
    lua_pop(L, 2);
    LEAVE();
    RETV();
}
#define OPEN(name, function)                                                                                           \
    LUA_EXPORT(name) {                                                                                                 \
        UNUSED();                                                                                                      \
        ENTER();                                                                                                       \
        int r = function(STATE(0));                                                                                    \
        LEAVE();                                                                                                       \
        RET(r);                                                                                                        \
    }
OPEN(luaopen_base, luaopen_base)
OPEN(luaopen_math, luaopen_math)
OPEN(luaopen_string, luaopen_string)
OPEN(luaopen_table, luaopen_table)
OPEN(luaopen_io, luaopen_io)
OPEN(luaopen_os, luaopen_os)
OPEN(luaopen_package, luaopen_package)
OPEN(luaopen_debug, luaopen_debug)
OPEN(luaopen_bit, nl_open_bit)
OPEN(luaopen_jit, nl_open_jit)
OPEN(luaopen_ffi, nl_open_ffi)

/* ---- stack -------------------------------------------------------------- */

LUA_EXPORT(lua_gettop) {
    UNUSED();
    (void)depth;
    RET(lua_gettop(STATE(0)));
}
LUA_EXPORT(lua_settop) {
    UNUSED();
    ENTER();
    lua_settop(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_pushvalue) {
    UNUSED();
    (void)depth;
    lua_pushvalue(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_remove) {
    UNUSED();
    (void)depth;
    lua_remove(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_insert) {
    UNUSED();
    (void)depth;
    lua_insert(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_replace) {
    UNUSED();
    (void)depth;
    lua_replace(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_checkstack) {
    UNUSED();
    (void)depth;
    RET(lua_checkstack(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(luaL_checkstack) {
    UNUSED();
    ENTER();
    luaL_checkstack(STATE(0), (int)ARG(1), ARG(2) ? STR(2) : NULL);
    LEAVE();
    RETV();
}

/* ---- access ------------------------------------------------------------- */

LUA_EXPORT(lua_type) {
    UNUSED();
    (void)depth;
    RET(lua_type(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_typename) {
    UNUSED();
    (void)depth;
    RET(lua_typename(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_isstring) {
    UNUSED();
    (void)depth;
    RET(lua_isstring(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_isnumber) {
    UNUSED();
    (void)depth;
    RET(lua_isnumber(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_iscfunction) {
    UNUSED();
    (void)depth;
    RET(lua_iscfunction(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_isuserdata) {
    UNUSED();
    (void)depth;
    RET(lua_isuserdata(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_rawequal) {
    UNUSED();
    (void)depth;
    RET(lua_rawequal(STATE(0), (int)ARG(1), (int)ARG(2)));
}
LUA_EXPORT(lua_equal) {
    UNUSED();
    ENTER();
    int r = lua_equal(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_lessthan) {
    UNUSED();
    ENTER();
    int r = lua_lessthan(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_tonumber) {
    UNUSED();
    (void)depth;
    return ret_double(ctx, ret, lua_tonumber(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_tointeger) {
    UNUSED();
    (void)depth;
    RET(lua_tointeger(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_toboolean) {
    UNUSED();
    (void)depth;
    RET(lua_toboolean(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_tolstring) {
    UNUSED();
    ENTER();
    size_t length = 0;
    const char *p = lua_tolstring(STATE(0), (int)ARG(1), &length);
    if (ARG(2)) {
        GW(ARG(2)) = p ? (uint32_t)length : 0;
    }
    LEAVE();
    RET(p);
}
LUA_EXPORT(lua_objlen) {
    UNUSED();
    (void)depth;
    RET(lua_objlen(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_touserdata) {
    UNUSED();
    (void)depth;
    RET(lua_touserdata(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_tothread) {
    UNUSED();
    (void)depth;
    RET(lua_tothread(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_topointer) {
    UNUSED();
    (void)depth;
    RET(lua_topointer(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_tocfunction) {
    UNUSED();
    (void)depth;
    uintptr_t f = (uintptr_t)lua_tocfunction(STATE(0), (int)ARG(1));
    /* A guest function's own address; a native one has none the guest could
     * call. */
    RET((f & GUEST_TAG) ? (f & ~GUEST_TAG) : 0);
}

/* ---- push --------------------------------------------------------------- */

LUA_EXPORT(lua_pushnil) {
    UNUSED();
    (void)depth;
    lua_pushnil(STATE(0));
    RETV();
}
LUA_EXPORT(lua_pushnumber) {
    UNUSED();
    (void)depth;
    lua_pushnumber(STATE(0), arg_double(ctx, 1));
    RETV();
}
LUA_EXPORT(lua_pushinteger) {
    UNUSED();
    (void)depth;
    lua_pushinteger(STATE(0), (int32_t)ARG(1));
    RETV();
}
LUA_EXPORT(lua_pushboolean) {
    UNUSED();
    (void)depth;
    lua_pushboolean(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_pushlightuserdata) {
    UNUSED();
    (void)depth;
    lua_pushlightuserdata(STATE(0), (void *)(uintptr_t)ARG(1));
    RETV();
}
LUA_EXPORT(lua_pushlstring) {
    UNUSED();
    ENTER();
    uint32_t length = ARG(2);
    lua_pushlstring(STATE(0), length ? STR(1) : "", length);
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_pushstring) {
    UNUSED();
    ENTER();
    lua_pushstring(STATE(0), ARG(1) ? STR(1) : NULL);
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_pushcclosure) {
    UNUSED();
    ENTER();
    lua_pushcclosure(STATE(0), guest_function(ARG(1)), (int)ARG(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_pushthread) {
    UNUSED();
    ENTER();
    int r = lua_pushthread(STATE(0));
    LEAVE();
    RET(r);
}
/* lua_pushfstring: Lua's own format subset, with cdecl varargs on the guest
 * stack. */
static const char *pushfstring(int ctx, lua_State *L, const char *format, unsigned next) {
    unsigned pieces = 0;
    const char *e;
    while ((e = strchr(format, '%')) != NULL) {
        lua_pushlstring(L, format, (size_t)(e - format));
        ++pieces;
        switch (e[1]) {
        case 's': {
            uint32_t p = ARG(next++);
            lua_pushstring(L, p ? (const char *)(uintptr_t)p : "(null)");
            break;
        }
        case 'c': {
            char c = (char)ARG(next++);
            lua_pushlstring(L, &c, 1);
            break;
        }
        case 'd':
            lua_pushnumber(L, (int32_t)ARG(next++));
            break;
        case 'f':
            lua_pushnumber(L, arg_double(ctx, next));
            next += 2;
            break;
        case 'p': {
            char buffer[16];
            uint32_t v = ARG(next++);
            static const char hex[] = "0123456789abcdef";
            int n = 0;
            buffer[n++] = '0';
            buffer[n++] = 'x';
            int started = 0;
            for (int sh = 28; sh >= 0; sh -= 4) {
                unsigned d = (v >> sh) & 15;
                if (d || started || sh == 0) {
                    buffer[n++] = hex[d];
                    started = 1;
                }
            }
            lua_pushlstring(L, buffer, (size_t)n);
            break;
        }
        case '%':
            lua_pushliteral(L, "%");
            break;
        default: {
            char buffer[2] = { '%', e[1] };
            lua_pushlstring(L, buffer, 2);
        }
        }
        ++pieces;
        format = e + 2;
    }
    lua_pushstring(L, format);
    lua_concat(L, (int)pieces + 1);
    return lua_tostring(L, -1);
}
LUA_EXPORT(lua_pushfstring) {
    UNUSED();
    ENTER();
    const char *r = pushfstring(ctx, STATE(0), STR(1), 2);
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_concat) {
    UNUSED();
    ENTER();
    lua_concat(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}

/* ---- tables ------------------------------------------------------------- */

LUA_EXPORT(lua_createtable) {
    UNUSED();
    ENTER();
    lua_createtable(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_newuserdata) {
    UNUSED();
    ENTER();
    void *p = lua_newuserdata(STATE(0), ARG(1));
    LEAVE();
    RET(p);
}
LUA_EXPORT(lua_gettable) {
    UNUSED();
    ENTER();
    lua_gettable(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_settable) {
    UNUSED();
    ENTER();
    lua_settable(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_getfield) {
    UNUSED();
    ENTER();
    lua_getfield(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_setfield) {
    UNUSED();
    ENTER();
    lua_setfield(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_rawget) {
    UNUSED();
    (void)depth;
    lua_rawget(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_rawset) {
    UNUSED();
    ENTER();
    lua_rawset(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_rawgeti) {
    UNUSED();
    (void)depth;
    lua_rawgeti(STATE(0), (int)ARG(1), (int)ARG(2));
    RETV();
}
LUA_EXPORT(lua_rawseti) {
    UNUSED();
    ENTER();
    lua_rawseti(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_getmetatable) {
    UNUSED();
    (void)depth;
    RET(lua_getmetatable(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_setmetatable) {
    UNUSED();
    ENTER();
    int r = lua_setmetatable(STATE(0), (int)ARG(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_getfenv) {
    UNUSED();
    (void)depth;
    lua_getfenv(STATE(0), (int)ARG(1));
    RETV();
}
LUA_EXPORT(lua_setfenv) {
    UNUSED();
    (void)depth;
    RET(lua_setfenv(STATE(0), (int)ARG(1)));
}
LUA_EXPORT(lua_next) {
    UNUSED();
    ENTER();
    int r = lua_next(STATE(0), (int)ARG(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_ref) {
    UNUSED();
    ENTER();
    int r = luaL_ref(STATE(0), (int)ARG(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_unref) {
    UNUSED();
    (void)depth;
    luaL_unref(STATE(0), (int)ARG(1), (int)ARG(2));
    RETV();
}
LUA_EXPORT(luaL_newmetatable) {
    UNUSED();
    ENTER();
    int r = luaL_newmetatable(STATE(0), STR(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_checkudata) {
    UNUSED();
    ENTER();
    void *r = luaL_checkudata(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RET(r);
}

/* ---- calls and chunks --------------------------------------------------- */

LUA_EXPORT(lua_call) {
    UNUSED();
    ENTER();
    lua_call(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(lua_pcall) {
    UNUSED();
    ENTER();
    int r = lua_pcall(STATE(0), (int)ARG(1), (int)ARG(2), (int)ARG(3));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_error) {
    UNUSED();
    ENTER();
    lua_error(STATE(0));
    LEAVE();
    RETV();
}
LUA_EXPORT(luaL_error) {
    UNUSED();
    ENTER();
    lua_State *L = STATE(0);
    luaL_where(L, 1);
    pushfstring(ctx, L, STR(1), 2);
    lua_concat(L, 2);
    lua_error(L);
    LEAVE();
    RETV();
}
LUA_EXPORT(luaL_loadstring) {
    UNUSED();
    ENTER();
    int r = luaL_loadstring(STATE(0), STR(1));
    LEAVE();
    RET(r);
}
static int loadbuffer(int ctx, lua_State *L, uint32_t buf, uint32_t size, uint32_t name, uint32_t mode) {
    const char *buffer = size ? (const char *)(uintptr_t)buf : "";
    if (mode && !strchr((const char *)(uintptr_t)mode, 't') && !(size && buffer[0] == LUA_SIGNATURE[0])) {
        lua_pushfstring(L, "attempt to load a text chunk (mode is '%s')", (const char *)(uintptr_t)mode);
        return LUA_ERRSYNTAX;
    }
    (void)ctx;
    return luaL_loadbuffer(L, buffer, size, name ? (const char *)(uintptr_t)name : NULL);
}
LUA_EXPORT(luaL_loadbufferx) {
    UNUSED();
    ENTER();
    int r = loadbuffer(ctx, STATE(0), ARG(1), ARG(2), ARG(3), ARG(4));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_loadbuffer) {
    UNUSED();
    ENTER();
    int r = loadbuffer(ctx, STATE(0), ARG(1), ARG(2), ARG(3), 0);
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_gc) {
    UNUSED();
    ENTER();
    int r = lua_gc(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_traceback) {
    /* LuaJIT's (L, L1, msg, level): the traceback of L1 pushed onto L. */
    UNUSED();
    ENTER();
    lua_State *L = STATE(0), *L1 = STATE(1);
    const char *msg = ARG(2) ? STR(2) : NULL;
    int level = (int)ARG(3);
    lua_Debug ar;
    luaL_Buffer b;
    luaL_buffinit(L, &b);
    if (msg) {
        luaL_addstring(&b, msg);
        luaL_addchar(&b, '\n');
    }
    luaL_addstring(&b, "stack traceback:");
    while (lua_getstack(L1, level++, &ar)) {
        lua_getinfo(L1, "Snl", &ar);
        luaL_addstring(&b, "\n\t");
        lua_pushfstring(L, "%s:", ar.short_src);
        luaL_addvalue(&b);
        if (ar.currentline > 0) {
            lua_pushfstring(L, "%d:", ar.currentline);
            luaL_addvalue(&b);
        }
        if (*ar.namewhat != '\0') {
            lua_pushfstring(L, " in function '%s'", ar.name);
        } else if (*ar.what == 'm') {
            lua_pushliteral(L, " in main chunk");
        } else if (*ar.what == 'C' || *ar.what == 't') {
            lua_pushliteral(L, " ?");
        } else {
            lua_pushfstring(L, " in function <%s:%d>", ar.short_src, ar.linedefined);
        }
        luaL_addvalue(&b);
    }
    luaL_pushresult(&b);
    LEAVE();
    RETV();
}

/* ---- debug -------------------------------------------------------------- */

/* lua_Debug is identical on i386 and wasm32, and its string fields are
 * guest addresses as they are. */
LUA_EXPORT(lua_getstack) {
    UNUSED();
    (void)depth;
    RET(lua_getstack(STATE(0), (int)ARG(1), (lua_Debug *)(uintptr_t)ARG(2)));
}
LUA_EXPORT(lua_getinfo) {
    UNUSED();
    ENTER();
    int r = lua_getinfo(STATE(0), STR(1), (lua_Debug *)(uintptr_t)ARG(2));
    LEAVE();
    RET(r);
}

/* ---- threads ------------------------------------------------------------ */

LUA_EXPORT(lua_newthread) {
    UNUSED();
    ENTER();
    lua_State *r = lua_newthread(STATE(0));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_resume) {
    UNUSED();
    ENTER();
    int r = lua_resume(STATE(0), (int)ARG(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_yield) {
    UNUSED();
    ENTER();
    int r = lua_yield(STATE(0), (int)ARG(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_status) {
    UNUSED();
    (void)depth;
    RET(lua_status(STATE(0)));
}
LUA_EXPORT(lua_xmove) {
    UNUSED();
    (void)depth;
    lua_xmove(STATE(0), STATE(1), (int)ARG(2));
    RETV();
}

/* ---- auxiliary ---------------------------------------------------------- */

LUA_EXPORT(luaL_checklstring) {
    UNUSED();
    ENTER();
    size_t len = 0;
    const char *p = luaL_checklstring(STATE(0), (int)ARG(1), &len);
    if (ARG(2)) {
        GW(ARG(2)) = (uint32_t)len;
    }
    LEAVE();
    RET(p);
}
LUA_EXPORT(luaL_optlstring) {
    UNUSED();
    ENTER();
    size_t len = 0;
    const char *p = luaL_optlstring(STATE(0), (int)ARG(1), ARG(2) ? STR(2) : NULL, &len);
    if (ARG(3)) {
        GW(ARG(3)) = (uint32_t)len;
    }
    LEAVE();
    RET(p);
}
LUA_EXPORT(luaL_checknumber) {
    UNUSED();
    ENTER();
    double v = luaL_checknumber(STATE(0), (int)ARG(1));
    LEAVE();
    return ret_double(ctx, ret, v);
}
LUA_EXPORT(luaL_optnumber) {
    UNUSED();
    ENTER();
    double v = luaL_optnumber(STATE(0), (int)ARG(1), arg_double(ctx, 2));
    LEAVE();
    return ret_double(ctx, ret, v);
}
LUA_EXPORT(luaL_checkinteger) {
    UNUSED();
    ENTER();
    int r = (int)luaL_checkinteger(STATE(0), (int)ARG(1));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_optinteger) {
    UNUSED();
    ENTER();
    int r = (int)luaL_optinteger(STATE(0), (int)ARG(1), (int32_t)ARG(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_checktype) {
    UNUSED();
    ENTER();
    luaL_checktype(STATE(0), (int)ARG(1), (int)ARG(2));
    LEAVE();
    RETV();
}
LUA_EXPORT(luaL_checkany) {
    UNUSED();
    ENTER();
    luaL_checkany(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}
LUA_EXPORT(luaL_argerror) {
    UNUSED();
    ENTER();
    int r = luaL_argerror(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_typerror) {
    UNUSED();
    ENTER();
    int r = luaL_typerror(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_where) {
    UNUSED();
    ENTER();
    luaL_where(STATE(0), (int)ARG(1));
    LEAVE();
    RETV();
}
LUA_EXPORT(luaL_getmetafield) {
    UNUSED();
    ENTER();
    int r = luaL_getmetafield(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(luaL_callmeta) {
    UNUSED();
    ENTER();
    int r = luaL_callmeta(STATE(0), (int)ARG(1), STR(2));
    LEAVE();
    RET(r);
}
LUA_EXPORT(lua_atpanic) {
    /* The guest's panic function is never called: an unprotected error stops
     * the run (nl_unprotected). */
    UNUSED();
    (void)depth;
    RET(0);
}
/* luaL_register(L, libname, const luaL_Reg *l): the guest's array of
 * {const char *name; lua_CFunction func} pairs, its functions tagged. */
LUA_EXPORT(luaL_register) {
    UNUSED();
    ENTER();
    lua_State *L = STATE(0);
    const char *libname = ARG(1) ? STR(1) : NULL;
    uint32_t l = ARG(2);
    if (libname) {
        /* As luaI_openlib with nup 0 does, through an empty luaL_Reg. */
        static const luaL_Reg none[] = { { NULL, NULL } };
        luaL_register(L, libname, none);
    }
    for (; l && GW(l); l += 8) {
        lua_pushcclosure(L, guest_function(GW(l + 4)), 0);
        lua_setfield(L, -2, (const char *)(uintptr_t)GW(l));
    }
    LEAVE();
    RETV();
}
