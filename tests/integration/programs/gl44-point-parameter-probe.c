#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>

static int cases;
static int passed;

static void result(const char *name, int ok) {
    ++cases;
    if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

static int close_float(float a, float b) {
    return fabsf(a - b) < 0.0001f;
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
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) ||
        config_count < 1) return 2;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 4, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 2;

    GLfloat fade = 0.0f;
    GLint origin = 0;
    glGetFloatv(GL_POINT_FADE_THRESHOLD_SIZE, &fade);
    glGetIntegerv(GL_POINT_SPRITE_COORD_ORIGIN, &origin);
    result("defaults", close_float(fade, 1.0f) && origin == GL_UPPER_LEFT &&
                       glGetError() == GL_NO_ERROR);

    glPointParameterf(GL_POINT_FADE_THRESHOLD_SIZE, 2.75f);
    glGetFloatv(GL_POINT_FADE_THRESHOLD_SIZE, &fade);
    GLint fade_integer = -1;
    glGetIntegerv(GL_POINT_FADE_THRESHOLD_SIZE, &fade_integer);
    printf("INFO scalar_fade=%g integer_query=%d\n", fade, fade_integer);
    result("scalar_float", close_float(fade, 2.75f) && glGetError() == GL_NO_ERROR);

    struct { unsigned before; GLfloat value; unsigned after; } float_input = {
        0x13579bdfu, 4.25f, 0x2468ace0u
    };
    glPointParameterfv(GL_POINT_FADE_THRESHOLD_SIZE, &float_input.value);
    glGetFloatv(GL_POINT_FADE_THRESHOLD_SIZE, &fade);
    result("vector_float_canary", close_float(fade, 4.25f) &&
           float_input.before == 0x13579bdfu && float_input.after == 0x2468ace0u &&
           glGetError() == GL_NO_ERROR);

    struct { unsigned before; GLint value; unsigned after; } int_input = {
        0x89abcdefu, 7, 0xfedcba98u
    };
    glPointParameteriv(GL_POINT_FADE_THRESHOLD_SIZE, &int_input.value);
    glGetFloatv(GL_POINT_FADE_THRESHOLD_SIZE, &fade);
    result("vector_int_canary", close_float(fade, 7.0f) &&
           int_input.before == 0x89abcdefu && int_input.after == 0xfedcba98u &&
           glGetError() == GL_NO_ERROR);

    GLint lower = GL_LOWER_LEFT;
    glPointParameteriv(GL_POINT_SPRITE_COORD_ORIGIN, &lower);
    glGetIntegerv(GL_POINT_SPRITE_COORD_ORIGIN, &origin);
    result("origin_lower", origin == GL_LOWER_LEFT && glGetError() == GL_NO_ERROR);
    GLfloat upper = (GLfloat)GL_UPPER_LEFT;
    glPointParameterfv(GL_POINT_SPRITE_COORD_ORIGIN, &upper);
    glGetIntegerv(GL_POINT_SPRITE_COORD_ORIGIN, &origin);
    result("origin_upper", origin == GL_UPPER_LEFT && glGetError() == GL_NO_ERROR);

    glPointParameterf(GL_POINT_FADE_THRESHOLD_SIZE, -0.5f);
    result("negative_fade_error", glGetError() == GL_INVALID_VALUE);
    glGetFloatv(GL_POINT_FADE_THRESHOLD_SIZE, &fade);
    result("negative_fade_preserves", close_float(fade, 7.0f) && glGetError() == GL_NO_ERROR);
    glPointParameteri(GL_POINT_SPRITE_COORD_ORIGIN, GL_TRIANGLES);
    GLenum origin_error = glGetError();
    printf("INFO invalid_origin_error=0x%x\n", origin_error);
    result("invalid_origin_error", origin_error == GL_INVALID_VALUE);
    glGetIntegerv(GL_POINT_SPRITE_COORD_ORIGIN, &origin);
    result("invalid_origin_preserves", origin == GL_UPPER_LEFT && glGetError() == GL_NO_ERROR);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, cases - passed);
    return passed == cases ? 0 : 1;
}
