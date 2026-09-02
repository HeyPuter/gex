/* Texture upload under the pixel-store modes, read back with texelFetch.
 *
 * This is a reduction of KHR-GL33.pixelstoragemodes, which fails all 896 of
 * its cases on the WebGPU path -- including `teximage2d.r8.0_0`, which is the
 * *default* pixel-store state and the simplest format the family has. A whole
 * family failing at its easiest case says the fault is not in the storage
 * modes, so this checks the pieces separately: first a plain upload with every
 * mode left alone, then one mode at a time.
 *
 * The CTS verifies the same way -- texelFetch in a fragment shader, comparing
 * against a reference -- so a difference between this and a real driver is the
 * same difference the CTS is reporting, with a name attached.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define WIDTH 8
#define HEIGHT 8

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

/* texelFetch by integer pixel coordinate: fragment (x,y) reads texel (x,y),
 * so the framebuffer comes back as a copy of the texture and every texel is
 * checked independently. */
/* The shape KHR-GL33.pixelstoragemodes actually uses: the texel coordinate
 * arrives as an interpolated varying computed from the vertex position, not
 * from gl_FragCoord. */
static const char *varying_vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "out vec2 texcoords;\n"
    "void main() { texcoords = pos; gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *varying_fragment_source =
    "#version 330 core\n"
    "in vec2 texcoords;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    fragColour = vec4(texcoords * 0.5 + 0.5, 0.0, 1.0);\n"
    "}\n";

/* A faithful copy of the CTS family's own shader pair: coordinate as an
 * interpolated varying, texelFetch by ivec2 of it, and an *exact* vec3
 * comparison against a reference colour, green on match. */
static const char *cts_fragment_source =
    "#version 330 core\n"
    "in vec2 texcoords;\n"
    "uniform sampler2D sampler;\n"
    "uniform vec4 refcolour;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    vec4 colour = texelFetch(sampler, ivec2(texcoords), 0);\n"
    "    if (colour.rgb == refcolour.rgb)\n"
    "        fragColour = vec4(0.0, 1.0, 0.0, 1.0);\n"
    "    else\n"
    "        fragColour = vec4(colour);\n"
    "}\n";

static const char *constant_fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static const char *fragment_source =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    ivec2 at = ivec2(gl_FragCoord.xy);\n"
    "    fragColour = vec4(texelFetch(tex, at, 0).r, 0.0, 0.0, 1.0);\n"
    "}\n";

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

/* Upload `pixels` as an R8 texture and read the result back through the
 * shader. Returns non-zero when every texel came back as it went in. */
