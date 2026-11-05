/* Native Wine DLLs freed and loaded again, at other bases (64-bit Wine's
 * native PE core, NATIVE=).
 *
 * Explorer's Start menu loads and frees each Control Panel applet in turn:
 * appwiz, bthprops, desk, inetcpl, irprops. Their dependencies (rpcrt4,
 * setupapi, cfgmgr32, bluetoothapis) go with each FreeLibrary and come back
 * with the next applet, often at another base, or at a base another DLL had
 * meanwhile. The new mapping is the loaded DLL's: what gex writes at a DLL's
 * mapping (winecrt0's base of it, and what GetModuleHandleEx's FROM_ADDRESS
 * finds) must follow it. Kept at the first mapping's, rpcrt4's entry under
 * desk.cpl's load never returned, holding the loader lock.
 *
 * Here: the applets in the Start menu's order, each loaded and freed, then
 * rpcrt4 (loaded again by the last) must be found from its own function's
 * address at its new base, and called (looked up, so the program itself
 * does not hold rpcrt4); three rounds.
 * gex-nt-arch: x64
 */
#include <stdio.h>
#include <windows.h>

static int failures;

static HMODULE from(const void *p)
{
    HMODULE m = 0;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, p, &m)) return 0;
    return m;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);  /* a hang still shows how far it got */
    static const char *const applets[] = { "appwiz.cpl", "bthprops.cpl", "desk.cpl", "inetcpl.cpl", "irprops.cpl" };
    for (int round = 0; round < 3; round++) {
        int loaded = 0;
        for (int i = 0; i < 5; i++) {
            HMODULE h = LoadLibraryA(applets[i]);
            if (h) {
                loaded++;
                FreeLibrary(h);
            } else {
                printf("FAIL round %d: %s does not load (error %lu)\n", round, applets[i], GetLastError());
                failures++;
            }
        }
        if (loaded == 5) printf("ok round %d: the five applets loaded and freed\n", round);
        /* rpcrt4 again, and a function of it found back at the base it has now */
        HMODULE r = LoadLibraryA("rpcrt4.dll");
        RPC_STATUS (WINAPI *create)(UUID *) = r ? (void *)GetProcAddress(r, "UuidCreate") : NULL;
        void *f = (void *)create;
        UUID u;
        HMODULE got = f ? from(f) : 0;
        if (f && got == r && create(&u) == RPC_S_OK) {
            printf("ok round %d: rpcrt4 works and UuidCreate is found in it\n", round);
        } else {
            printf("FAIL round %d: rpcrt4 %p, UuidCreate %p found in %p\n", round, (void *)r, f, (void *)got);
            failures++;
        }
        if (r) FreeLibrary(r);
    }
    printf("%d failures\n", failures);
    return failures != 0;
}
