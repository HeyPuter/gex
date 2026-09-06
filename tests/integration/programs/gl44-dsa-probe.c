/* Direct state access: GL 4.5 core, and the shape a modern engine uses.
 *
 * The point of this probe is not that DSA is exotic but that it is *default*.
 * An engine does not ask whether ARB_direct_state_access is present, it asks
 * what version the context is, and this driver says 4.6 -- so it takes the
 * DSA path. The guest marshaller carried none of GL 4.5 until now, so every
 * one of these entry points resolved to NULL and the application stopped
 * before drawing anything. None of that is reachable from a native run: the
 * native path goes through the system libGL and never through the guest.
 *
 * Deliberately uses no glBind* for the objects it configures. glCreateTextures
 * is what makes that legal -- a name from glGenTextures is uninitialised, and
 * glTextureStorage2D on one is an error until something binds it, which is the
 * bind DSA exists to avoid.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;
static int cases = 0;

static void report(const char* name, int ok, const char* detail) {
    ++cases;
    if (!ok) ++failures;
    printf("CASE %s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " -- " : "", detail ? detail : "");
}

#define NEED(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "dsa failed: %s\n", message); return __LINE__; } \
} while (0)

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    NEED(eglInitialize(display, NULL, NULL), "eglInitialize");
    NEED(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 24, EGL_STENCIL_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    NEED(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
         "eglChooseConfig");
    NEED(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config,
                                                 surface_attributes);
    NEED(context != EGL_NO_CONTEXT, "eglCreateContext");
    NEED(surface != EGL_NO_SURFACE, "eglCreatePbufferSurface");
    NEED(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    /* The version the engine would branch on. */
    GLint major = 0, minor = 0;
    glGetIntegerv(GL_MAJOR_VERSION, &major);
    glGetIntegerv(GL_MINOR_VERSION, &minor);
    {
        char detail[64];
        snprintf(detail, sizeof detail, "context reports %d.%d", major, minor);
        report("version_at_least_4_5", major > 4 || (major == 4 && minor >= 5),
               detail);
    }

    /* Objects of another kind first, deliberately. The guest names every
     * object from one counter while the host uses GL's per-kind numbering, so
     * without these the two happen to agree and any name a query returns looks
     * correct by coincidence. */
    GLuint spacers[3] = {0, 0, 0};
    glCreateBuffers(3, spacers);

    /* A texture built entirely without binding it. */
    GLuint texture = 0;
    glCreateTextures(GL_TEXTURE_2D, 1, &texture);
    report("create_textures", texture != 0 && glGetError() == GL_NO_ERROR, "");

    glTextureStorage2D(texture, 1, GL_RGBA8, 4, 4);
    glTextureParameteri(texture, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTextureParameteri(texture, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    report("texture_storage_and_parameters", glGetError() == GL_NO_ERROR, "");

    /* Storage really was allocated: ask the texture, not the binding. */
    GLint width = 0, height = 0;
    glGetTextureLevelParameteriv(texture, 0, GL_TEXTURE_WIDTH, &width);
    glGetTextureLevelParameteriv(texture, 0, GL_TEXTURE_HEIGHT, &height);
    {
        char detail[64];
        snprintf(detail, sizeof detail, "level 0 is %dx%d", width, height);
        report("texture_level_size", width == 4 && height == 4, detail);
    }

    /* A framebuffer assembled by name, and asked its own status. */
    GLuint framebuffer = 0;
    glCreateFramebuffers(1, &framebuffer);
    glNamedFramebufferTexture(framebuffer, GL_COLOR_ATTACHMENT0, texture, 0);
    report("named_framebuffer_texture",
           framebuffer != 0 && glGetError() == GL_NO_ERROR, "");

    /* Ask the framebuffer, by name, what it is attached to. */
    GLint attached = 0;
    glGetNamedFramebufferAttachmentParameteriv(
        framebuffer, GL_COLOR_ATTACHMENT0,
        GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME, &attached);
    report("named_framebuffer_attachment_query",
           attached == (GLint)texture && glGetError() == GL_NO_ERROR, "");

    /* Clear and read back. glClearNamedFramebufferfv is not carried by the
     * transport yet -- it is one of the pointer-bearing DSA calls still to do
     * -- so the clear goes through the binding while everything above it did
     * not. glReadPixels has no DSA form at all, so the read must bind anyway. */
    const GLfloat clear[4] = {0.25f, 0.5f, 0.75f, 1.0f};
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glClearBufferfv(GL_COLOR, 0, clear);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, framebuffer);
    unsigned char pixel[4] = {0, 0, 0, 0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    {
        char detail[96];
        const int ok = abs((int)pixel[0] - 64) <= 2 && abs((int)pixel[1] - 128) <= 2 &&
                       abs((int)pixel[2] - 191) <= 2 && pixel[3] == 255;
        snprintf(detail, sizeof detail, "read %u,%u,%u,%u", pixel[0], pixel[1],
                 pixel[2], pixel[3]);
        report("clear_named_framebuffer", ok, detail);
    }

    /* A vertex array configured by name. */
    GLuint vao = 0, buffer = 0;
    glCreateVertexArrays(1, &vao);
    glCreateBuffers(1, &buffer);
    glVertexArrayAttribFormat(vao, 0, 3, GL_FLOAT, GL_FALSE, 0);
    glVertexArrayAttribBinding(vao, 0, 0);
    glEnableVertexArrayAttrib(vao, 0);
    report("vertex_array_by_name",
           vao != 0 && buffer != 0 && glGetError() == GL_NO_ERROR, "");

    glBindTextureUnit(0, texture);
    report("bind_texture_unit", glGetError() == GL_NO_ERROR, "");

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return failures ? 1 : 0;
}
