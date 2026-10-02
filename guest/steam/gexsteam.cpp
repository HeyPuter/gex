/* gex-steam: libsteam_api.so for guests (docs/games.md).
 *
 * A game linked against Valve's libsteam_api.so loads this one instead: the
 * same flat entry points and interface versions, answered here, so the game
 * runs without a Steam client. The user is logged on and owns the app; the
 * app id comes from steam_appid.txt (or SteamAppId), the persona name from
 * GEX_STEAM_NAME. Callbacks are registered and never fire: nothing
 * asynchronous happens without a Steam to talk to.
 *
 * This is the seam where a real Steam would go later: the interfaces are
 * classes in the SDK's vtable order (gen.py), so forwarding a method to the
 * host is a change to its body.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define S_API extern "C" __attribute__((visibility("default")))

static unsigned gex_steam_appid_value;

static unsigned gex_steam_appid() {
    if (!gex_steam_appid_value) {
        const char *e = getenv("SteamAppId");
        unsigned v = e ? (unsigned)strtoul(e, 0, 10) : 0;
        if (!v) {
            FILE *f = fopen("steam_appid.txt", "r");
            if (f) {
                if (fscanf(f, "%u", &v) != 1) v = 0;
                fclose(f);
            }
        }
        gex_steam_appid_value = v ? v : 480;
    }
    return gex_steam_appid_value;
}

static const char *gex_steam_name() {
    const char *n = getenv("GEX_STEAM_NAME");
    return n ? n : "gex";
}

static const char *gex_steam_dir() {
    static char dir[1024];
    if (!dir[0] && !getcwd(dir, sizeof dir)) strcpy(dir, ".");
    return dir;
}

static bool gex_steam_copy(char *buf, int size, const char *s) {
    if (!buf || size <= 0) return false;
    snprintf(buf, (size_t)size, "%s", s);
    return true;
}

static unsigned gex_steam_time() { return (unsigned)time(0); }

#include "steam_interfaces.inc"

/* An individual account in the public universe: 0x0110000100000000 | id. */
static gex_ret_CSteamID gex_steam_id() {
    gex_ret_CSteamID r;
    unsigned long long id = 0x0110000100000000ull | 1;
    memcpy(r.b, &id, sizeof id);
    return r;
}

#define GEX_INSTANCE(cls, version) static cls inst_##cls;
GEX_STEAM_INTERFACES(GEX_INSTANCE)

/* An interface nobody here implements: every slot of its vtable points at a
 * function returning zero (callers clean the stack, so any slot may take
 * any arguments), which is how an unknown version fails soft. */
static long gex_steam_zero() { return 0; }
static void *gex_steam_unknown_vtable[512];
static void *gex_steam_unknown = gex_steam_unknown_vtable;

static void *gex_find(const char *version) {
    if (!version) return 0;
#define GEX_MATCH(cls, ver) if (!strcmp(version, ver)) return &inst_##cls;
    GEX_STEAM_INTERFACES(GEX_MATCH)
    if (getenv("GEX_STEAM_TRACE")) fprintf(stderr, "gex-steam: no interface %s\n", version);
    if (!gex_steam_unknown_vtable[0])
        for (int i = 0; i < 512; i++) gex_steam_unknown_vtable[i] = (void *)gex_steam_zero;
    return &gex_steam_unknown;
}

static unsigned gex_counter = 1;

S_API bool SteamAPI_Init() { return true; }
S_API bool SteamAPI_InitSafe() { return true; }
/* ESteamAPIInitResult: k_ESteamAPIInitResult_OK. */
S_API int SteamAPI_InitFlat(char *err) { if (err) err[0] = 0; return 0; }
S_API int SteamInternal_SteamAPI_Init(const char *, char *err) { if (err) err[0] = 0; return 0; }
S_API void SteamAPI_Shutdown() {}
S_API bool SteamAPI_RestartAppIfNecessary(unsigned) { return false; }
S_API bool SteamAPI_IsSteamRunning() { return true; }
S_API const char *SteamAPI_GetSteamInstallPath() { return gex_steam_dir(); }
S_API void SteamAPI_RunCallbacks() {}
S_API void SteamAPI_RegisterCallback(void *, int) {}
S_API void SteamAPI_UnregisterCallback(void *) {}
S_API void SteamAPI_RegisterCallResult(void *, unsigned long long) {}
S_API void SteamAPI_UnregisterCallResult(void *, unsigned long long) {}
S_API int SteamAPI_GetHSteamUser() { return 1; }
S_API int SteamAPI_GetHSteamPipe() { return 1; }
S_API void SteamAPI_SetBreakpadAppID(unsigned) {}
S_API void SteamAPI_UseBreakpadCrashHandler(const char *, const char *, const char *, bool, void *, void *) {}
S_API void SteamAPI_SetMiniDumpComment(const char *) {}
S_API void SteamAPI_WriteMiniDump(unsigned, void *, unsigned) {}
S_API void SteamAPI_ReleaseCurrentThreadMemory() {}
S_API void SteamAPI_SetTryCatchCallbacks(bool) {}
S_API void SteamAPI_ManualDispatch_Init() {}
S_API void SteamAPI_ManualDispatch_RunFrame(int) {}
S_API bool SteamAPI_ManualDispatch_GetNextCallback(int, void *) { return false; }
S_API void SteamAPI_ManualDispatch_FreeLastCallback(int) {}

S_API void *SteamInternal_FindOrCreateUserInterface(int, const char *v) { return gex_find(v); }
S_API void *SteamInternal_FindOrCreateGameServerInterface(int, const char *v) { return gex_find(v); }
S_API void *SteamInternal_CreateInterface(const char *v) { return gex_find(v); }
S_API void *SteamClient() { return gex_find("SteamClient021"); }

/* `{ void (*init)(void *ctx); uintptr_t counter; void *ctx; }`: run the
 * accessor's initialiser once per counter, then hand back its slot. */
S_API void *SteamInternal_ContextInit(void *data) {
    struct ctx_init { void (*init)(void *); unsigned long counter; void *ctx; } *d = (struct ctx_init *)data;
    if (d->counter != gex_counter) {
        d->init(&d->ctx);
        d->counter = gex_counter;
    }
    return &d->ctx;
}

S_API bool SteamInternal_GameServer_Init(unsigned, unsigned short, unsigned short, unsigned short, int, const char *) { return true; }
S_API int SteamInternal_GameServer_Init_V2(unsigned, unsigned short, unsigned short, int, const char *, const char *, char *err) {
    if (err) err[0] = 0;
    return 0;
}
S_API void SteamGameServer_Shutdown() {}
S_API void SteamGameServer_RunCallbacks() {}
S_API int SteamGameServer_GetHSteamUser() { return 2; }
S_API int SteamGameServer_GetHSteamPipe() { return 2; }
S_API bool SteamGameServer_BSecure() { return false; }
S_API unsigned long long SteamGameServer_GetSteamID() { return 0x0110000100000002ull; }
