/* A single-byte image copied through a fragment shader, at the widths that
 * are not word multiples.
 *
 * WebGPU has no `r8uint` storage texture, so a written one goes through the
 * packed route -- a storage buffer the shader packs, copied back into the
 * texture when the render pass closes. Two things have to agree for that to
 * work at a width like 17: the shader's sub-word addressing (several byte
 * texels share a 32-bit word, so a store is an `iand` to clear the field and
 * an `ior` to set it, two atomics) and the row arithmetic of the copy back.
 *
 * Nothing covered it. It was written while chasing
 * KHR-GL45.shader_image_load_store.single-byte_data_alignment, whose failure
 * turned out **not** to be here: this passes at every dimension that test
 * uses, on gex_wgpu and on llvmpipe alike. It is kept because it locks in an
 * area that had no coverage, and because it says where *not* to look next.
 *
 * Two traps it fell into first, both mine rather than the driver's, and both
 * of which made llvmpipe fail too -- which is the tell:
 *   - an image uniform with no `binding=` qualifier defaults to unit 0, so
 *     both images were the same one until glUniform1i set them;
 *   - GL_PACK_ALIGNMENT and GL_UNPACK_ALIGNMENT default to 4, so an odd-width
 *     r8ui row is padded in the client buffer. That alignment is the whole
 *     subject of the test this came from.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static int W = 16, H = 16;
static int cases, passed;

static int run_one(void);

int main(void) {
    static const int dims[][2] = {
        {16,16},{16,17},{17,16},{17,17},{16,18},{18,16},{18,18},{19,19}
    };
    for (unsigned i = 0; i < sizeof(dims)/sizeof(dims[0]); ++i) {
        W = dims[i][0]; H = dims[i][1];
        const int bad = run_one();
        ++cases;
        if (bad == 0) ++passed;
        printf("CASE copy_%dx%d %s\n", W, H, bad == 0 ? "PASS" : "FAIL");
    }
    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}

static int run_one(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 99;
    const EGLint sa[] = { EGL_WIDTH, W, EGL_HEIGHT, H, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 99;
    fprintf(stderr, "version=%s\n", (const char *)glGetString(GL_VERSION));
    /* Tight rows both ways. GL_PACK_ALIGNMENT and GL_UNPACK_ALIGNMENT both
       default to 4, so an odd-width r8ui row is padded in the client buffer --
       which is what made the first version of this probe report bad rows on
       llvmpipe too. The alignment *is* what the test is named for. */
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    while (glGetError() != GL_NO_ERROR) {}

    unsigned char src[512 * 512];
    for (int y = 0; y < H; ++y)
        for (int xx = 0; xx < W; ++xx)
            src[y * W + xx] = (unsigned char)(1 + y * 16 + (xx & 15));

    GLuint tex[2];
    glGenTextures(2, tex);
    glBindTexture(GL_TEXTURE_2D, tex[0]);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8UI, W, H);
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, W, H, GL_RED_INTEGER,
                    GL_UNSIGNED_BYTE, src);
    glBindTexture(GL_TEXTURE_2D, tex[1]);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8UI, W, H);
    unsigned char zero[512 * 512]; memset(zero, 0, sizeof(zero));
    glTexSubImage2D(GL_TEXTURE_2D, 0, 0, 0, W, H, GL_RED_INTEGER,
                    GL_UNSIGNED_BYTE, zero);
    glBindImageTexture(0, tex[1], 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_R8UI);
    glBindImageTexture(1, tex[0], 0, GL_FALSE, 0, GL_READ_ONLY, GL_R8UI);
    while (glGetError() != GL_NO_ERROR) {}

    static const char *vs =
        "#version 430 core\n"
        "void main() {\n"
        "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 3.0,\n"
        "                (gl_VertexID & 2) == 0 ? -1.0 : 3.0);\n"
        "  gl_Position = vec4(p, 0.0, 1.0); }\n";
    static const char *fs =
        "#version 430 core\n"
        "layout(r8ui) writeonly uniform uimage2D dst;\n"
        "layout(r8ui) readonly  uniform uimage2D src;\n"
        "void main() {\n"
        "  uvec4 v = imageLoad(src, ivec2(gl_FragCoord));\n"
        "  imageStore(dst, ivec2(gl_FragCoord), v); }\n";
    GLuint v = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(v, 1, &vs, NULL); glCompileShader(v);
    GLuint f = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(f, 1, &fs, NULL); glCompileShader(f);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, v); glAttachShader(prog, f);
    glLinkProgram(prog);
    GLint ok = 0; glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) { char l[900]; GLsizei ln=0; glGetProgramInfoLog(prog,sizeof l,&ln,l);
        fprintf(stderr, "link failed: %.*s\n", (int)ln, l); return 99; }
    GLuint vao=0; glGenVertexArrays(1,&vao); glBindVertexArray(vao);
    glUseProgram(prog);
    /* Without a `binding=` qualifier both image uniforms default to unit 0,
       which is what made the first version of this probe read zeros on
       llvmpipe as well -- the driver was not the problem. */
    glUniform1i(glGetUniformLocation(prog, "dst"), 0);
    glUniform1i(glGetUniformLocation(prog, "src"), 1);
    while (glGetError() != GL_NO_ERROR) {}
    glViewport(0, 0, W, H);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    glFinish();
    while (glGetError() != GL_NO_ERROR) {}

    unsigned char got[512 * 512]; memset(got, 0xAA, sizeof(got));
    glBindTexture(GL_TEXTURE_2D, tex[1]);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RED_INTEGER, GL_UNSIGNED_BYTE, got);
    while (glGetError() != GL_NO_ERROR) {}

    int bad_rows = 0;
    for (int y = 0; y < H; ++y) {
        int diff = 0, firstx = -1;
        for (int xx = 0; xx < W; ++xx)
            if (got[y*W+xx] != src[y*W+xx]) { if (!diff) firstx = xx; ++diff; }
        if (diff) {
            ++bad_rows;
            /* Where did this row's data actually come from? */
            int from = -1;
            for (int cand = 0; cand < H; ++cand)
                if (got[y*W] == src[cand*W] && got[y*W+1] == src[cand*W+1]) { from = cand; break; }
            fprintf(stderr, "row %d: %d of %d bytes differ (first x=%d) "
                    "got[0]=%u want=%u looks like row %d\n",
                    y, diff, W, firstx, got[y*W], src[y*W], from);
        }
    }
    if (bad_rows)
        fprintf(stderr, "INFO %dx%d: %d of %d rows differ\n", W, H,
                bad_rows, H);
    return bad_rows;
}
