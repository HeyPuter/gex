// M-embed: gex-embed in an emscripten program built with android-wasm's flags, running test/guest.s's AArch64
// functions from the host. `node out/embedtest.js [high]`; "high" puts the guest code above 2 GiB first.
#include <malloc.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "gex_embed.h"
#include "guest.h"

static uint64_t code;
static int failures;

#define CHECK(what, got, want)                                                                         \
    do {                                                                                               \
        uint64_t g_ = (got), w_ = (want);                                                              \
        if (g_ != w_) {                                                                                \
            printf("FAIL %s: got %llu want %llu\n", what, (unsigned long long)g_, (unsigned long long)w_); \
            __atomic_fetch_add(&failures, 1, __ATOMIC_RELAXED);                                        \
        }                                                                                              \
    } while (0)

static uint64_t at(uint32_t off) { return code + off; }

static uint64_t call2(uint64_t pc, uint64_t a, uint64_t b) {
    gex_call c;
    memset(&c, 0, sizeof c);
    c.x[0] = a;
    c.x[1] = b;
    gex_embed_call(pc, &c);
    return c.rx[0];
}

// A host function the guest calls: x0 * 100 + x1 + user.
static void host_add(uint32_t ctx, uint32_t user) {
    gex_ctx_set_x(ctx, 0, gex_ctx_x(ctx, 0) * 100 + gex_ctx_x(ctx, 1) + user);
}

// One that calls back into the guest: x0 = mul_loop(x0, x1), a call nested in a call.
static void host_nested(uint32_t ctx, uint32_t user) {
    (void)user;
    gex_ctx_set_x(ctx, 0, call2(at(OFF_mul_loop), gex_ctx_x(ctx, 0), gex_ctx_x(ctx, 1)));
}

static uint64_t thunk_add, thunk_nested;
static uint64_t excl_counter, lse_counter;

enum { THREADS = 4, INCS = 20000 };

static void* worker(void* arg) {
    uint64_t t = (uint64_t)(uintptr_t)arg;
    for (int i = 0; i < 20; i++) CHECK("mul_loop (thread)", call2(at(OFF_mul_loop), t + 3, 1000 + i), (t + 3) * (1000 + i));
    CHECK("call_host (thread)", ({
              gex_call c;
              memset(&c, 0, sizeof c);
              c.x[0] = t;
              c.x[1] = 7;
              c.x[2] = thunk_add;
              gex_embed_call(at(OFF_call_host), &c);
              c.rx[0];
          }),
          t * 100 + 7 + 5 + 1);
    call2(at(OFF_inc_excl), (uint64_t)(uintptr_t)&excl_counter, INCS);
    call2(at(OFF_inc_lse), (uint64_t)(uintptr_t)&lse_counter, INCS);
    return 0;
}

