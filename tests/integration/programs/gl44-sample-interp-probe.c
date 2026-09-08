/* Per-sample interpolation: does a varying actually vary across samples?
 *
 * Per-sample *shading* already works here -- gl_SampleID reaches WGSL as
 * @builtin(sample_index), which gl44-sample-shading-probe pins. Per-sample
 * *interpolation* is a separate mechanism and was given up on purpose: a
 * fragment input's SpvDecorationSample survives into
 * `@interpolate(perspective, sample)`, but a vertex *output* carrying the
 * same decoration is not valid SPIR-V and Tint drops it, so the two stages
 * disagreed on the interpolation sampling and WebGPU rejected the pipeline --
 * losing every draw batched with it. The driver's answer was to strip the
 * qualifier from the fragment input and interpolate at the pixel centre,
 * which is silent and wrong rather than loud and wrong.
 *
 * That makes three GL constructs unserved, and all three are the *same*
 * mechanism:
 *
 *   `sample in vec4 v`                             the qualifier itself
 *   interpolateAtSample(v, gl_SampleID)            the current sample
 *   interpolateAtOffset(v, gl_SamplePosition-0.5)  the current sample, again
 *
 * The last is worth spelling out: gl_SamplePosition is the sample's position
 * in the pixel with the lower-left corner at 0, so subtracting 0.5 is its
 * offset from the centre -- which is what interpolateAtOffset measures from.
 * Interpolating at that offset *is* interpolating at the sample. Both
 * functions reduce exactly to a per-sample read, with no approximation.
 *
 * The measurement has to name numbers, not merely "it drew". A varying that
 * ramps across a 4-sample pixel reads four different values when the
 * interpolation is per-sample and four *equal* values when it silently falls
 * back to the centre, so equality across samples is the failure -- and it is
 * invisible to anything that only checks the resolved average.
 *
 *   sample_qualifier_varies      `sample in` gives four distinct values
 *   at_sample_varies             interpolateAtSample does too
 *   at_sample_matches_qualifier  ...and agrees with the qualifier per sample
 *   at_offset_varies             interpolateAtOffset does too
 *   at_offset_matches_qualifier  ...and agrees with the qualifier per sample
 *   at_centroid_constant         interpolateAtCentroid is one value per pixel,
 *                                the control that must *not* vary
 *
 * No GEX_PROBE_EXPECT is declared, on purpose. That override is for probes
 * where the *reference* is what fails; here llvmpipe passes all six and this
 * driver passes four, the two at_offset cases being the named gap. Declaring
 * 6/6 would make the browser gate demand something the driver does not yet do,
 * and declaring 4/6 would freeze a known gap as permanently green -- which is
 * exactly what tests/gl44-probe.sh warns against. So the gate compares against
 * the reference and this probe is a measurement until interpolateAtOffset
 * lands, at which point it should go green with no edit here.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>

#define SIDE 4
#define SAMPLES 4

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* The ramp is x only. A standard 4x pattern puts its four samples at four
 * distinct x offsets, so x alone separates them; using y as well would only
 * make a wrong answer harder to read. One pixel of a 4-wide target spans a
 * quarter of the 0..1 ramp, so 8-bit output resolves the samples ~16 levels
 * apart -- far above rounding, and far below the 64 levels a whole pixel
 * covers, which is what keeps this from passing on a centre-interpolated
 * neighbour value. */
static const char *vs_src =
    "#version 400 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "out vec4 v_base;\n"
    "sample out vec4 v_sample;\n"
    "void main() {\n"
    "  vec4 ramp = vec4(pos.x * 0.5 + 0.5, 0.0, 0.0, 1.0);\n"
    "  v_base = ramp;\n"
    "  v_sample = ramp;\n"
    "  gl_Position = vec4(pos, 0.0, 1.0);\n"
    "}\n";

/* One fragment shader per form, differing only in the expression. Each writes
 * the interpolated red into red; the resolve pass below reads it per sample. */
static const char *fs_for(const char *expr, int needs_sample_pos)
{
    static char buf[1024];
    snprintf(buf, sizeof(buf),
             "#version 400 core\n"
             "%s"
             "in vec4 v_base;\n"
             "sample in vec4 v_sample;\n"
             "layout(location = 0) out vec4 colour;\n"
             "void main() {\n"
             "  vec4 got = %s;\n"
             "  colour = vec4(got.x, 0.0, 0.0, 1.0);\n"
             "}\n",
             needs_sample_pos ? "#extension GL_ARB_sample_shading : require\n"
                              : "",
             expr);
    return buf;
}

static const char *resolve_vs =
    "#version 400 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

/* Four samples of one pixel, packed into one pixel's four channels. */
static const char *resolve_fs =
    "#version 400 core\n"
    "uniform sampler2DMS src;\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() {\n"
    "  ivec2 at = ivec2(gl_FragCoord.xy);\n"
    "  colour = vec4(texelFetch(src, at, 0).x, texelFetch(src, at, 1).x,\n"
    "                texelFetch(src, at, 2).x, texelFetch(src, at, 3).x);\n"
    "}\n";

static GLuint compile(GLenum stage, const char *source)
{
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1200] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile (%s): %s\n",
                stage == GL_VERTEX_SHADER ? "vs" : "fs", log);
        glDeleteShader(shader);
        return 0;
    }
    return shader;
}

static GLuint build(const char *vs, const char *fs)
{
    GLuint v = compile(GL_VERTEX_SHADER, vs);
    GLuint f = compile(GL_FRAGMENT_SHADER, fs);
    if (!v || !f) return 0;
    GLuint prog = glCreateProgram();
    glAttachShader(prog, v);
    glAttachShader(prog, f);
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1200] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        glDeleteProgram(prog);
        return 0;
    }
    return prog;
}

