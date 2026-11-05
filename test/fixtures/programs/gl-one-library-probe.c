/* The guest GL library's three names are one library (guest/gl/build.sh): a
 * program that opens libGL.so.1, libEGL.so.1 and libGLESv2.so.2 gets the same
 * object each time, so one GL state. Prints SUMMARY failed=0 when so.
 *
 *   gexc run --env LD_LIBRARY_PATH=GUEST_GL probe     (test/gl/one-library) */
#include <dlfcn.h>
#include <stdio.h>

int main(void) {
    static const char *const names[] = {"libGL.so.1", "libEGL.so.1", "libGLESv2.so.2"};
    void *handle[3];
    void *sym[3][2];
    int failed = 0;
    for (int i = 0; i < 3; i++) {
        handle[i] = dlopen(names[i], RTLD_NOW | RTLD_LOCAL);
        if (!handle[i]) {
            printf("CASE open %s FAIL (%s)\n", names[i], dlerror());
            return 1;
        }
        sym[i][0] = dlsym(handle[i], "glGetFloatv");
        sym[i][1] = dlsym(handle[i], "eglGetDisplay");
    }
    for (int i = 1; i < 3; i++) {
        int ok = handle[i] == handle[0] && sym[i][0] == sym[0][0] && sym[i][1] == sym[0][1] && sym[0][0] && sym[0][1];
        failed += !ok;
        printf("CASE %s is %s %s\n", names[i], names[0], ok ? "PASS" : "FAIL");
    }
    printf("SUMMARY failed=%d\n", failed);
    return failed != 0;
}
