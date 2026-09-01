/* Does an FBO composited onto framebuffer 0 keep its orientation?
 *
 * SuperTuxKart draws its 3D scene into a texture and then draws that texture
 * over framebuffer 0, while the HUD goes straight to framebuffer 0. On this
 * driver the scene came out vertically mirrored and the HUD did not, which is
 * what a disagreement between the two paths looks like: st signals the window
 * framebuffer with a negative viewport y scale and an FBO with a positive one,
 * and the draw path negates clip-space y for the positive case only.
 *
 * The check here is an invariant rather than a comparison against a golden
 * image, because framebuffer 0 cannot be read reliably on native Mesa under
 * EGL_PLATFORM=surfaceless -- it reports draw=BACK/read=FRONT and changes its
 * answer if GL_READ_BUFFER is merely queried first. Both readbacks here come
 * from framebuffer 0, so whatever that quirk is, it cancels:
 *
 * When this was written the guest's EGL ignored the requested pbuffer size
 * and framebuffer 0 came out 320x240 for a probe that asked for 4x4, which is
 * what turned a wrong gl_FragCoord.y into a saturated value rather than a
 * plausible one. The host honours the size now, so the framebuffer really is
 * SIZExSIZE -- the invariant below does not depend on which, but a reader
 * comparing this against the driver log should know.
 *
 *   A: draw a red bottom half straight onto framebuffer 0, read it back.
 *   B: draw the same red bottom half into a texture, then draw that texture
 *      over framebuffer 0 with matching coordinates, and read it back.
 *
 * GL requires A and B to be the same image. A mirrored composite makes B red
 * on top, so the two disagree in every row.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define SIZE 4

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* A quad covering the bottom half in GL's terms: clip y from -1 to 0. */
static const char *half_vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos.x, pos.y * 0.5 - 0.5, 0.0, 1.0); }\n";

static const char *red_fragment_source =
    "#version 330 core\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = vec4(1.0, 0.0, 0.0, 1.0); }\n";

/* The composite: a full-screen quad whose texture coordinate is its own
 * position, which is what an application compositing a render target does. */
static const char *blit_vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "out vec2 uv;\n"
    "void main() { uv = pos * 0.5 + 0.5; gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *blit_fragment_source =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "in vec2 uv;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = texture(tex, uv); }\n";

/* The other way a post-process pass finds its texel: from gl_FragCoord rather
 * than from an interpolated coordinate. This is the path the driver has to
 * correct by hand on framebuffer 0 -- st renders it with a negative viewport
 * y scale, so without help WebGPU's top-down y reaches the shader where GL
 * promises a bottom-up one -- and the correction bakes in the height. */
static const char *blit_fragcoord_fragment_source =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "uniform vec2 size;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = texture(tex, gl_FragCoord.xy / size); }\n";

/* gl_FragCoord.y itself, scaled into the colour, so the value can be read
 * rather than inferred from what a lookup addressed by it returned. */
static const char *fragcoord_probe_fragment_source =
    "#version 330 core\n"
    "uniform vec2 size;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    fragColour = vec4(gl_FragCoord.y / size.y, 0.0, 0.0, 1.0);\n"
    "}\n";

/* Both at once: the value of gl_FragCoord.y in a shader that also samples a
 * texture. The driver builds a separate fragment module whenever a sampler
 * needs one, and the gl_FragCoord correction has to survive that -- reading
 * the coordinate on its own does not prove it does. */
static const char *fragcoord_with_sampler_fragment_source =
    "#version 330 core\n"
    "uniform sampler2D tex;\n"
    "uniform vec2 size;\n"
    "out vec4 fragColour;\n"
    "void main() {\n"
    "    fragColour = vec4(gl_FragCoord.y / size.y,\n"
    "                      texture(tex, vec2(0.5, 0.25)).g, 0.0, 1.0);\n"
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
        fprintf(stderr, "INFO compile failed: %s\n", log);
        return 0;
    }
    return shader;
}

static GLuint link(const char *vertex_source, const char *fragment_source) {
    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) return 0;
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[512] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link failed: %s\n", log);
        return 0;
    }
    return program;
}

/* One character per row: R if the row is red, B if blue, ? otherwise. Rows
 * are printed bottom-up, the order GL reads them in. */
