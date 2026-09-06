/* KHR_debug: the calls an engine makes during start-up before it draws.
 *
 * The point is not the diagnostics -- GL lets an implementation emit no
 * messages of its own, and this one does. The point is that the entry points
 * exist and behave: an engine that sees GL 4.3 installs a debug callback, and
 * glDebugMessageCallback resolving to NULL is a crash before the first frame
 * rather than a missing log line. Everything here lives on the guest side of
 * the ring, because the callback is a pointer into this process and nothing on
 * the host could ever call it.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

static int cases = 0, failures = 0;
static void report(const char* name, int ok, const char* detail) {
    ++cases;
    if (!ok) ++failures;
    printf("CASE %s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " -- " : "", detail ? detail : "");
}
#define NEED(c, m) do { if (!(c)) { fprintf(stderr, "debug failed: %s\n", m); return __LINE__; } } while (0)

static int seen_count = 0;
static char seen_text[256];
static unsigned seen_severity = 0;
static void APIENTRY on_message(GLenum source, GLenum type, GLuint id,
                                GLenum severity, GLsizei length,
                                const GLchar* message, const void* user) {
    (void)source; (void)type; (void)id; (void)length;
    ++seen_count;
    seen_severity = severity;
    (void)user;
    if (message) {
        strncpy(seen_text, message, sizeof seen_text - 1);
        seen_text[sizeof seen_text - 1] = '\0';
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    NEED(eglInitialize(display, NULL, NULL), "eglInitialize");
    NEED(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = NULL; EGLint config_count = 0;
    NEED(eglChooseConfig(display, config_attributes, &config, 1, &config_count),
         "eglChooseConfig");
    NEED(config_count >= 1 && config != NULL, "no pbuffer config");
    const EGLint surface_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    NEED(context != EGL_NO_CONTEXT && surface != EGL_NO_SURFACE, "context/surface");
    NEED(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    /* Installing the callback is the case that matters most: this is the call
     * that was absent, and an engine makes it before anything else. */
    glEnable(GL_DEBUG_OUTPUT);
    glDebugMessageCallback(on_message, NULL);
    report("install_callback", glGetError() == GL_NO_ERROR, "");

    /* GL hands the installed pointer back through glGetPointerv. */
    void* installed = NULL;
    glGetPointerv(GL_DEBUG_CALLBACK_FUNCTION, &installed);
    report("callback_readback", installed == (void*)on_message, "");

    /* A message the application inserts must reach the callback. */
    glDebugMessageControl(GL_DONT_CARE, GL_DONT_CARE, GL_DONT_CARE, 0, NULL,
                          GL_TRUE);
    static const char note[] = "gex debug probe";
    glDebugMessageInsert(GL_DEBUG_SOURCE_APPLICATION, GL_DEBUG_TYPE_OTHER, 7,
                         GL_DEBUG_SEVERITY_NOTIFICATION,
                         (GLsizei)(sizeof note - 1), note);
    {
        char detail[96];
        snprintf(detail, sizeof detail, "seen=%d text=\"%.40s\"", seen_count,
                 seen_text);
        report("insert_reaches_callback",
               seen_count == 1 && strcmp(seen_text, note) == 0, detail);
    }

    /* A group push is a message too, and the pop must balance it. */
    seen_count = 0;
    glPushDebugGroup(GL_DEBUG_SOURCE_APPLICATION, 1, -1, "frame");
    glPopDebugGroup();
    {
        char detail[64];
        snprintf(detail, sizeof detail, "%d messages from push+pop", seen_count);
        report("debug_group_push_pop", seen_count == 2, detail);
    }

    /* A label set on an object must read back. */
    GLuint texture = 0;
    glGenTextures(1, &texture);
    /* Bound first, deliberately. A name from glGenTextures is not an object
     * until something binds it, so glObjectLabel on it is GL_INVALID_VALUE and
     * the label never lands -- the reference caught this probe doing exactly
     * that. It is the same rule that makes glCreateTextures the entry point
     * direct state access is built on. */
    glBindTexture(GL_TEXTURE_2D, texture);
    glObjectLabel(GL_TEXTURE, texture, -1, "probe texture");
    char label[64] = {0};
    GLsizei label_length = 0;
    glGetObjectLabel(GL_TEXTURE, texture, (GLsizei)sizeof label, &label_length,
                     label);
    {
        char detail[96];
        snprintf(detail, sizeof detail, "len=%d label=\"%s\"", (int)label_length,
                 label);
        report("object_label_roundtrip",
               strcmp(label, "probe texture") == 0 && label_length == 13, detail);
    }

    /* With no callback installed the messages queue instead, and
     * glGetDebugMessageLog drains them. */
    glDebugMessageCallback(NULL, NULL);
    glDebugMessageInsert(GL_DEBUG_SOURCE_APPLICATION, GL_DEBUG_TYPE_OTHER, 9,
                         GL_DEBUG_SEVERITY_LOW, -1, "queued");
    {
        GLenum src = 0, type = 0, sev = 0; GLuint id = 0; GLsizei len = 0;
        char text[64] = {0};
        const GLuint got = glGetDebugMessageLog(1, (GLsizei)sizeof text, &src,
                                                &type, &id, &sev, &len, text);
        char detail[96];
        snprintf(detail, sizeof detail, "got=%u id=%u text=\"%s\"", got, id, text);
        report("message_log_drains",
               got == 1 && id == 9 && strcmp(text, "queued") == 0, detail);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return failures ? 1 : 0;
}
