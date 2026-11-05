/* Undefined weak symbols, as ELF has them: their address is null, so code
 * that checks before calling (glib's LeakSanitizer hooks) runs. With
 * GEXCC_SHARED=1 each is a GOT.func / GOT.mem import the loader binds to 0. */
#include <stdio.h>

extern void __lsan_enable(void) __attribute__((weak));
extern void __lsan_disable(void) __attribute__((weak));
extern int no_such_data __attribute__((weak));

static void (*volatile hook)(void) = __lsan_disable;

int main(void) {
    if (__lsan_enable)
        __lsan_enable();
    printf("function: %s\n", __lsan_enable ? "defined" : "null");
    printf("through a pointer: %s\n", hook ? "defined" : "null");
    printf("data: %s\n", &no_such_data ? "defined" : "null");
    return 0;
}
