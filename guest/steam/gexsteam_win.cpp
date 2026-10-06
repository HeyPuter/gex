/* gex-steam for Windows games: steam_api.dll / steam_api64.dll with no
 * Steam client (docs/games.md, "gex-steam").
 *
 * The game's own steam_api.dll fails without a running Steam ("Steam must be
 * running to play this game"), and many games then quit or ask Steam to
 * restart them. This one answers instead: the user is logged on, owns the
 * app and its DLC, the language is English; callbacks never fire.
 *
 * Interface objects come in every version the SDK has had
 * (gen_versions.py), because a game asks for the versions its SDK knew:
 * through SteamInternal_FindOrCreateUserInterface (version strings compiled
 * into the game), through ISteamClient::GetISteam*, or, for SDKs before
 * 1.37, through accessors (SteamUser(), ...) whose versions were compiled
 * into the replaced steam_api.dll. For those the original DLL, kept beside
 * this one as steam_api[64].dll.orig (or GEX_STEAM_ORIG), is read for the
 * version strings it contains.
 *
 * Freestanding: no C runtime, only kernel32 (the build makes an import
 * library from a .def). Built with clang for the MSVC ABI (build-win.sh), so
 * methods are __thiscall on i386 with callee-popped arguments, aggregates
 * come back through a hidden pointer, and overloads are laid out as MSVC
 * lays them out.
 */
typedef __INTPTR_TYPE__ intptr_t;
typedef __UINTPTR_TYPE__ uintptr_t;

#define WINAPI __stdcall
#define S_API extern "C" __declspec(dllexport)

extern "C" {
__declspec(dllimport) unsigned long WINAPI GetEnvironmentVariableA(const char *, char *, unsigned long);
__declspec(dllimport) void *WINAPI CreateFileA(const char *, unsigned long, unsigned long, void *, unsigned long,
                                                unsigned long, void *);
__declspec(dllimport) int WINAPI ReadFile(void *, void *, unsigned long, unsigned long *, void *);
__declspec(dllimport) unsigned long WINAPI GetFileSize(void *, unsigned long *);
__declspec(dllimport) int WINAPI CloseHandle(void *);
__declspec(dllimport) unsigned long WINAPI GetCurrentDirectoryA(unsigned long, char *);
__declspec(dllimport) void WINAPI GetSystemTimeAsFileTime(unsigned long long *);
__declspec(dllimport) void *WINAPI GetStdHandle(unsigned long);
__declspec(dllimport) int WINAPI WriteFile(void *, const void *, unsigned long, unsigned long *, void *);
__declspec(dllimport) unsigned long WINAPI GetModuleFileNameA(void *, char *, unsigned long);
__declspec(dllimport) int WINAPI GetModuleHandleExA(unsigned long, const char *, void **);
__declspec(dllimport) void *WINAPI VirtualAlloc(void *, uintptr_t, unsigned long, unsigned long);
__declspec(dllimport) int WINAPI VirtualFree(void *, uintptr_t, unsigned long);

/* What the compiler may call for struct copies and float use. */
int _fltused = 0;
void *memcpy(void *d, const void *s, uintptr_t n) {
    unsigned char *o = (unsigned char *)d;
    const unsigned char *i = (const unsigned char *)s;
    while (n--) *o++ = *i++;
    return d;
}
void *memset(void *d, int c, uintptr_t n) {
    unsigned char *o = (unsigned char *)d;
    while (n--) *o++ = (unsigned char)c;
    return d;
}
}

static int gex_strlen(const char *s) {
    int n = 0;
    while (s[n]) n++;
    return n;
}

static bool gex_streq(const char *a, const char *b) {
    while (*a && *a == *b) a++, b++;
    return *a == *b;
}

static void gex_log(const char *a, const char *b = "") {
    char tr[8];
    if (!GetEnvironmentVariableA("GEX_STEAM_TRACE", tr, sizeof tr)) return;
    void *err = GetStdHandle((unsigned long)-12);
    unsigned long w;
    WriteFile(err, "gex-steam: ", 11, &w, 0);
    WriteFile(err, a, (unsigned long)gex_strlen(a), &w, 0);
    WriteFile(err, b, (unsigned long)gex_strlen(b), &w, 0);
    WriteFile(err, "\n", 1, &w, 0);
}

