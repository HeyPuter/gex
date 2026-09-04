/* Sampling a texture from the geometry and tessellation stages.
 *
 * GL lets every stage sample. This driver runs the geometry and both
 * tessellation stages as *compute* passes, and nothing bound their textures:
 * gex_wgpu_bind_sampler_states took the fragment, vertex and compute stages
 * only. The shaders were refused at creation, which fails the link.
 *
 * The cost was larger than it looks. Every program in
 * KHR-GL45.shading_language_420pack binds a sampler in every stage, so all
 * 282 of its cases failed -- including `utf8_characters_case_in_comment`,
 * which is about a comment and has no business depending on the driver.
 *
 *   `geometry`      a geometry shader reads a texel and passes it on
 *   `tess_control`  a tessellation control shader reads a texel and passes it
 *                   through a patch output
 *   `tess_eval`     a tessellation evaluation shader reads a texel directly
 *
 * Each case samples a 1x1 texture holding a known colour and paints the
 * whole viewport with it, so a stage that could not sample fails on the link
 * and a stage that sampled the wrong thing fails on the pixel.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>
#include <string.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

#define SIDE 32

static GLuint build(const char *vs, const char *tcs, const char *tes,
                    const char *gs, const char *fs, const char *label) {
    struct { GLenum stage; const char *src; } parts[] = {
        {GL_VERTEX_SHADER, vs}, {GL_TESS_CONTROL_SHADER, tcs},
        {GL_TESS_EVALUATION_SHADER, tes}, {GL_GEOMETRY_SHADER, gs},
        {GL_FRAGMENT_SHADER, fs},
    };
    GLuint prog = glCreateProgram();
    for (unsigned i = 0; i < 5; ++i) {
        if (!parts[i].src) continue;
        GLuint sh = glCreateShader(parts[i].stage);
        glShaderSource(sh, 1, &parts[i].src, NULL);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        if (!ok) {
            char log[900] = {0};
            glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO %s stage %u compile: %s\n", label, i, log);
            return 0;
        }
        glAttachShader(prog, sh);
    }
    glLinkProgram(prog);
    GLint ok = 0;
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[900] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO %s link: %s\n", label, log);
        return 0;
    }
    return prog;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count)
        || config_count < 1) return 2;
    /* A *core* profile. With no profile mask this gets a compatibility
     * context, and st/mesa then routes glDrawArrays(GL_PATCHES) through a
     * path that hands the driver an *indexed* draw -- which the tessellation
     * rewrite declines, so the case failed for a reason that had nothing to
     * do with sampling. Nothing here needs compatibility. */
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 1,
        EGL_CONTEXT_OPENGL_PROFILE_MASK,
        EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT,
                                          context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;
    fprintf(stderr, "INFO version=%s\n", glGetString(GL_VERSION));

    GLuint colour = 0, fbo = 0, vao = 0, tex = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, SIDE, SIDE, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
        return 2;
    glViewport(0, 0, SIDE, SIDE);
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);

    /* One texel, a colour no clear value here uses. */
    const unsigned char texel[4] = {0, 200, 100, 255};
    glGenTextures(1, &tex);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, texel);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

    static const char *fs_pass =
        "#version 410 core\n"
        "in vec4 tint;\n"
        "out vec4 colour;\n"
        "void main() { colour = tint; }\n";

    /* 1. The geometry stage samples. A full-viewport triangle is emitted from
     * a single point so nothing depends on vertex data. */
    {
        static const char *vs =
            "#version 410 core\n"
            "void main() { gl_Position = vec4(0.0, 0.0, 0.0, 1.0); }\n";
        static const char *gs =
            "#version 410 core\n"
            "layout(points) in;\n"
            "layout(triangle_strip, max_vertices = 4) out;\n"
            "uniform sampler2D src;\n"
            "out vec4 tint;\n"
            "void main() {\n"
            "  vec4 c = texture(src, vec2(0.5));\n"
            "  tint = c; gl_Position = vec4(-1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
            "  tint = c; gl_Position = vec4( 1.0, -1.0, 0.0, 1.0); EmitVertex();\n"
            "  tint = c; gl_Position = vec4(-1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
            "  tint = c; gl_Position = vec4( 1.0,  1.0, 0.0, 1.0); EmitVertex();\n"
            "  EndPrimitive();\n"
            "}\n";
        GLuint prog = build(vs, NULL, NULL, gs, fs_pass, "geometry");
        if (!prog) {
            result("geometry", 0);
        } else {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "src"), 0);
            glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_POINTS, 0, 1);
            unsigned char px[4] = {0};
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO geometry got=%u,%u,%u want=0,200,100 "
                    "err=0x%04x\n", px[0], px[1], px[2], (unsigned)error);
            result("geometry", error == GL_NO_ERROR && px[0] < 40 &&
                               px[1] > 160 && px[2] > 60 && px[2] < 140);
        }
    }

    /* 2 and 3. The tessellation stages sample.
     *
     * A conventional three-vertex triangle patch fed from a real vertex
     * buffer. An earlier version of this used a one-vertex patch with no
     * vertex buffer bound and st/mesa turned it into an *indexed* draw, which
     * the driver's tessellation path declines -- so the case failed for a
     * reason that had nothing to do with sampling. Keep the shape ordinary:
     * the point here is which stage reads the texture, not how odd a patch
     * can be.
     */
    GLuint vbuf = 0;
    {
        const float verts[6] = {-1.0f, -1.0f, 3.0f, -1.0f, -1.0f, 3.0f};
        glGenBuffers(1, &vbuf);
        glBindBuffer(GL_ARRAY_BUFFER, vbuf);
        glBufferData(GL_ARRAY_BUFFER, sizeof(verts), verts, GL_STATIC_DRAW);
        glEnableVertexAttribArray(0);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);
    }
    for (unsigned which = 0; which < 2; ++which) {
        const int in_control = which == 0;
        static const char *vs =
            "#version 410 core\n"
            "layout(location = 0) in vec2 position;\n"
            "void main() { gl_Position = vec4(position, 0.0, 1.0); }\n";
        char tcs[1100], tes[1100];
        snprintf(tcs, sizeof(tcs),
                 "#version 410 core\n"
                 "layout(vertices = 3) out;\n"
                 "%s"
                 "patch out vec4 patch_tint;\n"
                 "void main() {\n"
                 "  gl_TessLevelOuter[0] = 1.0; gl_TessLevelOuter[1] = 1.0;\n"
                 "  gl_TessLevelOuter[2] = 1.0;\n"
                 "  gl_TessLevelInner[0] = 1.0;\n"
                 "  gl_out[gl_InvocationID].gl_Position =\n"
                 "      gl_in[gl_InvocationID].gl_Position;\n"
                 "  patch_tint = %s;\n"
                 "}\n",
                 in_control ? "uniform sampler2D src;\n" : "",
                 in_control ? "texture(src, vec2(0.5))"
                            : "vec4(0.0, 0.0, 0.0, 1.0)");
        snprintf(tes, sizeof(tes),
                 "#version 410 core\n"
                 "layout(triangles, equal_spacing) in;\n"
                 "%s"
                 "patch in vec4 patch_tint;\n"
                 "out vec4 tint;\n"
                 "void main() {\n"
                 "  gl_Position = gl_TessCoord.x * gl_in[0].gl_Position\n"
                 "              + gl_TessCoord.y * gl_in[1].gl_Position\n"
                 "              + gl_TessCoord.z * gl_in[2].gl_Position;\n"
                 "  tint = %s;\n"
                 "}\n",
                 in_control ? "" : "uniform sampler2D src;\n",
                 in_control ? "patch_tint" : "texture(src, vec2(0.5))");
        const char *label = in_control ? "tess_control" : "tess_eval";
        GLuint prog = build(vs, tcs, tes, NULL, fs_pass, label);
        if (!prog) {
            result(label, 0);
        } else {
            glUseProgram(prog);
            glUniform1i(glGetUniformLocation(prog, "src"), 0);
            glPatchParameteri(GL_PATCH_VERTICES, 3);
            glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_PATCHES, 0, 3);
            unsigned char px[4] = {0};
            glReadPixels(SIDE / 2, SIDE / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE,
                         px);
            const GLenum error = glGetError();
            fprintf(stderr, "INFO %s got=%u,%u,%u want=0,200,100 err=0x%04x\n",
                    label, px[0], px[1], px[2], (unsigned)error);
            result(label, error == GL_NO_ERROR && px[0] < 40 &&
                          px[1] > 160 && px[2] > 60 && px[2] < 140);
        }
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
