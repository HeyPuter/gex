/* A binding query must answer in the application's own names.
 *
 * The guest allocates GL object names itself and the host keeps a map from
 * those to the names its own GL handed out. Every call that takes a name
 * translates one way; a query that *returns* a name has to translate the
 * other, and glGetIntegerv did not translate at all -- it returned whatever
 * the host's GL said.
 *
 * The two numberings usually agree, because both start at one and count up,
 * which is why this went unnoticed: it only shows when they have been pushed
 * apart. Deleting objects does that, since the host's GL is free to reuse a
 * name and the guest's counter never does.
 *
 * It matters outside the suite. Saving and restoring GL state -- read the
 * current binding, change it, put it back -- is what every toolkit that
 * shares a context does, ImGui and Qt among them, and a restore that binds a
 * name the application never generated is a silent unbind at best.
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* Push the two numberings apart: make several objects, throw them away, and
 * the next one the guest names high while the host may name it low. */
static void churn(void) {
    GLuint spare[8];
    glGenBuffers(8, spare);
    glDeleteBuffers(8, spare);
    GLuint tex[8];
    glGenTextures(8, tex);
    glDeleteTextures(8, tex);
}

int main(void) {
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    eglInitialize(d, NULL, NULL); eglBindAPI(EGL_OPENGL_API);
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
                          EGL_RED_SIZE, 8, EGL_NONE };
    EGLConfig c; EGLint n = 0;
    eglChooseConfig(d, ca, &c, 1, &n);
    if (!n) return 3;
    const EGLint sa[] = { EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE };
    EGLContext x = eglCreateContext(d, c, EGL_NO_CONTEXT, NULL);
    EGLSurface s = eglCreatePbufferSurface(d, c, sa);
    if (!x || !s || !eglMakeCurrent(d, s, s, x)) return 3;
    fprintf(stderr, "INFO version=%s\n", (const char *)glGetString(GL_VERSION));

    churn();

    GLuint vbo = 0;
    glGenBuffers(1, &vbo);
    glBindBuffer(GL_ARRAY_BUFFER, vbo);
    GLint got = -1;
    glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &got);
    fprintf(stderr, "INFO array_buffer name=%u query=%d\n", vbo, got);
    result("array_buffer_binding", (GLuint)got == vbo);

    glBindBuffer(GL_SHADER_STORAGE_BUFFER, vbo);
    got = -1;
    glGetIntegerv(GL_SHADER_STORAGE_BUFFER_BINDING, &got);
    fprintf(stderr, "INFO ssbo name=%u query=%d\n", vbo, got);
    result("shader_storage_binding", (GLuint)got == vbo);

    GLuint dispatch = 0;
    glGenBuffers(1, &dispatch);
    glBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, dispatch);
    got = -1;
    glGetIntegerv(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &got);
    fprintf(stderr, "INFO dispatch_indirect name=%u query=%d\n", dispatch, got);
    result("dispatch_indirect_binding", (GLuint)got == dispatch);

    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    got = -1;
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &got);
    fprintf(stderr, "INFO texture_2d name=%u query=%d\n", tex, got);
    result("texture_2d_binding", (GLuint)got == tex);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    got = -1;
    glGetIntegerv(GL_VERTEX_ARRAY_BINDING, &got);
    fprintf(stderr, "INFO vertex_array name=%u query=%d\n", vao, got);
    result("vertex_array_binding", (GLuint)got == vao);

    GLuint fbo = 0;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    got = -1;
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &got);
    fprintf(stderr, "INFO framebuffer name=%u query=%d\n", fbo, got);
    result("framebuffer_binding", (GLuint)got == fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, 0);

    /* The same binding through the other spellings. GL converts the object
     * name to whatever type was asked for, so a float and a double and a
     * 64-bit integer all carry it and all have to be translated --
     * KHR-GL44.compute_shader.dispatch-indirect asks
     * GL_DISPATCH_INDIRECT_BUFFER_BINDING four ways in one helper and
     * requires every answer to agree.
     *
     * glBooleanv is the exception and is checked as such: GL defines it as
     * false for zero and true for anything else, so it must NOT go through
     * the name map -- doing so would turn GL_TRUE into whichever object the
     * host numbered one. */
    {
        glBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, dispatch);
        GLfloat f = -1.0f;
        GLdouble db = -1.0;
        GLint64 i64 = -1;
        GLboolean b = GL_FALSE;
        glGetFloatv(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &f);
        glGetDoublev(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &db);
        glGetInteger64v(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &i64);
        glGetBooleanv(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &b);
        fprintf(stderr, "INFO other_spellings name=%u float=%g double=%g "
                "int64=%lld boolean=%d err=0x%04x\n", dispatch, (double)f,
                (double)db, (long long)i64, (int)b, (unsigned)glGetError());
        result("binding_as_float", f == (GLfloat)dispatch);
        result("binding_as_double", db == (GLdouble)dispatch);
        result("binding_as_int64", i64 == (GLint64)dispatch);
        result("binding_as_boolean", b == GL_TRUE);

        /* And zero stays zero in every spelling. */
        glBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, 0);
        f = -1.0f; b = GL_TRUE;
        glGetFloatv(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &f);
        glGetBooleanv(GL_DISPATCH_INDIRECT_BUFFER_BINDING, &b);
        fprintf(stderr, "INFO unbound float=%g boolean=%d want=0,0\n",
                (double)f, (int)b);
        result("unbound_is_zero", f == 0.0f && b == GL_FALSE);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* A float query whose first component is not a whole number.
     *
     * The name translation above has to be conditional on the pname. When it
     * was not, the first component of every float answer went through GLuint
     * on its way back: glGetFloatv(GL_COLOR_CLEAR_VALUE) with red 0.25 came
     * back 0, and so would GL_LINE_WIDTH 0.5. These are core-profile queries,
     * which is the point of testing them here -- the probe that first caught
     * it needed a compatibility context and so could not have gated this. */
    {
        glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
        GLfloat clear[4] = { -1.0f, -1.0f, -1.0f, -1.0f };
        glGetFloatv(GL_COLOR_CLEAR_VALUE, clear);
        fprintf(stderr, "INFO clear_value=%g,%g,%g,%g want=0.25,0.5,0.75,1\n",
                (double)clear[0], (double)clear[1], (double)clear[2],
                (double)clear[3]);
        result("float_query_keeps_fraction",
               clear[0] > 0.24f && clear[0] < 0.26f && clear[1] > 0.49f &&
               clear[2] > 0.74f && clear[3] > 0.99f);

        glLineWidth(1.0f);
        GLfloat width = -1.0f;
        glGetFloatv(GL_LINE_WIDTH, &width);
        fprintf(stderr, "INFO line_width=%g want=1\n", (double)width);
        result("line_width_query", width > 0.99f && width < 1.01f);
        while (glGetError() != GL_NO_ERROR) {}
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