static void describe(const char *label, const unsigned char *pixels) {
    char rows[SIZE + 1];
    for (int y = 0; y < SIZE; ++y) {
        const unsigned char *p = pixels + (size_t)y * SIZE * 4;
        rows[y] = p[0] > 200 && p[2] < 60 ? 'R'
                : p[2] > 200 && p[0] < 60 ? 'B' : '?';
    }
    rows[SIZE] = 0;
    fprintf(stderr, "INFO %s bottom-to-top=%s\n", label, rows);
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
    const EGLint surface_attributes[] = {
        EGL_WIDTH, SIZE, EGL_HEIGHT, SIZE, EGL_NONE,
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

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

    GLuint half = link(half_vertex_source, red_fragment_source);
    GLuint blit = link(blit_vertex_source, blit_fragment_source);
    if (!half || !blit) return 2;

    /* A: straight onto framebuffer 0. */
    unsigned char direct[SIZE * SIZE * 4];
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glViewport(0, 0, SIZE, SIZE);
    glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(half);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, direct);
    describe("direct", direct);
    /* The pattern has to be asymmetric for the comparison to mean anything.
     * If the direct draw came out uniform, nothing below can distinguish an
     * upright composite from a mirrored one. */
    result("direct_draw_is_asymmetric",
           memcmp(direct, direct + (size_t)(SIZE - 1) * SIZE * 4,
                  SIZE * 4) != 0);

    /* B: the same pattern through a texture. */
    GLuint scene = 0, framebuffer = 0;
    glGenTextures(1, &scene);
    glBindTexture(GL_TEXTURE_2D, scene);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIZE, SIZE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 0);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           scene, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, SIZE, SIZE);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(half);
    glDrawArrays(GL_TRIANGLES, 0, 6);

    /* The render target on its own, as the application would read it back. */
    unsigned char offscreen[SIZE * SIZE * 4];
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, offscreen);
    describe("offscreen", offscreen);
    result("offscreen_matches_direct",
           memcmp(offscreen, direct, sizeof(direct)) == 0);

    unsigned char composited[SIZE * SIZE * 4];
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glViewport(0, 0, SIZE, SIZE);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(blit);
    glUniform1i(glGetUniformLocation(blit, "tex"), 0);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, scene);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, composited);
    describe("composited", composited);
    const GLenum error = glGetError();
    if (error != GL_NO_ERROR)
        fprintf(stderr, "INFO err=0x%04x\n", (unsigned)error);
    result("composited_matches_direct",
           error == GL_NO_ERROR &&
           memcmp(composited, direct, sizeof(direct)) == 0);

    /* C: the same render target moved with glBlitFramebuffer instead of
     * drawn. This is the other way an application gets a render target onto
     * the screen, and it is a different path in the driver -- the one that
     * has to flip rows itself, because WebGPU has no flipping copy. */
    unsigned char blitted[SIZE * SIZE * 4];
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, 0);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, framebuffer);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glBlitFramebuffer(0, 0, SIZE, SIZE, 0, 0, SIZE, SIZE,
                      GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, blitted);
    describe("blitted", blitted);
    const GLenum blit_error = glGetError();
    if (blit_error != GL_NO_ERROR)
        fprintf(stderr, "INFO blit err=0x%04x\n", (unsigned)blit_error);
    result("blitted_matches_direct",
           blit_error == GL_NO_ERROR &&
           memcmp(blitted, direct, sizeof(direct)) == 0);

    /* D: composited again, but addressed by gl_FragCoord. */
    GLuint blit_fragcoord = link(blit_vertex_source,
                                 blit_fragcoord_fragment_source);
    if (!blit_fragcoord) return 2;
    unsigned char by_fragcoord[SIZE * SIZE * 4];
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glViewport(0, 0, SIZE, SIZE);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(blit_fragcoord);
    glUniform1i(glGetUniformLocation(blit_fragcoord, "tex"), 0);
    glUniform2f(glGetUniformLocation(blit_fragcoord, "size"),
                (float)SIZE, (float)SIZE);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, scene);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, by_fragcoord);
    describe("by_fragcoord", by_fragcoord);
    result("fragcoord_composite_matches_direct",
           memcmp(by_fragcoord, direct, sizeof(direct)) == 0);

    /* E: what gl_FragCoord.y actually is on framebuffer 0. GL measures it
     * from the bottom of the framebuffer, so with a 4-row viewport at the
     * bottom the four rows must read 0.5, 1.5, 2.5, 3.5 -- whatever the
     * framebuffer's real height is, which in the browser is not the size the
     * pbuffer was asked for. */
    GLuint fragcoord_program = link(blit_vertex_source,
                                    fragcoord_probe_fragment_source);
    if (!fragcoord_program) return 2;
    unsigned char measured[SIZE * SIZE * 4];
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glViewport(0, 0, SIZE, SIZE);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(fragcoord_program);
    glUniform2f(glGetUniformLocation(fragcoord_program, "size"),
                (float)SIZE, (float)SIZE);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, measured);
    int fragcoord_ok = 1;
    fprintf(stderr, "INFO fragcoord_y bottom-to-top=");
    for (int y = 0; y < SIZE; ++y) {
        const unsigned char got = measured[(size_t)y * SIZE * 4];
        const int want = (int)((y + 0.5) / SIZE * 255.0 + 0.5);
        fprintf(stderr, "%u(want %d) ", got, want);
        if (got > want + 3 || got < want - 3) fragcoord_ok = 0;
    }
    fprintf(stderr, "\n");
    result("fragcoord_y_is_bottom_up", fragcoord_ok);

    /* F: the same reading, from a shader that also samples. */
    GLuint fragcoord_sampler = link(blit_vertex_source,
                                    fragcoord_with_sampler_fragment_source);
    if (!fragcoord_sampler) return 2;
    unsigned char with_sampler[SIZE * SIZE * 4];
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glViewport(0, 0, SIZE, SIZE);
    glClearColor(0.0f, 1.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(fragcoord_sampler);
    glUniform1i(glGetUniformLocation(fragcoord_sampler, "tex"), 0);
    glUniform2f(glGetUniformLocation(fragcoord_sampler, "size"),
                (float)SIZE, (float)SIZE);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, scene);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glReadPixels(0, 0, SIZE, SIZE, GL_RGBA, GL_UNSIGNED_BYTE, with_sampler);
    int sampler_ok = 1;
    fprintf(stderr, "INFO fragcoord_y_with_sampler bottom-to-top=");
    for (int y = 0; y < SIZE; ++y) {
        const unsigned char got = with_sampler[(size_t)y * SIZE * 4];
        const int want = (int)((y + 0.5) / SIZE * 255.0 + 0.5);
        fprintf(stderr, "%u(want %d) ", got, want);
        if (got > want + 3 || got < want - 3) sampler_ok = 0;
    }
    fprintf(stderr, "\n");
    result("fragcoord_y_survives_sampler_variant", sampler_ok);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
