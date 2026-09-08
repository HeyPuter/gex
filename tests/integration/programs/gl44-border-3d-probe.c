/* GL_CLAMP_TO_BORDER on the third axis of a 3D texture.
 *
 * WebGPU has no border address mode at all -- its three are repeat,
 * mirror-repeat and clamp-to-edge -- so the sampler is a clamp-to-edge one and
 * the border is put back in the shader: sample as usual, and where the
 * coordinate falls outside [0,1] on a bordered axis, substitute the border
 * colour. gex_wgpu_lower_nearest_border_instr does that, and until recently it
 * served only non-array 2D and returned false for every other shape without
 * logging anything -- so a 3D border silently became a clamp to edge.
 *
 * The s and t axes work. The r axis is the open question:
 * KHR-GL45.texture_border_clamp's four Texture3D* cases still fail, and the
 * CTS is precise about where:
 *
 *   Wrong value for layer (6) at point (x,y) = (128,128), component (0)
 *   Expected value [255]  Result value [0]
 *
 * Layer 6 of a depth-6 texture is outside in r, so it must read the border --
 * and it is the *only* layer that can reveal the border colour at all, which
 * is why the other seven rendered images match the reference pixel for pixel.
 * That makes the whole question one sample, which is what this isolates.
 *
 * The texture is filled with a value that is neither the border nor zero, so
 * all three outcomes are distinguishable: the texture means the border was
 * not applied, the border means it was, and zero means neither -- a dropped
 * draw or a sample that read nothing, which is what the CTS actually sees.
 *
 *   inside_reads_texture     r within [0,1] gives the texel, not the border
 *   r_above_reads_border     r > 1 gives the border
 *   r_below_reads_border     r < 0 gives the border
 *   s_outside_reads_border   the axis that already worked, as a control
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

#define SIDE 4
#define DEPTH 6
#define TEXEL 64

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

/* The coordinate comes from a uniform rather than a varying: the question is
 * about one exact coordinate, and interpolating it across a quad would put
 * some fragments on either side of the boundary and make the read-back depend
 * on which pixel is sampled. */
static const char *fs_src =
    "#version 330 core\n"
    "uniform sampler3D tex;\n"
    "uniform vec3 at;\n"
    "layout(location = 0) out vec4 colour;\n"
    "void main() { colour = texture(tex, at); }\n";

static GLuint compile(GLenum stage, const char *source)
{
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        return 0;
    }
    return shader;
}

static GLuint prog, quad, fbo;

static int sample_at(float s, float t, float r, unsigned char out[4])
{
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glViewport(0, 0, SIDE, SIDE);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(prog);
    glUniform3f(glGetUniformLocation(prog, "at"), s, t, r);
    glUniform1i(glGetUniformLocation(prog, "tex"), 0);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, out);
    return glGetError() == GL_NO_ERROR;
}

static int is_border(const unsigned char v[4])
{
    return v[0] > 250 && v[1] > 250 && v[2] > 250;
}

static int is_texel(const unsigned char v[4])
{
    const int d = (int)v[0] - TEXEL;
    return d > -4 && d < 4;
}

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

    /* A uniform mid-grey fill, so any texel that is read gives the same
     * answer and the test does not depend on which one clamp-to-edge picks. */
    static unsigned char texels[SIDE * SIDE * DEPTH * 4];
    for (unsigned i = 0; i < sizeof(texels); i += 4) {
        texels[i + 0] = TEXEL; texels[i + 1] = TEXEL;
        texels[i + 2] = TEXEL; texels[i + 3] = 255;
    }
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_3D, tex);
    glTexImage3D(GL_TEXTURE_3D, 0, GL_RGBA8, SIDE, SIDE, DEPTH, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texels);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_BORDER);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_BORDER);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_WRAP_R, GL_CLAMP_TO_BORDER);
    static const float white[4] = {1, 1, 1, 1};
    glTexParameterfv(GL_TEXTURE_3D, GL_TEXTURE_BORDER_COLOR, white);

    GLuint colour_tex = 0;
    glGenTextures(1, &colour_tex);
    glBindTexture(GL_TEXTURE_2D, colour_tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour_tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        printf("FATAL framebuffer incomplete\n");
        return 1;
    }

    static const float verts[] = {-1, -1, -1, 1, 1, -1, 1, 1};
    glGenBuffers(1, &quad);
    glBindBuffer(GL_ARRAY_BUFFER, quad);
    glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);

    GLuint vs = compile(GL_VERTEX_SHADER, vs_src);
    GLuint fs = compile(GL_FRAGMENT_SHADER, fs_src);
    if (!vs || !fs) { printf("FATAL compile\n"); return 1; }
    prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("FATAL link\n");
        return 1;
    }

    /* Layer 6 of a depth-6 texture in the CTS's own terms, which is exactly
     * 1.0: the test drives the third axis with `index / depth`, not with the
     * texel centre. That coordinate is the whole point of this probe. A
     * nearest sample picks texel floor(1.0 * 6) = 6, one past the end, so it
     * must read the border -- and a test written as `c > 1.0` calls it inside
     * and reads the clamped edge texel instead. */
    const float r_above = (float)DEPTH / (float)DEPTH;
    const float r_below = -0.5f / (float)DEPTH;
    const float r_in = (2.5f) / (float)DEPTH;

    unsigned char inside[4] = {0}, above[4] = {0}, below[4] = {0}, wide[4] = {0};
    const int m_inside = sample_at(0.5f, 0.5f, r_in, inside);
    const int m_above = sample_at(0.5f, 0.5f, r_above, above);
    const int m_below = sample_at(0.5f, 0.5f, r_below, below);
    const int m_wide = sample_at(1.5f, 0.5f, r_in, wide);

    fprintf(stderr, "INFO inside r=%.3f -> %u,%u,%u,%u\n", r_in,
            inside[0], inside[1], inside[2], inside[3]);
    fprintf(stderr, "INFO above  r=%.3f -> %u,%u,%u,%u\n", r_above,
            above[0], above[1], above[2], above[3]);
    fprintf(stderr, "INFO below  r=%.3f -> %u,%u,%u,%u\n", r_below,
            below[0], below[1], below[2], below[3]);
    fprintf(stderr, "INFO s=1.5 (control)  -> %u,%u,%u,%u\n",
            wide[0], wide[1], wide[2], wide[3]);

    result("inside_reads_texture", m_inside && is_texel(inside));
    result("r_above_reads_border", m_above && is_border(above));
    result("r_below_reads_border", m_below && is_border(below));
    result("s_outside_reads_border", m_wide && is_border(wide));

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