static int upload_and_check(const char *name, GLuint texture,
                            const unsigned char *pixels, int row_length,
                            int alignment, const unsigned char *expected) {
    fprintf(stderr, "STEP %s begin\n", name);
    glBindTexture(GL_TEXTURE_2D, texture);
    glPixelStorei(GL_UNPACK_ALIGNMENT, alignment);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, row_length);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, WIDTH, HEIGHT, 0, GL_RED,
                 GL_UNSIGNED_BYTE, pixels);
    glPixelStorei(GL_UNPACK_ROW_LENGTH, 0);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
    const GLenum upload_error = glGetError();
    fprintf(stderr, "STEP %s uploaded\n", name);

    glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    fprintf(stderr, "STEP %s drew\n", name);

    unsigned char got[WIDTH * HEIGHT * 4];
    memset(got, 0xAB, sizeof(got));
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, got);
    const GLenum error = glGetError();
    fprintf(stderr, "STEP %s read\n", name);

    int ok = upload_error == GL_NO_ERROR && error == GL_NO_ERROR;
    for (int y = 0; y < HEIGHT; ++y) {
        for (int x = 0; x < WIDTH; ++x) {
            const unsigned char want = expected[y * WIDTH + x];
            const unsigned char have = got[(y * WIDTH + x) * 4];
            if (have != want) ok = 0;
        }
    }
    fprintf(stderr, "INFO %s upload_err=0x%04x read_err=0x%04x"
            " row0_got=%u,%u,%u,%u row0_want=%u,%u,%u,%u\n",
            name, (unsigned)upload_error, (unsigned)error,
            got[0], got[4], got[8], got[12],
            expected[0], expected[1], expected[2], expected[3]);
    return ok;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 2;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[1024] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "link failed: %s\n", log);
        return 2;
    }
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "tex"), 0);

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, WIDTH, HEIGHT, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, WIDTH, HEIGHT);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float positions[12] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,
         1.0f, -1.0f,  1.0f,  1.0f,  -1.0f, 1.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    GLuint texture = 0;
    glGenTextures(1, &texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);

    /* Distinct, non-zero, and not a ramp that a stride mistake would still
     * satisfy: a wrong row pitch has to change the answer. */
    unsigned char base[WIDTH * HEIGHT];
    for (int i = 0; i < WIDTH * HEIGHT; ++i)
        base[i] = (unsigned char)(10 + i * 3);

    /* 1. Default pixel-store state. GL_UNPACK_ALIGNMENT is 4 by default and a
     *    4-wide R8 row is already 4 bytes, so no padding is involved -- this
     *    is the plainest upload the API has. */
    result("default", upload_and_check("default", texture, base, 0, 4, base));

    /* 2. Alignment 1, same data: the rows are 4 bytes either way, so the
     *    result must be identical. Any difference is the alignment being
     *    applied where it should not be. */
    result("alignment1", upload_and_check("alignment1", texture, base, 0, 1,
                                          base));

    /* 3. GL_UNPACK_ROW_LENGTH: the source rows are 8 bytes apart but only the
     *    first 4 of each are uploaded. */
    unsigned char wide[2 * WIDTH * HEIGHT];
    memset(wide, 0, sizeof(wide));
    for (int y = 0; y < HEIGHT; ++y)
        for (int x = 0; x < WIDTH; ++x)
            wide[y * 2 * WIDTH + x] = base[y * WIDTH + x];
    result("row_length", upload_and_check("row_length", texture, wide, 2 * WIDTH, 1,
                                          base));

    /* Framebuffer 0, reported but deliberately NOT counted as a case.
     *
     * KHR-GL33.pixelstoragemodes verifies by rendering to the default
     * framebuffer and reading it back (glcPixelStorageModesTests.cpp:775),
     * and fails all 896 of its cases, so this is worth measuring. It cannot
     * be *gated*, because the gate's reference is native Mesa under
     * EGL_PLATFORM=surfaceless and its default framebuffer is not sound
     * there: it answers draw=BACK read=FRONT, and merely querying
     * GL_READ_BUFFER beforehand changes which buffer the readback returns.
     * A reference that moves when you look at it cannot decide anything.
     *
     * So this asks a question that needs no reference: clear framebuffer 0 to
     * an unmistakable colour and read the same buffer straight back. Draw
     * buffer and read buffer are printed alongside, because the answer only
     * means something when they are the same. */
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glViewport(0, 0, WIDTH, HEIGHT);
    GLint draw_buffer = 0, read_buffer = 0;
    GLboolean doubled = GL_FALSE;
    glGetIntegerv(GL_DRAW_BUFFER, &draw_buffer);
    glGetIntegerv(GL_READ_BUFFER, &read_buffer);
    glGetBooleanv(GL_DOUBLEBUFFER, &doubled);
    glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    unsigned char cleared[4] = {0xAB, 0xAB, 0xAB, 0xAB};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, cleared);
    fprintf(stderr,
            "INFO fb0 draw=0x%04x read=0x%04x doubled=%d status=0x%04x "
            "cleared_to_blue_reads=%u,%u,%u,%u %s\n",
            (unsigned)draw_buffer, (unsigned)read_buffer, (int)doubled,
            (unsigned)glCheckFramebufferStatus(GL_FRAMEBUFFER),
            cleared[0], cleared[1], cleared[2], cleared[3],
            (draw_buffer == read_buffer && cleared[2] < 128)
               ? "<-- BROKEN: same buffer cleared and read" : "");

    /* Does a *varying* interpolate correctly when the target is framebuffer 0?
     * This is the shape the CTS family uses, and the one thing the cases
     * above do not exercise -- they take their coordinate from
     * gl_FragCoord. The quad spans NDC -1..1, so the centre pixel must read
     * back about 127,127 and the corners near 0 and 255. */
    {
        GLuint vvs = compile(GL_VERTEX_SHADER, varying_vertex_source);
        GLuint vfs = compile(GL_FRAGMENT_SHADER, varying_fragment_source);
        GLuint vprog = glCreateProgram();
        glAttachShader(vprog, vvs);
        glAttachShader(vprog, vfs);
        glLinkProgram(vprog);
        GLint vlinked = 0;
        glGetProgramiv(vprog, GL_LINK_STATUS, &vlinked);
        glUseProgram(vprog);
        glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);
        unsigned char mid[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        unsigned char corner[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        glReadPixels(WIDTH / 2, HEIGHT / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, mid);
        glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, corner);
        fprintf(stderr, "INFO fb0 varying linked=%d err=0x%04x mid=%u,%u,%u,%u "
                "corner=%u,%u,%u,%u (mid should be ~127,~127)\n", (int)vlinked,
                (unsigned)glGetError(), mid[0], mid[1], mid[2], mid[3],
                corner[0], corner[1], corner[2], corner[3]);
        glUseProgram(program);
    }

    /* The CTS family's own shape, end to end: R8 texture filled with 255,
     * refcolour (1,0,0,1), exact rgb compare, rendered to framebuffer 0.
     * Green means the fetch matched; anything else is the colour it fetched,
     * which is what the CTS would be looking at when it reports Fail. */
    {
        GLuint cvs = compile(GL_VERTEX_SHADER, varying_vertex_source);
        GLuint cfs2 = compile(GL_FRAGMENT_SHADER, cts_fragment_source);
        GLuint ctsprog = glCreateProgram();
        glAttachShader(ctsprog, cvs);
        glAttachShader(ctsprog, cfs2);
        glLinkProgram(ctsprog);
        GLint clinked = 0;
        glGetProgramiv(ctsprog, GL_LINK_STATUS, &clinked);
        if (!clinked) {
            char log[512] = {0};
            glGetProgramInfoLog(ctsprog, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO fb0 cts_shape link_log=%s\n", log);
        }
        glUseProgram(ctsprog);
        glUniform1i(glGetUniformLocation(ctsprog, "sampler"), 0);
        glUniform4f(glGetUniformLocation(ctsprog, "refcolour"),
                    1.0f, 0.0f, 0.0f, 1.0f);
        unsigned char full[WIDTH * HEIGHT];
        memset(full, 255, sizeof(full));
        glBindTexture(GL_TEXTURE_2D, texture);
        glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, WIDTH, HEIGHT, 0, GL_RED,
                     GL_UNSIGNED_BYTE, full);
        glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
        glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);
        unsigned char cts[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        glReadPixels(WIDTH / 2, HEIGHT / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, cts);
        fprintf(stderr, "INFO fb0 cts_shape linked=%d err=0x%04x reads=%u,%u,%u,%u"
                " (0,255,0,255 = matched)\n", (int)clinked,
                (unsigned)glGetError(), cts[0], cts[1], cts[2], cts[3]);
        glUseProgram(program);
    }

    /* And now the same draw the three FBO cases above make, but into
     * framebuffer 0. The clear above proves the buffer is readable, so
     * whatever this returns is about the draw. */
    glBindTexture(GL_TEXTURE_2D, texture);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, WIDTH, HEIGHT, 0, GL_RED,
                 GL_UNSIGNED_BYTE, base);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    unsigned char drawn[WIDTH * HEIGHT * 4];
    memset(drawn, 0xAB, sizeof(drawn));
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, drawn);
    fprintf(stderr, "INFO fb0 after_draw px(0,0)=%u,%u,%u,%u px(4,4)=%u,%u,%u,%u"
            " px(7,7)=%u,%u,%u,%u\n",
            drawn[0], drawn[1], drawn[2], drawn[3],
            drawn[(4 * WIDTH + 4) * 4 + 0], drawn[(4 * WIDTH + 4) * 4 + 1],
            drawn[(4 * WIDTH + 4) * 4 + 2], drawn[(4 * WIDTH + 4) * 4 + 3],
            drawn[(7 * WIDTH + 7) * 4 + 0], drawn[(7 * WIDTH + 7) * 4 + 1],
            drawn[(7 * WIDTH + 7) * 4 + 2], drawn[(7 * WIDTH + 7) * 4 + 3]);
    fprintf(stderr, "INFO fb0 after_draw err=0x%04x row0=%u,%u,%u,%u "
            "texture_row0=%u,%u,%u,%u texture_rowlast=%u,%u,%u,%u\n",
            (unsigned)glGetError(), drawn[0], drawn[4], drawn[8], drawn[12],
            base[0], base[1], base[2], base[3],
            base[(HEIGHT - 1) * WIDTH], base[(HEIGHT - 1) * WIDTH + 1],
            base[(HEIGHT - 1) * WIDTH + 2], base[(HEIGHT - 1) * WIDTH + 3]);

    /* Does a draw into fb0 work at all, independent of texturing? Same
     * geometry, a fragment shader that writes constant green and samples
     * nothing. */
    {
        GLuint cfs = compile(GL_FRAGMENT_SHADER, constant_fragment_source);
        GLuint cprog = glCreateProgram();
        glAttachShader(cprog, vertex);
        glAttachShader(cprog, cfs);
        glLinkProgram(cprog);
        glUseProgram(cprog);
        glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDrawArrays(GL_TRIANGLES, 0, 6);
        unsigned char flat[4] = {0xAB, 0xAB, 0xAB, 0xAB};
        glReadPixels(WIDTH / 2, HEIGHT / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                     flat);
        fprintf(stderr, "INFO fb0 constant_draw err=0x%04x reads=%u,%u,%u,%u "
                "expect=0,255,0,255\n", (unsigned)glGetError(),
                flat[0], flat[1], flat[2], flat[3]);
        glUseProgram(program);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    /* Explicit teardown. Leaving it to process exit trapped the runtime with
     * "the GL device did not answer in thirty seconds" *after* every case had
     * already passed, so the probe reported a timeout while its results were
     * all correct. */
    fprintf(stderr, "STEP teardown begin\n");
    glBindTexture(GL_TEXTURE_2D, 0);
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glUseProgram(0);
    glDeleteTextures(1, &texture);
    glDeleteTextures(1, &colour);
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteBuffers(1, &position_buffer);
    glDeleteVertexArrays(1, &vao);
    glDeleteProgram(program);
    glFinish();
    fprintf(stderr, "STEP teardown done\n");
    return cases == passed ? 0 : 1;
}
