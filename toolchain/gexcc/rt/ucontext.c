// libucontext for musl's <ucontext.h> declarations (musl implements none of
// them): a wasm module cannot switch to another stack, so getcontext and
// swapcontext fail with ENOSYS and a caller that checks them (systemd's
// fibers) reports it rather than running on a stack it never reaches.
#include <errno.h>
#include <ucontext.h>

int getcontext(ucontext_t *uc) {
    (void)uc;
    errno = ENOSYS;
    return -1;
}

int setcontext(const ucontext_t *uc) {
    (void)uc;
    errno = ENOSYS;
    return -1;
}

void makecontext(ucontext_t *uc, void (*fn)(void), int argc, ...) {
    (void)uc;
    (void)fn;
    (void)argc;
}

int swapcontext(ucontext_t *old, const ucontext_t *uc) {
    (void)old;
    (void)uc;
    errno = ENOSYS;
    return -1;
}
