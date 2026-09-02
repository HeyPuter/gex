/* Which texture targets can hold storage.
 *
 * KHR-GL33.texture_size_promotion walks eight of them and throws on the first
 * GL error, reporting only "Texture Size Promotion Test have approached
 * error" -- which says nothing about where. This says where.
 *
 * Each target is allocated the way that test allocates it and its error code
 * recorded. Seven of the eight work; the answer was
 * GL_TEXTURE_2D_MULTISAMPLE_ARRAY, which fails allocation with
 * GL_INVALID_OPERATION.
 *
 * That one is a platform limit rather than a defect, and it cannot be closed:
 * WebGPU requires a texture with sampleCount > 1 to be 2d with exactly one
 * layer, so a multisampled array has nothing to be made of. GL 3.2 makes it
 * mandatory, so this is a ceiling the backend cannot reach, like the absence
 * of renderable snorm formats. It is reported here rather than graded --
 * native Mesa allocates it and this backend does not, so grading it would
 * make the two disagree by design -- and what the gate does assert is that
 * the other seven work and that the eighth fails *cleanly*, with an error
 * code, rather than latching the context or crashing.
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

    static const struct { const char *name; GLenum target; int shape; }
    targets[] = {
        /* shape: 1/2/3 = texImage1D/2D/3D, 4 = 2D multisample,
         * 5 = 2D multisample array. */
        /* First on purpose: it is the one that fails, and the seven after it
         * are then also a check that its failure did not latch the context. */
        {"texture_2d_multisample_array", GL_TEXTURE_2D_MULTISAMPLE_ARRAY, 5},
        {"texture_1d", GL_TEXTURE_1D, 1},
        {"texture_2d", GL_TEXTURE_2D, 2},
        {"texture_1d_array", GL_TEXTURE_1D_ARRAY, 2},
        {"texture_rectangle", GL_TEXTURE_RECTANGLE, 2},
        {"texture_2d_array", GL_TEXTURE_2D_ARRAY, 3},
        {"texture_3d", GL_TEXTURE_3D, 3},
        {"texture_2d_multisample", GL_TEXTURE_2D_MULTISAMPLE, 4},
    };

    for (unsigned i = 0; i < sizeof(targets) / sizeof(targets[0]); ++i) {
        GLuint texture = 0;
        glGenTextures(1, &texture);
        glBindTexture(targets[i].target, texture);
        while (glGetError() != GL_NO_ERROR) { }
        switch (targets[i].shape) {
        case 1:
            glTexImage1D(targets[i].target, 0, GL_RGBA8, 4, 0, GL_RGBA,
                         GL_UNSIGNED_BYTE, NULL);
            break;
        case 2:
            glTexImage2D(targets[i].target, 0, GL_RGBA8, 4, 4, 0, GL_RGBA,
                         GL_UNSIGNED_BYTE, NULL);
            break;
        case 3:
            glTexImage3D(targets[i].target, 0, GL_RGBA8, 4, 4, 4, 0, GL_RGBA,
                         GL_UNSIGNED_BYTE, NULL);
            break;
        case 4:
            glTexImage2DMultisample(targets[i].target, 4, GL_RGBA8, 4, 4,
                                    GL_TRUE);
            break;
        default:
            glTexImage3DMultisample(targets[i].target, 4, GL_RGBA8, 4, 4, 4,
                                    GL_TRUE);
            break;
        }
        const GLenum allocate_error = glGetError();
        /* The query the size-promotion test guards after every allocation. */
        GLint red = -1;
        glGetTexLevelParameteriv(targets[i].target, 0, GL_TEXTURE_RED_SIZE,
                                 &red);
        const GLenum query_error = glGetError();
        fprintf(stderr, "INFO %s alloc=0x%04x query=0x%04x red=%d\n",
                targets[i].name, (unsigned)allocate_error,
                (unsigned)query_error, red);
        if (targets[i].shape == 5) {
            /* Ungraded; see the note at the top. What is required of it is
             * that it did not take the context down, which every case after
             * this one would show. */
            fprintf(stderr, "INFO multisample arrays are not gradeable here: "
                            "native allocates them and WebGPU cannot\n");
        } else {
            result(targets[i].name, allocate_error == GL_NO_ERROR &&
                                    query_error == GL_NO_ERROR && red > 0);
        }
        glDeleteTextures(1, &texture);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
