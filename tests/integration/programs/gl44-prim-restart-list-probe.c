/* Primitive restart on the *list* topologies, GL_TRIANGLES and GL_LINES.
 *
 * WebGPU has primitive restart only for strips, and only at a fixed index, so
 * the general GL form -- any index, any topology -- has to be resolved on the
 * CPU. The strips are the cases a driver gets right first, because the
 * hardware does them; the lists are the ones that quietly draw the restart
 * index as though it were a real index, which turns one primitive into two
 * wrong ones and is invisible unless the geometry is arranged to show it.
 *
 * KHR-GL45.primitive_restart.restart_mode fails on exactly LINES and
 * TRIANGLES and passes the strips, which is what this pins down.
 *
 * Each case also checks the *incomplete* run. GL discards a primitive the
 * restart leaves unfinished, and a driver that instead draws it -- or that
 * shifts the following vertices into it -- fails here while passing the
 * complete case.
 *
 *   tri_restart      [0 1 2 R 3 4 5] -- two triangles, one each side
 *   tri_incomplete   [0 1 R 3 4 5]   -- the (0,1) run is dropped
 *   line_restart     [0 1 R 2 3]     -- two lines, one in each half
 *   line_incomplete  [0 R 2 3]       -- the lone index 0 is dropped
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIDE 32
#define RESTART 0xffffu

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static const char *vs_src =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *fs_src =
    "#version 330 core\n"
    "out vec4 colour;\n"
    "void main() { colour = vec4(1.0); }\n";

/* Two triangles that do not touch: one strictly left of centre, one strictly
 * right, so "was it drawn" is a count of covered pixels in each half. */
static const float tri_pos[6][2] = {
    {-0.9f, -0.9f}, {-0.2f, -0.9f}, {-0.9f, 0.9f},
    { 0.2f, -0.9f}, { 0.9f, -0.9f}, { 0.9f, 0.9f},
};
/* Two horizontal segments, one in each half vertically. */
static const float line_pos[4][2] = {
    {-0.8f, -0.5f}, {0.8f, -0.5f},
    {-0.8f,  0.5f}, {0.8f,  0.5f},
};

int main(void)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) { printf("FATAL eglInitialize\n"); return 1; }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                                       EGL_NONE};
    EGLConfig cfg; EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) { printf("FATAL eglCreateContext\n"); return 1; }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    glShaderSource(vs, 1, &vs_src, NULL);
    glCompileShader(vs);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(fs, 1, &fs_src, NULL);
    glCompileShader(fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glLinkProgram(prog);
    glUseProgram(prog);

    GLuint vao = 0, vbo = 0, ibo = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &vbo);
    glGenBuffers(1, &ibo);

    GLuint tex = 0, fbo = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }
    glViewport(0, 0, SIDE, SIDE);
    glDisable(GL_DEPTH_TEST);
    glEnable(GL_PRIMITIVE_RESTART);
    glPrimitiveRestartIndex(RESTART);

    struct {
        const char *name;
        GLenum mode;
        const float (*positions)[2];
        unsigned vertex_count;
        GLushort indices[8];
        unsigned index_count;
        /* Whether each region must have been covered. For triangles the
         * regions are the left and right halves; for lines, bottom and top. */
        int want_first, want_second;
        int vertical;
    } runs[] = {
        {"tri_restart", GL_TRIANGLES, tri_pos, 6,
         {0, 1, 2, RESTART, 3, 4, 5}, 7, 1, 1, 0},
        {"tri_incomplete", GL_TRIANGLES, tri_pos, 6,
         {0, 1, RESTART, 3, 4, 5}, 6, 0, 1, 0},
        {"line_restart", GL_LINES, line_pos, 4,
         {0, 1, RESTART, 2, 3}, 5, 1, 1, 1},
        {"line_incomplete", GL_LINES, line_pos, 4,
         {0, RESTART, 2, 3}, 4, 0, 1, 1},
    };

    for (unsigned r = 0; r < 4; ++r) {
        glBindBuffer(GL_ARRAY_BUFFER, vbo);
        glBufferData(GL_ARRAY_BUFFER,
                     (GLsizeiptr)(runs[r].vertex_count * 2 * sizeof(float)),
                     runs[r].positions, GL_STATIC_DRAW);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, ibo);
        glBufferData(GL_ELEMENT_ARRAY_BUFFER,
                     (GLsizeiptr)(runs[r].index_count * sizeof(GLushort)),
                     runs[r].indices, GL_STATIC_DRAW);

        glClearColor(0, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawElements(runs[r].mode, (GLsizei)runs[r].index_count,
                       GL_UNSIGNED_SHORT, NULL);

        unsigned char px[SIDE * SIDE * 4];
        memset(px, 0, sizeof px);
        glReadPixels(0, 0, SIDE, SIDE, GL_RGBA, GL_UNSIGNED_BYTE, px);
        unsigned first = 0, second = 0;
        for (int y = 0; y < SIDE; ++y) {
            for (int x = 0; x < SIDE; ++x) {
                if (px[(y * SIDE + x) * 4] < 128)
                    continue;
                const int low = runs[r].vertical ? y < SIDE / 2 : x < SIDE / 2;
                if (low) ++first; else ++second;
            }
        }
        fprintf(stderr, "INFO %s covered first=%u second=%u want %d/%d "
                "err=0x%04x\n", runs[r].name, first, second,
                runs[r].want_first, runs[r].want_second,
                (unsigned)glGetError());
        result(runs[r].name,
               (first > 0) == (runs[r].want_first != 0) &&
               (second > 0) == (runs[r].want_second != 0));
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
