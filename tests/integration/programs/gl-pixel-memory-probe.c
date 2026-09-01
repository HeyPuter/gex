#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

typedef void *EGLDisplay;
typedef void *EGLConfig;
typedef void *EGLContext;
typedef void *EGLSurface;
typedef int EGLint;
extern EGLDisplay eglGetDisplay(void *);
extern unsigned eglInitialize(EGLDisplay, EGLint *, EGLint *);
extern unsigned eglBindAPI(unsigned);
extern unsigned eglChooseConfig(EGLDisplay, const EGLint *, EGLConfig *, EGLint, EGLint *);
extern EGLContext eglCreateContext(EGLDisplay, EGLConfig, EGLContext, const EGLint *);
extern EGLSurface eglCreatePbufferSurface(EGLDisplay, EGLConfig, const EGLint *);
extern unsigned eglMakeCurrent(EGLDisplay, EGLSurface, EGLSurface, EGLContext);

extern void glGenTextures(int, unsigned *);
extern void glBindTexture(unsigned, unsigned);
extern void glTexParameteri(unsigned, unsigned, int);
extern void glTexImage2D(unsigned, int, int, int, int, int, unsigned, unsigned, const void *);
extern void glTexSubImage2D(unsigned, int, int, int, int, int, unsigned, unsigned, const void *);
extern void glTexImage1D(unsigned, int, int, int, int, unsigned, unsigned, const void *);
extern void glTexSubImage1D(unsigned, int, int, int, unsigned, unsigned, const void *);
extern void glTexImage3D(unsigned, int, int, int, int, int, int, unsigned, unsigned, const void *);
extern void glTexSubImage3D(unsigned, int, int, int, int, int, int, int, unsigned, unsigned, const void *);
extern void glCompressedTexImage2D(unsigned, int, unsigned, int, int, int, int, const void *);
extern void glGetTexImage(unsigned, int, unsigned, unsigned, void *);
extern void glGetTexLevelParameteriv(unsigned, int, unsigned, int *);
extern void glPixelStorei(unsigned, int);
extern void glClearColor(float, float, float, float);
extern void glClear(unsigned);
extern void glReadPixels(int, int, int, int, unsigned, unsigned, void *);
extern void glGenBuffers(int, unsigned *);
extern void glBindBuffer(unsigned, unsigned);
extern void glBufferData(unsigned, long, const void *, unsigned);
extern void glGetBufferSubData(unsigned, long, long, void *);
extern void glGetBufferParameteriv(unsigned, unsigned, int *);
extern void *glMapBuffer(unsigned, unsigned);
extern unsigned char glUnmapBuffer(unsigned);
extern void glFinish(void);
extern unsigned glGetError(void);

enum {
    EGL_NONE = 0x3038, EGL_SURFACE_TYPE = 0x3033, EGL_PBUFFER_BIT = 1,
    EGL_RENDERABLE_TYPE = 0x3040, EGL_OPENGL_BIT = 8,
    EGL_WIDTH = 0x3057, EGL_HEIGHT = 0x3056, EGL_OPENGL_API = 0x30a2,
    EGL_CONTEXT_MAJOR_VERSION = 0x3098, EGL_CONTEXT_MINOR_VERSION = 0x30fb,
    GL_TEXTURE_1D = 0x0de0, GL_TEXTURE_2D = 0x0de1, GL_TEXTURE_3D = 0x806f,
    GL_TEXTURE_MIN_FILTER = 0x2801, GL_NEAREST = 0x2600,
    GL_RGB = 0x1907, GL_RGBA = 0x1908, GL_RGB8 = 0x8051,
    GL_UNSIGNED_BYTE = 0x1401, GL_COLOR_BUFFER_BIT = 0x4000,
    GL_PACK_ALIGNMENT = 0x0d05, GL_PACK_ROW_LENGTH = 0x0d02,
    GL_PACK_SKIP_ROWS = 0x0d03, GL_PACK_SKIP_PIXELS = 0x0d04,
    GL_UNPACK_ALIGNMENT = 0x0cf5,
    GL_PIXEL_PACK_BUFFER = 0x88eb, GL_PIXEL_UNPACK_BUFFER = 0x88ec,
    GL_ARRAY_BUFFER = 0x8892, GL_BUFFER_SIZE = 0x8764,
    GL_STREAM_READ = 0x88e1, GL_STATIC_DRAW = 0x88e4,
    GL_WRITE_ONLY = 0x88b9,
    GL_TEXTURE_WIDTH = 0x1000, GL_TEXTURE_HEIGHT = 0x1001,
    GL_INVALID_ENUM = 0x0500, GL_INVALID_OPERATION = 0x0502,
    GL_COMPRESSED_RGBA_S3TC_DXT5_EXT = 0x83f3,
};