static GLuint ms_tex, ms_fbo, flat_tex, flat_fbo, quad;

/* Draws the quad through `prog` into the multisample target, then reads the
 * centre pixel's four samples back through the resolve pass. */
static int measure(GLuint prog, unsigned char out[4])
{
    glBindFramebuffer(GL_FRAMEBUFFER, ms_fbo);
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

    static GLuint resolve;
    if (!resolve) {
        resolve = build(resolve_vs, resolve_fs);
        if (!resolve) return 0;
    }
    glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
    glViewport(0, 0, SIDE, SIDE);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(resolve);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms_tex);
    glUniform1i(glGetUniformLocation(resolve, "src"), 0);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);
    return glGetError() == GL_NO_ERROR;
}

static int distinct4(const unsigned char v[4])
{
    for (int i = 0; i < 4; ++i)
        for (int j = i + 1; j < 4; ++j) {
            const int d = (int)v[i] - (int)v[j];
            if (d > -4 && d < 4) return 0;
        }
    return 1;
}

static int equal4(const unsigned char a[4], const unsigned char b[4])
{
    for (int i = 0; i < 4; ++i) {
        const int d = (int)a[i] - (int)b[i];
        if (d < -3 || d > 3) return 0;
    }
    return 1;
}

static void show(const char *what, const unsigned char v[4])
{
    fprintf(stderr, "INFO %-22s samples=%u,%u,%u,%u\n", what, v[0], v[1], v[2],
            v[3]);
}

int main(void)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(dpy, NULL, NULL)) {
        printf("FATAL eglInitialize\n");
        return 1;
    }
    static const EGLint cfg_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                                       EGL_RENDERABLE_TYPE,
                                       EGL_OPENGL_BIT, EGL_NONE};
    EGLConfig cfg;
    EGLint n = 0;
    eglChooseConfig(dpy, cfg_attrs, &cfg, 1, &n);
    eglBindAPI(EGL_OPENGL_API);
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) {
        printf("FATAL eglCreateContext\n");
        return 1;
    }
    static const EGLint surf_attrs[] = {EGL_WIDTH, SIDE, EGL_HEIGHT, SIDE,
                                        EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    glGenTextures(1, &ms_tex);
    glBindTexture(GL_TEXTURE_2D_MULTISAMPLE, ms_tex);
    glTexImage2DMultisample(GL_TEXTURE_2D_MULTISAMPLE, SAMPLES, GL_RGBA8, SIDE,
                            SIDE, GL_TRUE);
    glGenFramebuffers(1, &ms_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, ms_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D_MULTISAMPLE, ms_tex, 0);
    const GLenum ms_ok = glCheckFramebufferStatus(GL_FRAMEBUFFER);

    glGenTextures(1, &flat_tex);
    glBindTexture(GL_TEXTURE_2D, flat_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glGenFramebuffers(1, &flat_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, flat_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           flat_tex, 0);
    const GLenum flat_ok = glCheckFramebufferStatus(GL_FRAMEBUFFER);

    if (ms_ok != GL_FRAMEBUFFER_COMPLETE || flat_ok != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO fbo ms=0x%04x flat=0x%04x\n", (unsigned)ms_ok,
                (unsigned)flat_ok);
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }

    static const float verts[] = {-1, -1, -1, 1, 1, -1, 1, 1};
    glGenBuffers(1, &quad);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);
    glEnable(GL_MULTISAMPLE);

    GLuint p_qual = build(vs_src, fs_for("v_sample", 0));
    GLuint p_at_sample =
        build(vs_src, fs_for("interpolateAtSample(v_base, gl_SampleID)", 1));
    GLuint p_at_offset = build(
        vs_src,
        fs_for("interpolateAtOffset(v_base, gl_SamplePosition - 0.5)", 1));
    GLuint p_at_centroid =
        build(vs_src, fs_for("interpolateAtCentroid(v_base)", 0));

    unsigned char qual[4] = {0}, at_sample[4] = {0}, at_offset[4] = {0},
                  at_centroid[4] = {0};
    const int m_qual = p_qual && measure(p_qual, qual);
    const int m_at_sample = p_at_sample && measure(p_at_sample, at_sample);
    const int m_at_offset = p_at_offset && measure(p_at_offset, at_offset);
    const int m_at_centroid =
        p_at_centroid && measure(p_at_centroid, at_centroid);

    if (m_qual) show("sample qualifier", qual);
    if (m_at_sample) show("interpolateAtSample", at_sample);
    if (m_at_offset) show("interpolateAtOffset", at_offset);
    if (m_at_centroid) show("interpolateAtCentroid", at_centroid);

    result("sample_qualifier_varies", m_qual && distinct4(qual));
    result("at_sample_varies", m_at_sample && distinct4(at_sample));
    result("at_sample_matches_qualifier",
           m_qual && m_at_sample && distinct4(qual) &&
               equal4(qual, at_sample));
    result("at_offset_varies", m_at_offset && distinct4(at_offset));
    result("at_offset_matches_qualifier",
           m_qual && m_at_offset && distinct4(qual) &&
               equal4(qual, at_offset));
    /* The control: one value for the pixel, so the four reads agree. */
    result("at_centroid_constant",
           m_at_centroid && at_centroid[0] == at_centroid[1] &&
               at_centroid[1] == at_centroid[2] &&
               at_centroid[2] == at_centroid[3]);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
