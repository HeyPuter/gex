/* What eglGetError says after a failed call: the failure's code, then
   EGL_SUCCESS. Run where /dev/gex-gl does not exist, so eglInitialize
   fails. */
#include <stdio.h>

unsigned eglInitialize(void *dpy, int *major, int *minor);
unsigned eglBindAPI(unsigned api);
unsigned eglMakeCurrent(void *dpy, void *draw, void *read, void *ctx);
int eglGetError(void);

int main(void) {
    printf("before 0x%x\n", eglGetError());
    int major = 0, minor = 0;
    unsigned ok = eglInitialize((void *)1, &major, &minor);
    printf("initialize %u 0x%x 0x%x\n", ok, eglGetError(), eglGetError());
    ok = eglBindAPI(0x1234u);
    printf("bind %u 0x%x\n", ok, eglGetError());
    ok = eglMakeCurrent((void *)1, NULL, NULL, NULL);
    printf("make-current %u 0x%x\n", ok, eglGetError());
    return 0;
}
