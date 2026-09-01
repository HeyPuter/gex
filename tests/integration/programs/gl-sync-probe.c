#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>

typedef void *(*fence_sync_fn)(unsigned, unsigned);
typedef void (*delete_sync_fn)(void *);
typedef unsigned char (*is_sync_fn)(void *);
typedef unsigned (*client_wait_sync_fn)(void *, unsigned, uint64_t);
typedef void (*get_sync_iv_fn)(void *, unsigned, int, int *, int *);

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) {
        fprintf(stderr, "%s\n", dlerror());
        return 2;
    }
#define LOAD(type, name) type name = (type)dlsym(library, #name)
    LOAD(fence_sync_fn, glFenceSync);
    LOAD(delete_sync_fn, glDeleteSync);
    LOAD(is_sync_fn, glIsSync);
    LOAD(client_wait_sync_fn, glClientWaitSync);
    LOAD(get_sync_iv_fn, glGetSynciv);
    if (!glFenceSync || !glDeleteSync || !glIsSync ||
        !glClientWaitSync || !glGetSynciv) return 3;

    void *sync = glFenceSync(0x9117u, 0);
    if (!sync || !glIsSync(sync)) return 4;
    if (glClientWaitSync(sync, 0, 0) != 0x911Au) return 5;

    const unsigned pnames[] = {0x9112u, 0x9113u, 0x9114u, 0x9115u};
    const int expected[] = {0x9116, 0x9117, 0x9119, 0};
    for (unsigned i = 0; i < 4; i++) {
        int length = 0, value = -1;
        glGetSynciv(sync, pnames[i], 1, &length, &value);
        if (length != 1 || value != expected[i]) return 6 + (int)i;
    }

    glDeleteSync(sync);
    if (glIsSync(sync)) return 10;
    if (glClientWaitSync(sync, 0, 0) != 0x911Du) return 11;
    dlclose(library);
    return 0;
}
