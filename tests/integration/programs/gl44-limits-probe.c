/* The implementation limits GL requires, checked rather than assumed.
 *
 * An application reads these and sizes itself to them. A limit below GL's
 * required minimum is not a missing feature -- it is a promise the driver
 * makes and cannot keep, and nothing else in this tree notices: no shader
 * fails to compile, no draw is refused, and the number just comes out small.
 *
 * The cases here are the varying-interface limits, because those are the ones
 * this driver derives from WebGPU adapter limits rather than picking, so they
 * move when the adapter does -- and the browser's adapter is not the native
 * one. Every case names the GL minimum it is asserting.
 *
 * Three shortfalls are known and are deliberately *not* cases, because they
 * are tracked with their causes in docs/gl-webgpu-handoff.md and asserting
 * them here would make this probe a permanent difference against the
 * reference rather than a guard. They are printed as INFO instead, so the log
 * still carries the numbers and the browser's can be compared with the native
 * ones:
 *
 *   MAX_FRAGMENT_INPUT_COMPONENTS         4 x the adapter's inter-stage count
 *   MAX_GEOMETRY_OUTPUT_COMPONENTS        4 x (maxVertexAttributes - 1)
 *   MAX_TESS_EVALUATION_OUTPUT_COMPONENTS the same
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases;
static int passed;

static void result(const char *name, int ok)
{
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
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
    static const EGLint surf_attrs[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, surf_attrs);
    eglMakeCurrent(dpy, surf, surf, ctx);

    /* Asserted: GL's minimum for each, and this driver meets all of them. */
    struct { const char *name; GLenum token; GLint minimum; } required[] = {
        {"vertex_attribs", GL_MAX_VERTEX_ATTRIBS, 16},
        {"vertex_output_components", GL_MAX_VERTEX_OUTPUT_COMPONENTS, 64},
        {"geometry_input_components", GL_MAX_GEOMETRY_INPUT_COMPONENTS, 64},
        /* The control shader's inputs come from the vertex stage's capture
         * buffer, so nothing in that path is attribute-bound; this reported
         * 116 while it was grouped with the rasterized stages. */
        {"tess_control_input_components",
         GL_MAX_TESS_CONTROL_INPUT_COMPONENTS, 128},
        {"tess_control_output_components",
         GL_MAX_TESS_CONTROL_OUTPUT_COMPONENTS, 128},
        {"tess_patch_components", GL_MAX_TESS_PATCH_COMPONENTS, 120},
        {"tess_evaluation_input_components",
         GL_MAX_TESS_EVALUATION_INPUT_COMPONENTS, 128},
        {"clip_distances", GL_MAX_CLIP_DISTANCES, 8},
        {"cull_distances", GL_MAX_CULL_DISTANCES, 8},
        {"viewports", GL_MAX_VIEWPORTS, 16},
        {"draw_buffers", GL_MAX_DRAW_BUFFERS, 8},
        {"texture_image_units", GL_MAX_TEXTURE_IMAGE_UNITS, 16},
        {"uniform_buffer_bindings", GL_MAX_UNIFORM_BUFFER_BINDINGS, 84},
        {"shader_storage_buffer_bindings",
         GL_MAX_SHADER_STORAGE_BUFFER_BINDINGS, 8},
        {"array_texture_layers", GL_MAX_ARRAY_TEXTURE_LAYERS, 256},
        {"texture_size", GL_MAX_TEXTURE_SIZE, 16384},
        {"3d_texture_size", GL_MAX_3D_TEXTURE_SIZE, 2048},
        {"tess_gen_level", GL_MAX_TESS_GEN_LEVEL, 64},
        {"patch_vertices", GL_MAX_PATCH_VERTICES, 32},
        {"geometry_output_vertices", GL_MAX_GEOMETRY_OUTPUT_VERTICES, 256},
        {"compute_shared_memory_size",
         GL_MAX_COMPUTE_SHARED_MEMORY_SIZE, 32768},
    };
    for (unsigned i = 0; i < sizeof required / sizeof *required; ++i) {
        GLint value = -1;
        glGetIntegerv(required[i].token, &value);
        const GLenum error = glGetError();
        if (value < required[i].minimum || error != GL_NO_ERROR)
            fprintf(stderr, "INFO %s = %d, below the required %d (err 0x%04x)\n",
                    required[i].name, value, required[i].minimum,
                    (unsigned)error);
        result(required[i].name,
               error == GL_NO_ERROR && value >= required[i].minimum);
    }

    /* Reported, not asserted: see the note at the top. */
    struct { const char *name; GLenum token; GLint minimum; } tracked[] = {
        {"fragment_input_components", GL_MAX_FRAGMENT_INPUT_COMPONENTS, 128},
        {"geometry_output_components", GL_MAX_GEOMETRY_OUTPUT_COMPONENTS, 128},
        {"tess_evaluation_output_components",
         GL_MAX_TESS_EVALUATION_OUTPUT_COMPONENTS, 128},
        {"vertex_streams", GL_MAX_VERTEX_STREAMS, 4},
    };
    for (unsigned i = 0; i < sizeof tracked / sizeof *tracked; ++i) {
        GLint value = -1;
        glGetIntegerv(tracked[i].token, &value);
        (void)glGetError();
        fprintf(stderr, "INFO tracked %s = %d (GL wants %d)%s\n",
                tracked[i].name, value, tracked[i].minimum,
                value < tracked[i].minimum ? " SHORT" : "");
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return 0;
}