int main(int argc, char** argv) {
    gex_embed_config cfg = {.floor = 0x400, .stack_size = 1 << 20};
    gex_embed_init(&cfg);

    if (argc > 1 && !strcmp(argv[1], "high")) {
        // Fill the memory below 2 GiB, so the code lands above it.
        void* fill = malloc(2200u << 20);
        printf("filler at %p\n", fill);
    }
    void* blob = memalign(4096, (sizeof guest_code + 4095) & ~4095u);
    memcpy(blob, guest_code, sizeof guest_code);
    code = (uint64_t)(uintptr_t)blob;
    gex_embed_map_exec(code, sizeof guest_code);
    printf("guest code at %#llx\n", (unsigned long long)code);

    thunk_add = gex_embed_thunk(host_add, 5);
    thunk_nested = gex_embed_thunk(host_nested, 0);

    CHECK("mul_loop", call2(at(OFF_mul_loop), 6, 7), 42);
    CHECK("mul_loop again", call2(at(OFF_mul_loop), 123, 1000), 123000);

    gex_call c;
    memset(&c, 0, sizeof c);
    c.x[0] = 4;
    c.x[1] = 2;
    c.x[2] = thunk_add;
    gex_embed_call(at(OFF_call_host), &c);
    CHECK("call_host", c.rx[0], 4 * 100 + 2 + 5 + 1);

    memset(&c, 0, sizeof c);
    c.x[0] = 9;
    c.x[1] = 11;
    c.x[2] = thunk_nested;
    gex_embed_call(at(OFF_call_host), &c);
    CHECK("nested guest-host-guest", c.rx[0], 9 * 11 + 1);

    const char msg[] = "hello from the guest\n";
    CHECK("say", call2(at(OFF_say), (uint64_t)(uintptr_t)msg, sizeof msg - 1), sizeof msg - 1);

    int64_t ts[2] = {-1, -1};
    CHECK("clock", call2(at(OFF_clock), (uint64_t)(uintptr_t)ts, 0), 0);
    CHECK("clock nsec in range", ts[1] >= 0 && ts[1] < 1000000000, 1);

    double a = 1.25, b = 2.5, r;
    memset(&c, 0, sizeof c);
    memcpy(&c.v[0][0], &a, 8);
    memcpy(&c.v[1][0], &b, 8);
    gex_embed_call(at(OFF_fadd2), &c);
    memcpy(&r, &c.rv[0][0], 8);
    CHECK("fadd2", r == 3.75, 1);

    // Scalar by-element multiplies write only their element: the rest of v0 is zeroed.
    float fs[4] = {1.0f, 2.0f, 3.5f, 4.0f}, f0 = 1.5f, fr;
    memset(&c, 0, sizeof c);
    memcpy(&c.v[0][0], &f0, 4);
    c.v[0][1] = 0x7fc000007fc00000ull;
    memcpy(&c.v[1][0], fs, 16);
    gex_embed_call(at(OFF_fmul_lane), &c);
    memcpy(&fr, &c.rv[0][0], 4);
    CHECK("fmul s0, s0, v1.s[2]", fr == 5.25f && (c.rv[0][0] >> 32) == 0 && c.rv[0][1] == 0, 1);
    double acc = 1.0, mul = 3.0, lanes[2] = {-1.0, 4.0};
    memset(&c, 0, sizeof c);
    memcpy(&c.v[0][0], &acc, 8);
    memcpy(&c.v[1][0], &mul, 8);
    memcpy(&c.v[2][0], lanes, 16);
    gex_embed_call(at(OFF_fmla_lane), &c);
    memcpy(&r, &c.rv[0][0], 8);
    CHECK("fmla d0, d1, v2.d[1]", r == 13.0 && c.rv[0][1] == 0, 1);

    CHECK("popcount", call2(at(OFF_popcount), 0xf00f00000000ff01ull, 0), 17);
    int16_t hs[8] = {-32768, -32768, -1, 7, 32767, -100, 3, -32768};
    uint32_t ws[4] = {0xffffffffu, 0xffffffffu, 5, 0x80000000u};
    memset(&c, 0, sizeof c);
    memcpy(&c.v[0][0], hs, 16);
    memcpy(&c.v[1][0], ws, 16);
    gex_embed_call(at(OFF_addlv), &c);
    CHECK("saddlv s0, v0.8h", c.rv[0][0], (uint32_t)(-32768 * 3 - 1 + 7 + 32767 - 100 + 3));
    CHECK("saddlv zeroes the rest", c.rv[0][1], 0);
    CHECK("uaddlv d1, v1.4s", c.rv[1][0], 0xffffffffull * 2 + 5 + 0x80000000ull);

    uint64_t stack[2] = {1000, 20000};
    memset(&c, 0, sizeof c);
    c.x[0] = 7;
    c.stack = stack;
    c.nstack = 2;
    gex_embed_call(at(OFF_stack_args), &c);
    CHECK("stack_args", c.rx[0], 21007);

    pthread_t th[THREADS];
    for (uintptr_t i = 0; i < THREADS; i++) pthread_create(&th[i], 0, worker, (void*)i);
    for (int i = 0; i < THREADS; i++) pthread_join(th[i], 0);
    CHECK("ldxr/stxr counter", excl_counter, (uint64_t)THREADS * INCS);
    CHECK("ldaddal counter", lse_counter, (uint64_t)THREADS * INCS);

    printf("embedtest: %s\n", failures ? "FAIL" : "PASS");
    return failures ? 1 : 0;
}
