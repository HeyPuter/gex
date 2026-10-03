// Indirect calls through a vtable-like table in a hot loop, as C++ virtual
// getters: the shape profile-guided devirtualisation (an indirect-call
// profile, `<cache>/icall/`) and leaf copies speed up. Most calls at each
// site go to one target; one site switches target now and then, so the
// guarded direct call's fallback through the function table runs too; one
// target is not a leaf (it calls another function).
#include <stdio.h>

struct obj;
struct vt {
    int (*get)(const struct obj *);
    int (*step)(struct obj *, int);
};
struct obj {
    const struct vt *vt;
    int a, b;
};

__attribute__((noinline)) static int get_a(const struct obj *o) { return o->a; }
__attribute__((noinline)) static int get_b(const struct obj *o) { return o->b * 3 + 1; }
__attribute__((noinline)) static int mix(int x, int y) { return (x * 31) ^ (y >> 2); }
__attribute__((noinline)) static int step_mix(struct obj *o, int k) {
    o->a = mix(o->a, k);
    return o->a & 0xff;
}
__attribute__((noinline)) static int step_add(struct obj *o, int k) {
    o->b += k;
    return o->b & 0xff;
}

static const struct vt vt_a = {get_a, step_mix};
static const struct vt vt_b = {get_b, step_add};

// Opaque to the compiler, so the calls stay indirect.
struct obj objs[2];
const struct vt *volatile vts[2] = {&vt_a, &vt_b};

int main(void) {
    objs[0] = (struct obj){vts[0], 1, 2};
    objs[1] = (struct obj){vts[1], 3, 4};
    struct obj *volatile pick = &objs[0];
    unsigned sum = 0;
    for (int i = 0; i < 3000000; i++) {
        struct obj *o = pick;
        sum += o->vt->get(o);
        sum += o->vt->step(o, i);
        // One call in 64 goes to the other object's methods.
        struct obj *p = &objs[(i & 63) == 63];
        sum = sum * 7 + p->vt->get(p);
    }
    printf("icall: %u %d %d %d %d\n", sum, objs[0].a, objs[0].b, objs[1].a, objs[1].b);
    return 0;
}