/* A whole small file, or 0 (VirtualAlloc'd; *len is its size). */
static unsigned char *gex_read(const char *path, unsigned long *len) {
    void *h = CreateFileA(path, 0x80000000 /* GENERIC_READ */, 1 /* FILE_SHARE_READ */, 0, 3 /* OPEN_EXISTING */,
                          0x80, 0);
    if (h == (void *)(intptr_t)-1) return 0;
    unsigned long size = GetFileSize(h, 0), got = 0;
    unsigned char *buf = 0;
    if (size != 0xffffffffu && size < (64u << 20)) {
        buf = (unsigned char *)VirtualAlloc(0, size + 1, 0x3000, 4);
        if (buf && (!ReadFile(h, buf, size, &got, 0) || got != size)) {
            VirtualFree(buf, 0, 0x8000);
            buf = 0;
        }
    }
    CloseHandle(h);
    if (buf) {
        buf[size] = 0;
        *len = size;
    }
    return buf;
}

static unsigned gex_steam_appid_value;

static unsigned gex_parse_uint(const char *s) {
    unsigned v = 0;
    while (*s == ' ' || *s == '\t') s++;
    while (*s >= '0' && *s <= '9') v = v * 10 + (unsigned)(*s++ - '0');
    return v;
}

static unsigned gex_steam_appid() {
    if (!gex_steam_appid_value) {
        char e[32];
        unsigned v = 0;
        if (GetEnvironmentVariableA("SteamAppId", e, sizeof e)) v = gex_parse_uint(e);
        if (!v) {
            unsigned long n;
            unsigned char *f = gex_read("steam_appid.txt", &n);
            if (f) {
                v = gex_parse_uint((const char *)f);
                VirtualFree(f, 0, 0x8000);
            }
        }
        gex_steam_appid_value = v ? v : 480;
    }
    return gex_steam_appid_value;
}

static const char *gex_steam_name() {
    static char name[64];
    if (!name[0] && !GetEnvironmentVariableA("GEX_STEAM_NAME", name, sizeof name)) {
        name[0] = 'g', name[1] = 'e', name[2] = 'x', name[3] = 0;
    }
    return name;
}

static const char *gex_steam_dir() {
    static char dir[1024];
    if (!dir[0] && !GetCurrentDirectoryA(sizeof dir, dir)) dir[0] = '.', dir[1] = 0;
    return dir;
}

static bool gex_steam_copy(char *buf, int size, const char *s) {
    if (!buf || size <= 0) return false;
    int i = 0;
    for (; s[i] && i < size - 1; i++) buf[i] = s[i];
    buf[i] = 0;
    return true;
}

static bool gex_steam_dlc() {
    char e[8];
    return !GetEnvironmentVariableA("GEX_STEAM_NO_DLC", e, sizeof e);
}

/* u / d by shift and subtract: no 64-bit division helper on i386. */
static unsigned long long gex_udiv(unsigned long long u, unsigned d) {
    unsigned long long q = 0, r = 0;
    for (int i = 63; i >= 0; i--) {
        r = (r << 1) | ((u >> i) & 1);
        if (r >= d) r -= d, q |= 1ull << i;
    }
    return q;
}

static unsigned gex_steam_time() {
    unsigned long long ft;
    GetSystemTimeAsFileTime(&ft);
    /* 100 ns ticks since 1601 -> seconds since 1970. */
    return (unsigned)gex_udiv(ft - 116444736000000000ull, 10000000u);
}

#include "steam_versions.inc"

typedef struct gex_agg_CSteamID gex_ret_CSteamID;

/* An individual account in the public universe: 0x0110000100000000 | id. */
static gex_ret_CSteamID gex_steam_id() {
    gex_ret_CSteamID r;
    unsigned long long id = 0x0110000100000000ull | 1;
    memcpy(r.b, &id, sizeof id);
    return r;
}

/* An auth ticket: "gexsteam", then the user's SteamID. */
static int gex_steam_ticket(void *buf, int max) {
    unsigned char t[16];
    memcpy(t, "gexsteam", 8);
    gex_ret_CSteamID id = gex_steam_id();
    memcpy(t + 8, id.b, 8);
    if (!buf || max < (int)sizeof t) return 0;
    memcpy(buf, t, sizeof t);
    return (int)sizeof t;
}

#define GEX_INSTANCE(cls, fam, n) constinit static cls inst_##cls;
GEX_STEAM_VERSIONS(GEX_INSTANCE)

struct gex_version { const char *family; int n; void *obj; };
static gex_version gex_versions[] = {
#define GEX_ENTRY(cls, fam, n) {fam, n, &inst_##cls},
    GEX_STEAM_VERSIONS(GEX_ENTRY)
};
static const int gex_nversions = sizeof gex_versions / sizeof gex_versions[0];

