#include <dlfcn.h>
#include <stdio.h>

typedef void *(*get_proc_fn)(const char *);

int main(void) {
    void *egl = dlopen("libEGL.so", RTLD_LAZY | RTLD_LOCAL);
    if (!egl) {
        fprintf(stderr, "gl-proc-probe: dlopen failed: %s\n", dlerror());
        return 1;
    }

    get_proc_fn get_proc = (get_proc_fn)dlsym(egl, "eglGetProcAddress");
    if (!get_proc) {
        fprintf(stderr, "gl-proc-probe: eglGetProcAddress missing\n");
        return 1;
    }

    void *known = get_proc("glBeginTransformFeedback");
    void *missing = get_proc("glGexDefinitelyMissingProcedure");
    printf("gl-proc-probe: known=%s missing=%s\n",
           known ? "yes" : "no", missing ? "BAD" : "null");
    return known && !missing ? 0 : 1;
}
