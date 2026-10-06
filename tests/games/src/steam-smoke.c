/* gex-steam (guest/steam) as a game uses it: SteamAPI_Init, an interface by
 * version, a call through its vtable, an older version of a known
 * interface. Built per arch by tests/games/steam-smoke.sh and run on the
 * host (the library is plain C++ with no Steam behind it). */
#include <dlfcn.h>
#include <stdio.h>

typedef int (*init_t)(void);
typedef void *(*find_t)(int, const char *);
typedef int (*call0_t)(void *);

int main(int argc, char **argv) {
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { printf("FAIL dlopen %s\n", dlerror()); return 1; }
    int failures = 0;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); failures += !ok_; } while (0)
    init_t init = (init_t)dlsym(h, "SteamAPI_Init");
    find_t find = (find_t)dlsym(h, "SteamInternal_FindOrCreateUserInterface");
    CHECK("SteamAPI_Init", init && init());
    void **user = find ? (void **)find(1, "SteamUser023") : 0;
    /* ISteamUser: GetHSteamUser is slot 0, BLoggedOn slot 1 in every version. */
    CHECK("SteamUser023 BLoggedOn", user && ((call0_t)(*(void ***)user)[1])(user));
    void **old = find ? (void **)find(1, "SteamUser017") : 0;
    CHECK("SteamUser017 (an older version) BLoggedOn", old && ((call0_t)(*(void ***)old)[1])(old));
    (void)argc;
    printf("%d failures\n", failures);
    return failures;
}
