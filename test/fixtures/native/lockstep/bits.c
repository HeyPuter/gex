/* test/native/lockstep: C bitfields read-modify-write, hand-written popcount, bit reversal and
 * shifts/rotates on packed records. */
#include "kernel.h"
#include <stdint.h>
typedef struct { uint32_t kind : 3, flags : 5, count : 10, owner : 14; } Rec;
enum { N = 4096 };
static Rec recs[N];
static uint32_t pop(uint32_t v) { v = v - ((v >> 1) & 0x55555555u); v = (v & 0x33333333u) + ((v >> 2) & 0x33333333u); return (((v + (v >> 4)) & 0x0f0f0f0fu) * 0x01010101u) >> 24; }
static uint32_t rev(uint32_t v) {
    v = ((v >> 1) & 0x55555555u) | ((v & 0x55555555u) << 1); v = ((v >> 2) & 0x33333333u) | ((v & 0x33333333u) << 2);
    v = ((v >> 4) & 0x0f0f0f0fu) | ((v & 0x0f0f0f0fu) << 4); v = ((v >> 8) & 0x00ff00ffu) | ((v & 0x00ff00ffu) << 8);
    return (v >> 16) | (v << 16);
}
static uint32_t rotl(uint32_t v, unsigned k) { k &= 31; return k ? (v << k) | (v >> (32 - k)) : v; }
unsigned kernel(int scale) {
    for (int i = 0; i < N; ++i) recs[i] = (Rec){ (uint32_t)i & 7, (uint32_t)(i * 3) & 31, (uint32_t)i & 1023, (uint32_t)(i * 77) & 16383 };
    uint32_t acc = 0;
    for (int r = 0; r < 120 * scale; ++r) {
        for (int i = 0; i < N; ++i) {
            Rec *p = &recs[i];
            p->count += p->kind + 1;
            if (p->flags & 1) p->owner ^= r;
            p->flags = (p->flags >> 1) | ((p->count & 1) << 4);
            if (p->kind == 5) p->kind = (uint32_t)r & 7; else p->kind++;
            uint32_t w = rotl((uint32_t)p->owner * 2654435761u, p->kind * 5);
            acc += pop(w) + (rev(w) >> 27);
        }
    }
    for (int i = 0; i < N; ++i) acc = acc * 31 + recs[i].count + recs[i].owner + recs[i].flags + recs[i].kind;
    return acc;
}
