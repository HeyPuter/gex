/* From pewasm (port/src/noita_lua_libs.c).
 * LuaJIT's built-in modules for the native Lua binding: `bit` (LuaBitOp
 * semantics), a `jit` table reporting an interpreter, and an `ffi` module
 * that fails loudly when used. */
#include "lua/lua.h"
#include "lua/lauxlib.h"
#include <stdint.h>
#include <string.h>

typedef int32_t SBits;
typedef uint32_t UBits;

/* LuaBitOp's normalization: numbers wrap modulo 2^32 to a signed 32-bit int. */
static UBits barg(lua_State *L, int index) {
    union {
        lua_Number n;
        uint64_t b;
    } bn;
    bn.n = luaL_checknumber(L, index) + 6755399441055744.0; /* 2^52+2^51 */
    return (UBits)bn.b;
}
#define BRET(b)                                                                                                                            \
    lua_pushnumber(L, (lua_Number)(SBits)(b));                                                                                             \
    return 1;

static int tobit(lua_State *L) {
    BRET(barg(L, 1))
}
static int bnot(lua_State *L) {
    BRET(~barg(L, 1))
}
static int band(lua_State *L) {
    int i;
    UBits b = barg(L, 1);
    for (i = lua_gettop(L); i > 1; i--) b &= barg(L, i);
    BRET(b)
}
static int bor(lua_State *L) {
    int i;
    UBits b = barg(L, 1);
    for (i = lua_gettop(L); i > 1; i--) b |= barg(L, i);
    BRET(b)
}
static int bxor(lua_State *L) {
    int i;
    UBits b = barg(L, 1);
    for (i = lua_gettop(L); i > 1; i--) b ^= barg(L, i);
    BRET(b)
}
static int lshift(lua_State *L) {
    UBits b = barg(L, 1), n = barg(L, 2) & 31;
    BRET(b << n)
}
static int rshift(lua_State *L) {
    UBits b = barg(L, 1), n = barg(L, 2) & 31;
    BRET(b >> n)
}
static int arshift(lua_State *L) {
    UBits b = barg(L, 1), n = barg(L, 2) & 31;
    BRET((SBits)b >> n)
}
static int rol(lua_State *L) {
    UBits b = barg(L, 1), n = barg(L, 2) & 31;
    BRET((b << n) | (b >> ((32 - n) & 31)))
}
static int ror(lua_State *L) {
    UBits b = barg(L, 1), n = barg(L, 2) & 31;
    BRET((b >> n) | (b << ((32 - n) & 31)))
}
static int bswap(lua_State *L) {
    UBits b = barg(L, 1);
    b = (b >> 24) | ((b >> 8) & 0xff00) | ((b & 0xff00) << 8) | (b << 24);
    BRET(b)
}
static int tohex(lua_State *L) {
    UBits b = barg(L, 1);
    SBits n = lua_isnone(L, 2) ? 8 : (SBits)barg(L, 2);
    const char *hexdigits = "0123456789abcdef";
    char buf[8];
    int i;
    if (n < 0) {
        n = -n;
        hexdigits = "0123456789ABCDEF";
    }
    if (n > 8) n = 8;
    for (i = (int)n; --i >= 0;) {
        buf[i] = hexdigits[b & 15];
        b >>= 4;
    }
    lua_pushlstring(L, buf, (size_t)n);
    return 1;
}
static const luaL_Reg bit_functions[] = {
    { "tobit", tobit }, { "bnot", bnot },       { "band", band },   { "bor", bor },     { "bxor", bxor },   { "lshift", lshift },
    { "rshift", rshift }, { "arshift", arshift }, { "rol", rol },   { "ror", ror },     { "bswap", bswap }, { "tohex", tohex },
    { NULL, NULL }
};
int nl_open_bit(lua_State *L) {
    luaL_register(L, "bit", bit_functions);
    return 1;
}

/* The interpreter is always on and there is no trace compiler to control. */
static int jit_noop(lua_State *L) {
    (void)L;
    return 0;
}
static int jit_status(lua_State *L) {
    lua_pushboolean(L, 0);
    return 1;
}
static const luaL_Reg jit_functions[] = {
    { "on", jit_noop }, { "off", jit_noop }, { "flush", jit_noop }, { "status", jit_status }, { NULL, NULL }
};
int nl_open_jit(lua_State *L) {
    luaL_register(L, "jit", jit_functions);
    lua_pushliteral(L, "LuaJIT 2.0.4");
    lua_setfield(L, -2, "version");
    lua_pushnumber(L, 20004);
    lua_setfield(L, -2, "version_num");
    lua_pushliteral(L, "Windows");
    lua_setfield(L, -2, "os");
    lua_pushliteral(L, "x86");
    lua_setfield(L, -2, "arch");
    return 1;
}

static int ffi_unavailable(lua_State *L) {
    return luaL_error(L, "ffi is not available in the ejected project (PUC Lua)");
}
int nl_open_ffi(lua_State *L) {
    lua_newtable(L);
    lua_newtable(L);
    lua_pushcfunction(L, ffi_unavailable);
    lua_setfield(L, -2, "__index");
    lua_setmetatable(L, -2);
    return 1;
}