/* SteamUser018, STEAMUSERSTATS_INTERFACE_VERSION011 -> family, number. */
static bool gex_key(const char *v, char *fam, int famsize, int *n) {
    int f = 0, num = -1;
    const char *drop = "INTERFACEVERSION";
    char up[96];
    int u = 0;
    for (; *v && u < (int)sizeof up - 1; v++) {
        char c = *v;
        if (c >= 'a' && c <= 'z') c = (char)(c - 32);
        if ((c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')) up[u++] = c;
    }
    up[u] = 0;
    /* Remove "INTERFACEVERSION" where it occurs. */
    char out[96];
    int o = 0;
    for (int i = 0; up[i];) {
        int k = 0;
        while (drop[k] && up[i + k] == drop[k]) k++;
        if (!drop[k]) {
            i += k;
            continue;
        }
        out[o++] = up[i++];
    }
    out[o] = 0;
    while (out[f] >= 'A' && out[f] <= 'Z') f++;
    if (f == 0 || f >= famsize || !out[f]) return false;
    num = 0;
    for (int i = f; out[i]; i++) {
        if (out[i] < '0' || out[i] > '9') return false;
        num = num * 10 + (out[i] - '0');
    }
    for (int i = 0; i < f; i++) fam[i] = out[i];
    fam[f] = 0;
    *n = num;
    return true;
}

/* An interface nobody here implements: every slot returns zero. */
static intptr_t gex_steam_zero() { return 0; }
static void *gex_steam_unknown_vtable[512];
static void *gex_steam_unknown = gex_steam_unknown_vtable;

static void *gex_find(const char *version) {
    char fam[64];
    int n;
    if (!version || !gex_key(version, fam, sizeof fam, &n)) {
        gex_log("no interface ", version ? version : "(null)");
        if (!gex_steam_unknown_vtable[0])
            for (int i = 0; i < 512; i++) gex_steam_unknown_vtable[i] = (void *)gex_steam_zero;
        return &gex_steam_unknown;
    }
    void *best = 0;
    int bestn = -1;
    for (int i = 0; i < gex_nversions; i++) {
        if (!gex_streq(gex_versions[i].family, fam)) continue;
        if (gex_versions[i].n == n) return gex_versions[i].obj;
        if (gex_versions[i].n > bestn) best = gex_versions[i].obj, bestn = gex_versions[i].n;
    }
    gex_log(best ? "no exact version, using the newest for " : "no interface ", version);
    if (best) return best;
    if (!gex_steam_unknown_vtable[0])
        for (int i = 0; i < 512; i++) gex_steam_unknown_vtable[i] = (void *)gex_steam_zero;
    return &gex_steam_unknown;
}

/* The versions an old game's accessors use: those in the original DLL. */
static struct { char family[32]; int n; } gex_pref[48];
static int gex_npref = -1;

static void gex_scan_original() {
    if (gex_npref >= 0) return;
    gex_npref = 0;
    char path[1040];
    if (!GetEnvironmentVariableA("GEX_STEAM_ORIG", path, 1024)) {
        void *self = 0;
        if (!GetModuleHandleExA(6 /* FROM_ADDRESS | UNCHANGED_REFCOUNT */, (const char *)&gex_scan_original, &self))
            return;
        unsigned long n = GetModuleFileNameA(self, path, 1024);
        if (!n || n >= 1024) return;
        path[n] = '.', path[n + 1] = 'o', path[n + 2] = 'r', path[n + 3] = 'i', path[n + 4] = 'g', path[n + 5] = 0;
    }
    unsigned long len;
    unsigned char *d = gex_read(path, &len);
    if (!d) {
        gex_log("no original DLL at ", path);
        return;
    }
    for (unsigned long i = 0; i < len;) {
        unsigned long j = i;
        while (j < len && ((d[j] >= 'A' && d[j] <= 'Z') || (d[j] >= 'a' && d[j] <= 'z') || (d[j] >= '0' && d[j] <= '9') ||
                           d[j] == '_'))
            j++;
        if (j < len && d[j] == 0 && j - i >= 8 && j - i < 64 && (i == 0 || d[i - 1] == 0) &&
            (d[i] == 'S' || d[i] == 's')) {
            char fam[64];
            int n;
            if (gex_key((const char *)d + i, fam, sizeof fam, &n)) {
                bool known = false;
                for (int k = 0; k < gex_nversions && !known; k++)
                    known = gex_streq(gex_versions[k].family, fam) && gex_versions[k].n == n;
                if (known) {
                    int k = 0;
                    while (k < gex_npref && !gex_streq(gex_pref[k].family, fam)) k++;
                    if (k == gex_npref && k < 48) {
                        gex_steam_copy(gex_pref[k].family, 32, fam);
                        gex_pref[k].n = n;
                        gex_npref++;
                    } else if (k < gex_npref && n > gex_pref[k].n) {
                        gex_pref[k].n = n;
                    }
                }
            }
        }
        i = j + 1;
    }
    VirtualFree(d, 0, 0x8000);
}

/* An accessor's object: the original DLL's version of `family`, else the newest. */
static void *gex_accessor(const char *family) {
    gex_scan_original();
    int n = 0;
    for (int k = 0; k < gex_npref; k++)
        if (gex_streq(gex_pref[k].family, family)) n = gex_pref[k].n;
    char v[64];
    int i = 0;
    for (; family[i]; i++) v[i] = family[i];
    v[i++] = (char)('0' + n / 100 % 10);
    v[i++] = (char)('0' + n / 10 % 10);
    v[i++] = (char)('0' + n % 10);
    v[i] = 0;
    return gex_find(v);
}

static unsigned gex_counter = 1;

S_API bool SteamAPI_Init() { gex_log("SteamAPI_Init"); return true; }
S_API bool SteamAPI_InitSafe() { gex_log("SteamAPI_InitSafe"); return true; }
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
S_API bool SteamAPI_ManualDispatch_GetAPICallResult(int, unsigned long long, void *, int, int, bool *f) {
    if (f) *f = true;
    return false;
}

S_API void *SteamInternal_FindOrCreateUserInterface(int, const char *v) { gex_log("user interface ", v); return gex_find(v); }
S_API void *SteamInternal_FindOrCreateGameServerInterface(int, const char *v) { return gex_find(v); }
S_API void *SteamInternal_CreateInterface(const char *v) { gex_log("interface ", v); return gex_find(v); }
S_API void *CreateInterface(const char *v, int *rc) { if (rc) *rc = 0; return gex_find(v); }

/* `{ void (*init)(void *ctx); uintptr_t counter; void *ctx; }`: run the
 * accessor's initialiser once per counter, then hand back its slot. */
S_API void *SteamInternal_ContextInit(void *data) {
    struct ctx_init { void (*init)(void *); uintptr_t counter; void *ctx; } *d = (struct ctx_init *)data;
    if (d->counter != gex_counter) {
        d->init(&d->ctx);
        d->counter = gex_counter;
    }
    return &d->ctx;
}

/* SDKs before 1.37: accessors exported by steam_api.dll. */
#define GEX_ACCESSOR(name, family) S_API void *name() { return gex_accessor(family); }
GEX_ACCESSOR(SteamClient, "STEAMCLIENT")
GEX_ACCESSOR(SteamUser, "STEAMUSER")
GEX_ACCESSOR(SteamFriends, "STEAMFRIENDS")
GEX_ACCESSOR(SteamUtils, "STEAMUTILS")
GEX_ACCESSOR(SteamMatchmaking, "STEAMMATCHMAKING")
GEX_ACCESSOR(SteamMatchmakingServers, "STEAMMATCHMAKINGSERVERS")
GEX_ACCESSOR(SteamUserStats, "STEAMUSERSTATS")
GEX_ACCESSOR(SteamApps, "STEAMAPPS")
GEX_ACCESSOR(SteamNetworking, "STEAMNETWORKING")
GEX_ACCESSOR(SteamRemoteStorage, "STEAMREMOTESTORAGE")
GEX_ACCESSOR(SteamScreenshots, "STEAMSCREENSHOTS")
GEX_ACCESSOR(SteamHTTP, "STEAMHTTP")
GEX_ACCESSOR(SteamController, "STEAMCONTROLLER")
GEX_ACCESSOR(SteamUGC, "STEAMUGC")
GEX_ACCESSOR(SteamAppList, "STEAMAPPLIST")
GEX_ACCESSOR(SteamMusic, "STEAMMUSIC")
GEX_ACCESSOR(SteamMusicRemote, "STEAMMUSICREMOTE")
GEX_ACCESSOR(SteamHTMLSurface, "STEAMHTMLSURFACE")
GEX_ACCESSOR(SteamInventory, "STEAMINVENTORY")
GEX_ACCESSOR(SteamVideo, "STEAMVIDEO")
GEX_ACCESSOR(SteamParentalSettings, "STEAMPARENTALSETTINGS")
GEX_ACCESSOR(SteamInput, "STEAMINPUT")
GEX_ACCESSOR(SteamGameServer, "STEAMGAMESERVER")
GEX_ACCESSOR(SteamGameServerUtils, "STEAMUTILS")
GEX_ACCESSOR(SteamGameServerNetworking, "STEAMNETWORKING")
GEX_ACCESSOR(SteamGameServerStats, "STEAMGAMESERVERSTATS")
GEX_ACCESSOR(SteamGameServerHTTP, "STEAMHTTP")
GEX_ACCESSOR(SteamGameServerUGC, "STEAMUGC")

S_API bool SteamGameServer_Init(unsigned, unsigned short, unsigned short, unsigned short, int, const char *) { return true; }
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
