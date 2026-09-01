#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

extern void glTexParameteriv(unsigned, unsigned, const int *);
extern void glTexParameterfv(unsigned, unsigned, const float *);
extern void glGetIntegerv(unsigned, int *);
extern unsigned glGetError(void);

enum {
    GL_TEXTURE_2D = 0x0de1,
    GL_TEXTURE_MIN_FILTER = 0x2801,
    GL_MAX_TEXTURE_SIZE = 0x0d33,
    GL_NUM_COMPRESSED_TEXTURE_FORMATS = 0x86a2,
    GL_COMPRESSED_TEXTURE_FORMATS = 0x86a3,
};

int main(void) {
    const long page = sysconf(_SC_PAGESIZE);
    unsigned char *guard = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (guard == MAP_FAILED || mprotect(guard + page, (size_t)page, PROT_NONE) != 0)
        return 2;

    int *scalar_i = (int *)(guard + page - sizeof(*scalar_i));
    *scalar_i = 0x2601; /* GL_LINEAR */
    glTexParameteriv(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, scalar_i);
    float *scalar_f = (float *)(guard + page - sizeof(*scalar_f));
    *scalar_f = 1.0f;
    glTexParameterfv(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, scalar_f);
    (void)glGetError();

    struct {
        uint64_t before;
        int value;
        uint64_t after;
    } scalar = {0x1122334455667788ull, 0, 0x8877665544332211ull};
    glGetIntegerv(GL_MAX_TEXTURE_SIZE, &scalar.value);
    if (scalar.before != 0x1122334455667788ull ||
        scalar.after != 0x8877665544332211ull) {
        puts("scalar query overwrote canary");
        return 3;
    }

    int count = 0;
    glGetIntegerv(GL_NUM_COMPRESSED_TEXTURE_FORMATS, &count);
    if (count < 0 || count > 4096) return 4;
    uint64_t *storage = calloc((size_t)(count > 0 ? count : 1) * sizeof(int) + 16, 1);
    if (!storage) return 5;
    unsigned char *bytes = (unsigned char *)storage;
    uint64_t *before = (uint64_t *)bytes;
    int *formats = (int *)(bytes + 8);
    uint64_t *after = (uint64_t *)(bytes + 8 + (size_t)(count > 0 ? count : 1) * sizeof(int));
    *before = 0xa5a55a5af00dcafeull;
    *after = 0x5a5aa5a50dd0faceull;
    glGetIntegerv(GL_COMPRESSED_TEXTURE_FORMATS, formats);
    if (*before != 0xa5a55a5af00dcafeull || *after != 0x5a5aa5a50dd0faceull) {
        puts("variable query overwrote canary");
        return 6;
    }
    printf("gl-memory-safe scalar=%d compressed=%d\n", scalar.value, count);
    return 0;
}
