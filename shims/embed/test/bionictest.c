// Spike C: gex-embed booting bionic's linker64 in an emscripten program (android-wasm's flags), then calling libm and
// libc as an Android app's library would. The guest image (android-wasm native/guest/build.sh: out/guest/system) is
// embedded at /system. `node out/bionictest.js [trace]`.
#include <dlfcn.h>
#include <malloc.h>
#include <math.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "gex_embed.h"

int gex_embed_boot(const char* linker, const char* exe);
uint64_t gex_embed_dlopen(const char* path, int flags);
uint64_t gex_embed_dlsym(uint64_t handle, const char* name);
uint32_t gex_embed_dlerror(void);
void gex_embed_set_trace(int on);

static int failures;

#define CHECK(what, cond)                                       \
    do {                                                        \
        if (!(cond)) {                                          \
            printf("FAIL %s\n", what);                          \
            __atomic_fetch_add(&failures, 1, __ATOMIC_RELAXED); \
        } else {                                                \
            printf("ok   %s\n", what);                          \
        }                                                       \
    } while (0)

static uint64_t libc_, libm_;

static uint64_t sym(uint64_t h, const char* name) {
    uint64_t p = gex_embed_dlsym(h, name);
    if (!p) printf("no %s: %s\n", name, (const char*)(uintptr_t)gex_embed_dlerror());
    return p;
}

static uint64_t callx(uint64_t pc, uint64_t a, uint64_t b, uint64_t c, uint64_t d) {
    gex_call k;
    memset(&k, 0, sizeof k);
    k.x[0] = a, k.x[1] = b, k.x[2] = c, k.x[3] = d;
    gex_embed_call(pc, &k);
    return k.rx[0];
}

static void* second(void* arg) {
    (void)arg;
    // The first guest call on this thread adopts it into bionic.
    const char s[] = "adopted thread";
    CHECK("strlen on a second thread", callx(sym(libc_, "strlen"), (uintptr_t)s, 0, 0, 0) == strlen(s));
    char buf[64];
    callx(sym(libc_, "snprintf"), (uintptr_t)buf, sizeof buf, (uintptr_t)"tid-ish %d", 7);
    CHECK("snprintf on a second thread", !strcmp(buf, "tid-ish 7"));
    return 0;
}

int main(int argc, char** argv) {
    gex_embed_config cfg = {.floor = 0x400, .stack_size = 1 << 20};
    gex_embed_init(&cfg);
    if (argc > 1 && !strcmp(argv[1], "trace")) gex_embed_set_trace(1);
    if (gex_embed_boot("/system/bin/linker64", "/system/bin/aw_guest_init")) return 1;
    printf("ok   booted linker64\n");

    libm_ = gex_embed_dlopen("libm.so", RTLD_NOW);
    CHECK("dlopen libm.so", libm_ != 0);
    uint64_t sin_ = sym(libm_, "sin");
    gex_call k;
    memset(&k, 0, sizeof k);
    double x = 1.0, r;
    memcpy(&k.v[0][0], &x, 8);
    gex_embed_call(sin_, &k);
    memcpy(&r, &k.rv[0][0], 8);
    printf("     sin(1.0) = %.17g (host %.17g)\n", r, sin(1.0));
    CHECK("libm sin", r == sin(1.0));

    libc_ = gex_embed_dlopen("libc.so", RTLD_NOW);
    CHECK("dlopen libc.so", libc_ != 0);

    char buf[128];
    memset(&k, 0, sizeof k);
    k.x[0] = (uintptr_t)buf, k.x[1] = sizeof buf, k.x[2] = (uintptr_t)"%d %s %.3f %x";
    k.x[3] = 42, k.x[4] = (uintptr_t)"hi", k.x[5] = 0xbeef;
    double pi = 3.14159265;
    memcpy(&k.v[0][0], &pi, 8);
    gex_embed_call(sym(libc_, "snprintf"), &k);
    printf("     snprintf: \"%s\"\n", buf);
    CHECK("snprintf varargs", !strcmp(buf, "42 hi 3.142 beef"));

    uint64_t p = callx(sym(libc_, "malloc"), 1000, 0, 0, 0);
    CHECK("guest malloc is the host's heap", p && malloc_usable_size((void*)(uintptr_t)p) >= 1000);
    callx(sym(libc_, "free"), p, 0, 0, 0);

    CHECK("getauxval(AT_PAGESZ)", callx(sym(libc_, "getauxval"), 6, 0, 0, 0) == 4096);

    uint64_t f = callx(sym(libc_, "fopen"), (uintptr_t)"/tmp/guest.txt", (uintptr_t)"w", 0, 0);
    CHECK("fopen", f != 0);
    callx(sym(libc_, "fprintf"), f, (uintptr_t)"written by the guest %d\n", 3, 0);
    callx(sym(libc_, "fclose"), f, 0, 0, 0);
    FILE* h = fopen("/tmp/guest.txt", "r");
    char line[64] = {0};
    if (h) fgets(line, sizeof line, h), fclose(h);
    CHECK("the host reads what guest stdio wrote", !strcmp(line, "written by the guest 3\n"));

    pthread_t t;
    pthread_create(&t, 0, second, 0);
    pthread_join(t, 0);

    printf("bionictest: %s\n", failures ? "FAIL" : "PASS");
    return failures ? 1 : 0;
}