static int context(void) {
    EGLDisplay d = eglGetDisplay(NULL);
    EGLint major = 0, minor = 0, count = 0;
    EGLConfig config = NULL;
    const EGLint config_attrs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE
    };
    const EGLint surface_attrs[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    const EGLint context_attrs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE
    };
    if (!d || !eglInitialize(d, &major, &minor) || !eglBindAPI(EGL_OPENGL_API) ||
        !eglChooseConfig(d, config_attrs, &config, 1, &count) || count != 1)
        return 0;
    EGLSurface surface = eglCreatePbufferSurface(d, config, surface_attrs);
    EGLContext ctx = eglCreateContext(d, config, NULL, context_attrs);
    return surface && ctx && eglMakeCurrent(d, surface, surface, ctx);
}

static int close_to(unsigned char value, unsigned char expected) {
    return value + 3 >= expected && value <= expected + 3;
}

int main(void) {
    if (!context()) { puts("context failed"); return 2; }
    const long page = sysconf(_SC_PAGESIZE);
    unsigned char *mapping = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED || mprotect(mapping + page, (size_t)page, PROT_NONE))
        return 3;

    /* RGB/ubyte, width 3, default alignment 4: 9 active bytes, a 12-byte row
     * stride, then 9 active bytes. The final row has no trailing padding, so
     * the exact accessible span is 21 bytes. */
    unsigned char *upload = mapping + page - 21;
    const unsigned char expected[21] = {
        255,0,0, 0,255,0, 0,0,255, 0xaa,0xbb,0xcc,
        255,255,0, 0,255,255, 255,0,255
    };
    memcpy(upload, expected, sizeof(expected));
    unsigned texture = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGB8, 3, 2, 0,
                 GL_RGB, GL_UNSIGNED_BYTE, upload);
    if (glGetError()) { puts("guarded texture upload failed"); return 4; }

    unsigned char roundtrip[21];
    memset(roundtrip, 0x5a, sizeof(roundtrip));
    glPixelStorei(GL_PACK_ALIGNMENT, 4);
    int texture_width = -1, texture_height = -1;
    glGetTexLevelParameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &texture_width);
    glGetTexLevelParameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &texture_height);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RGB, GL_UNSIGNED_BYTE, roundtrip);
    if (memcmp(roundtrip, expected, 9) || memcmp(roundtrip + 12, expected + 12, 9)) {
        printf("aligned RGB texture roundtrip mismatch size=%dx%d error=0x%x\n",
               texture_width, texture_height, glGetError());
        fputs("expected:", stdout);
        for (int i = 0; i < 21; ++i) printf(" %02x", expected[i]);
        fputs("\nactual:  ", stdout);
        for (int i = 0; i < 21; ++i) printf(" %02x", roundtrip[i]);
        putchar('\n');
        return 5;
    }

    /* PACK skips must leave the caller's prefix and inter-row padding intact.
     * rowStride=align(5*3,4)=16, offset=16+2*3=22, end=22+16+9=47. */
    unsigned char readback[47];
    memset(readback, 0xcd, sizeof(readback));
    glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glPixelStorei(GL_PACK_ALIGNMENT, 4);
    glPixelStorei(GL_PACK_ROW_LENGTH, 5);
    glPixelStorei(GL_PACK_SKIP_ROWS, 1);
    glPixelStorei(GL_PACK_SKIP_PIXELS, 2);
    glReadPixels(0, 0, 3, 2, GL_RGB, GL_UNSIGNED_BYTE, readback);
    for (int i = 0; i < 22; ++i)
        if (readback[i] != 0xcd) { puts("readback overwrote skipped prefix"); return 6; }
    for (int i = 31; i < 38; ++i)
        if (readback[i] != 0xcd) { puts("readback overwrote row padding"); return 7; }
    for (int row = 0; row < 2; ++row) for (int col = 0; col < 3; ++col) {
        unsigned char *px = readback + 22 + row * 16 + col * 3;
        if (!close_to(px[0], 64) || !close_to(px[1], 128) || !close_to(px[2], 191)) {
            puts("readback pixel mismatch"); return 8;
        }
    }

    /* A small numeric pointer is an offset when a pixel buffer is bound. If
     * the guest marshaller treats it as client memory, either call faults. */
    unsigned buffers[2] = {0, 0};
    unsigned char pbo_data[128];
    memset(pbo_data, 0x7f, sizeof(pbo_data));
    glGenBuffers(2, buffers);
    /* A NULL allocation larger than the command ring must remain a tiny
     * command carrying only its size; older encoding copied uninitialized
     * ring bytes and silently dropped this call. */
    glBindBuffer(GL_ARRAY_BUFFER, buffers[0]);
    glBufferData(GL_ARRAY_BUFFER, 40L * 1024L * 1024L, NULL, GL_STATIC_DRAW);
    int large_buffer_size = 0;
    glGetBufferParameteriv(GL_ARRAY_BUFFER, GL_BUFFER_SIZE, &large_buffer_size);
    if (large_buffer_size != 40 * 1024 * 1024) {
        puts("large NULL buffer allocation was truncated or dropped");
        return 9;
    }

    /* Mapping state belongs to a buffer object, not its binding target. Bind
     * a smaller object in between so target-keyed bookkeeping would allocate
     * only 16 bytes when the 64-byte object is mapped again. */
    unsigned mapped_buffers[2] = {0, 0};
    glGenBuffers(2, mapped_buffers);
    glBindBuffer(GL_ARRAY_BUFFER, mapped_buffers[0]);
    glBufferData(GL_ARRAY_BUFFER, 64, NULL, GL_STATIC_DRAW);
    glBindBuffer(GL_ARRAY_BUFFER, mapped_buffers[1]);
    glBufferData(GL_ARRAY_BUFFER, 16, NULL, GL_STATIC_DRAW);
    glBindBuffer(GL_ARRAY_BUFFER, mapped_buffers[0]);
    unsigned char *mapped = glMapBuffer(GL_ARRAY_BUFFER, GL_WRITE_ONLY);
    if (!mapped) { puts("object-keyed buffer mapping failed"); return 10; }
    memset(mapped, 0x39, 64);
    if (!glUnmapBuffer(GL_ARRAY_BUFFER)) { puts("object-keyed buffer unmap failed"); return 11; }
    unsigned char mapped_result[64];
    memset(mapped_result, 0, sizeof(mapped_result));
    glGetBufferSubData(GL_ARRAY_BUFFER, 0, sizeof(mapped_result), mapped_result);
    for (unsigned i = 0; i < sizeof(mapped_result); ++i)
        if (mapped_result[i] != 0x39) { puts("mapped buffer object content mismatch"); return 12; }
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, buffers[0]);
    glBufferData(GL_PIXEL_UNPACK_BUFFER, sizeof(pbo_data), pbo_data, GL_STATIC_DRAW);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGB8, 2, 2, 0,
                 GL_RGB, GL_UNSIGNED_BYTE, (const void *)16);

    /* Unsupported compressed formats are not advertised. They must produce
     * INVALID_ENUM rather than following a PBO offset or aborting through an
     * unimplemented exception. */
    while (glGetError()) {}
    glCompressedTexImage2D(GL_TEXTURE_2D, 0, GL_COMPRESSED_RGBA_S3TC_DXT5_EXT,
                           4, 4, 0, 16, (const void *)124);
    if (glGetError() != GL_INVALID_ENUM) {
        puts("unsupported compressed upload did not report GL_INVALID_ENUM");
        return 13;
    }

    /* The offset itself is in bounds, but the 12-byte transfer extends beyond
     * the 128-byte PBO. This must report INVALID_OPERATION without reading
     * beyond the buffer or disturbing the preceding valid upload. */
    while (glGetError()) {}
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, 2, 2,
                    GL_RGB, GL_UNSIGNED_BYTE, (const void *)124);
    if (glGetError() != GL_INVALID_OPERATION) {
        puts("short pixel-unpack buffer range did not report GL_INVALID_OPERATION");
        return 14;
    }

    /* Exercise the generated 1D and 3D transports too. These used to be
     * absent or to treat the numeric PBO offset as a guest address. */
    unsigned dimensional[2] = {0, 0};
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
    glGenTextures(2, dimensional);
    glBindTexture(GL_TEXTURE_1D, dimensional[0]);
    glTexImage1D(GL_TEXTURE_1D, 0, GL_RGB8, 4, 0, GL_RGB, GL_UNSIGNED_BYTE, NULL);
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, buffers[0]);
    while (glGetError()) {}
    glTexSubImage1D(GL_TEXTURE_1D, 0, 0, 2, GL_RGB, GL_UNSIGNED_BYTE, (const void *)126);
    if (glGetError() != GL_INVALID_OPERATION) {
        puts("short 1D pixel-unpack buffer range did not report GL_INVALID_OPERATION");
        return 15;
    }
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
    glBindTexture(GL_TEXTURE_3D, dimensional[1]);
    glTexImage3D(GL_TEXTURE_3D, 0, GL_RGB8, 2, 2, 2, 0,
                 GL_RGB, GL_UNSIGNED_BYTE, NULL);
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, buffers[0]);
    while (glGetError()) {}
    glTexSubImage3D(GL_TEXTURE_3D, 0, 0, 0, 0, 1, 2, 2,
                    GL_RGB, GL_UNSIGNED_BYTE, (const void *)124);
    if (glGetError() != GL_INVALID_OPERATION) {
        puts("short 3D pixel-unpack buffer range did not report GL_INVALID_OPERATION");
        return 16;
    }
    glBindBuffer(GL_PIXEL_UNPACK_BUFFER, 0);
    glBindTexture(GL_TEXTURE_2D, texture);
    unsigned char unpacked[12];
    memset(unpacked, 0, sizeof(unpacked));
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glPixelStorei(GL_PACK_ROW_LENGTH, 0);
    glPixelStorei(GL_PACK_SKIP_ROWS, 0);
    glPixelStorei(GL_PACK_SKIP_PIXELS, 0);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RGB, GL_UNSIGNED_BYTE, unpacked);
    for (unsigned i = 0; i < sizeof(unpacked); ++i)
        if (unpacked[i] != 0x7f) { puts("pixel-unpack buffer content mismatch"); return 17; }
    glBindBuffer(GL_PIXEL_PACK_BUFFER, buffers[1]);
    glBufferData(GL_PIXEL_PACK_BUFFER, sizeof(pbo_data), NULL, GL_STREAM_READ);
    glPixelStorei(GL_PACK_ROW_LENGTH, 0);
    glPixelStorei(GL_PACK_SKIP_ROWS, 0);
    glPixelStorei(GL_PACK_SKIP_PIXELS, 0);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, 2, 2, GL_RGBA, GL_UNSIGNED_BYTE, (void *)16);
    glFinish();
    unsigned char packed[16];
    memset(packed, 0, sizeof(packed));
    glGetBufferSubData(GL_PIXEL_PACK_BUFFER, 16, sizeof(packed), packed);
    for (int pixel = 0; pixel < 4; ++pixel) {
        unsigned char *px = packed + pixel * 4;
        if (!close_to(px[0], 64) || !close_to(px[1], 128) ||
            !close_to(px[2], 191) || px[3] != 255) {
            puts("pixel-pack buffer content mismatch"); return 18;
        }
    }
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RGB, GL_UNSIGNED_BYTE, (void *)48);
    unsigned char packed_texture[12];
    memset(packed_texture, 0, sizeof(packed_texture));
    glGetBufferSubData(GL_PIXEL_PACK_BUFFER, 48, sizeof(packed_texture), packed_texture);
    for (unsigned i = 0; i < sizeof(packed_texture); ++i)
        if (packed_texture[i] != 0x7f) {
            puts("GetTexImage pixel-pack buffer content mismatch"); return 19;
        }
    if (glGetError()) { puts("pixel-buffer offset transfer failed"); return 20; }

    puts("gl-pixel-memory-safe map=object-keyed pbo=bounded compressed=error-not-abort");
    return 0;
}
