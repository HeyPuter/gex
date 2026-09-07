/* Whether a shader invocation reads back what it just stored to an image.
 *
 * GL guarantees this without any barrier: within one invocation, a load from
 * the same address as a preceding store to the same image sees that store. No
 * memory model question, no ordering between invocations -- program order in
 * one thread.
 *
 * KHR-GL45.es_31_compatibility.shader_image_load_store.advanced-memory-order
 * tests exactly it, in both stages at once, and fails here with no rejection,
 * no decline and no GL error -- a silent wrong answer, which is the shape
 * worth having a minimal reproducer for. Its fragment shader is
 *
 *     for (int i = 0; i < 3; ++i) {
 *       imageStore(g_image, coord, vec4(i));
 *       vec4 v = imageLoad(g_image, coord);   // must be vec4(i)
 *     }
 *
 * and its vertex shader does the same to an imageBuffer. This probe asks the
 * two halves separately, because the driver serves them by different routes: a
 * storage *texture* for image2D, and a storage *buffer* -- the packed route --
 * for an imageBuffer.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void eq(const char *name, long long got, long long want) {
    ++cases;
    const int ok = got == want;
    if (ok) ++passed;
    fprintf(stderr, "INFO %-28s got=%lld want=%lld\n", name, got, want);
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}
static void drain(void) { while (glGetError() != GL_NO_ERROR) { } }

static GLuint build(const char *vs_src, const char *fs_src) {
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL); glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL); glCompileShader(fs);
    GLuint p = glCreateProgram();
    glAttachShader(p, vs); glAttachShader(p, fs);
    glLinkProgram(p);
    GLint ok = 0; glGetProgramiv(p, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1024]; GLsizei n = 0;
        glGetProgramInfoLog(p, sizeof(log), &n, log);
        fprintf(stderr, "INFO link failed: %.*s\n", (int)n, log);
        return 0;
    }
    return p;
}

static const char *passthrough_vs =
    "#version 430 core\n"
    "void main() {\n"
    "  vec2 p = vec2((gl_VertexID & 1) == 0 ? -1.0 : 3.0,\n"
    "                (gl_VertexID & 2) == 0 ? -1.0 : 3.0);\n"
    "  gl_Position = vec4(p, 0.0, 1.0); }\n";

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));
    drain();

    GLuint vao = 0; glGenVertexArrays(1, &vao); glBindVertexArray(vao);
    /* A one-texel r32ui result buffer, so the answer is a number rather than a
       colour: the shader reports how many of the three iterations read back
       what they had just written. */
    GLuint result = 0;
    glGenBuffers(1, &result);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, result);
    const GLuint zero[4] = {0, 0, 0, 0};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(zero), zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, result);
    drain();

    /* --- a storage texture, read back in the fragment stage --------------- */
    {
        GLuint tex = 0;
        glGenTextures(1, &tex);
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32F, 1, 1);
        glBindImageTexture(0, tex, 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32F);
        GLuint prog = build(passthrough_vs,
            "#version 430 core\n"
            "layout(r32f, binding = 0) uniform image2D img;\n"
            "layout(std430, binding = 0) buffer Out { uint agreed; };\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uint ok = 0u;\n"
            "  for (int i = 1; i <= 3; ++i) {\n"
            "    imageStore(img, ivec2(0, 0), vec4(float(i)));\n"
            "    if (imageLoad(img, ivec2(0, 0)).r == float(i)) ok++;\n"
            "  }\n"
            "  agreed = ok;\n"
            "  colour = vec4(1.0); }\n");
        eq("texture_program_links", prog != 0, 1);
        if (prog) {
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_ALL_BARRIER_BITS);
            glFinish();
            GLuint back[4] = {9, 9, 9, 9};
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, result);
            glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), back);
            drain();
            eq("image2d_selfread_3_of_3", back[0], 3);
        }
    }

    /* --- a buffer image, which the driver serves as a storage buffer ------ */
    {
        const GLuint zero2[4] = {0, 0, 0, 0};
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, result);
        glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(zero2), zero2);
        GLuint texbuf = 0, buf = 0;
        glGenBuffers(1, &buf);
        glBindBuffer(GL_TEXTURE_BUFFER, buf);
        glBufferData(GL_TEXTURE_BUFFER, 16, NULL, GL_DYNAMIC_COPY);
        glGenTextures(1, &texbuf);
        glBindTexture(GL_TEXTURE_BUFFER, texbuf);
        glTexBuffer(GL_TEXTURE_BUFFER, GL_R32F, buf);
        glBindImageTexture(1, texbuf, 0, GL_FALSE, 0, GL_READ_WRITE, GL_R32F);
        GLuint prog = build(passthrough_vs,
            "#version 430 core\n"
            "layout(r32f, binding = 1) uniform imageBuffer img;\n"
            "layout(std430, binding = 0) buffer Out { uint agreed; };\n"
            "out vec4 colour;\n"
            "void main() {\n"
            "  uint ok = 0u;\n"
            "  for (int i = 1; i <= 3; ++i) {\n"
            "    imageStore(img, 0, vec4(float(i)));\n"
            "    if (imageLoad(img, 0).r == float(i)) ok++;\n"
            "  }\n"
            "  agreed = ok;\n"
            "  colour = vec4(1.0); }\n");
        eq("buffer_program_links", prog != 0, 1);
        if (prog) {
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
            glMemoryBarrier(GL_ALL_BARRIER_BITS);
            glFinish();
            GLuint back[4] = {9, 9, 9, 9};
            glBindBuffer(GL_SHADER_STORAGE_BUFFER, result);
            glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(back), back);
            drain();
            eq("imagebuffer_selfread_3_of_3", back[0], 3);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n",
           cases, passed, cases - passed);
    return cases == passed ? 0 : 1;
}
