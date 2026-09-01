#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>

static GLuint compile(GLenum type, const char *source) {
    GLuint shader = glCreateShader(type);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof log, NULL, log);
        fprintf(stderr, "shader compile failed: %s\n", log);
        exit(2);
    }
    return shader;
}

static int floats_equal(const float *actual, const float *expected, int count) {
    for (int i = 0; i < count; i++) {
        if (fabsf(actual[i] - expected[i]) > 0.0001f) return 0;
    }
    return 1;
}

static void print_floats(const char *name, const float *values, int count) {
    printf("%s", name);
    for (int i = 0; i < count; i++) printf(" %g", values[i]);
    putchar('\n');
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 1;
    EGLConfig config = NULL;
    EGLint count = 0;
    eglChooseConfig(display, NULL, &config, 1, &count);
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, NULL);
    if (!eglMakeCurrent(display, surface, surface, context)) return 1;

    static const char *vertex =
        "#version 330 core\n"
        "uniform vec4 uf; uniform ivec3 si; uniform uvec4 uu;\n"
        "uniform mat3x2 m32; uniform mat2x3 m23; uniform mat2x4 m24;\n"
        "uniform mat4x2 m42; uniform mat3x4 m34; uniform mat4x3 m43;\n"
        "out vec4 colour;\n"
        "void main() {\n"
        "  float used = uf.x + float(si.y) + float(uu.z) + m32[1][0] + m23[0][2]\n"
        "    + m24[1][3] + m42[3][1] + m34[2][3] + m43[3][2];\n"
        "  colour = vec4(used); gl_Position = vec4(0.0, 0.0, 0.0, 1.0);\n"
        "}\n";
    static const char *fragment =
        "#version 330 core\n"
        "in vec4 colour; out vec4 out_colour;\n"
        "void main() { out_colour = colour; }\n";

    GLuint program = glCreateProgram();
    glAttachShader(program, compile(GL_VERTEX_SHADER, vertex));
    glAttachShader(program, compile(GL_FRAGMENT_SHADER, fragment));
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) return 3;

    GLint uf = glGetUniformLocation(program, "uf");
    GLint si = glGetUniformLocation(program, "si");
    GLint uu = glGetUniformLocation(program, "uu");
    GLint m32 = glGetUniformLocation(program, "m32");
    GLint m23 = glGetUniformLocation(program, "m23");
    GLint m24 = glGetUniformLocation(program, "m24");
    GLint m42 = glGetUniformLocation(program, "m42");
    GLint m34 = glGetUniformLocation(program, "m34");
    GLint m43 = glGetUniformLocation(program, "m43");
    if (uf < 0 || si < 0 || uu < 0 || m32 < 0 || m23 < 0 || m24 < 0 ||
        m42 < 0 || m34 < 0 || m43 < 0) return 4;

    /* Direct-state-access updates must target `program` while it is not the
       current program. This exercises guest object-name translation too. */
    glUseProgram(0);
    const GLfloat expected_f[4] = {1.25f, -2.5f, 3.75f, 4.5f};
    const GLint expected_i[3] = {-7, 11, 23};
    const GLuint expected_u[4] = {3, 5, 8, 13};
    const GLfloat expected_m32[6] = {1, 2, 3, 4, 5, 6};
    /* transpose=TRUE input is row-major; readback is always column-major. */
    const GLfloat input_m24_transposed[8] = {1, 5, 2, 6, 3, 7, 4, 8};
    const GLfloat expected_m24[8] = {1, 2, 3, 4, 5, 6, 7, 8};
    const GLfloat expected_m42[8] = {8, 7, 6, 5, 4, 3, 2, 1};
    const GLfloat expected_m34[12] = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12};
    const GLfloat input_m43_transposed[12] = {12, 9, 6, 3, 11, 8, 5, 2, 10, 7, 4, 1};
    const GLfloat expected_m43[12] = {12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1};
    glProgramUniform4fv(program, uf, 1, expected_f);
    glProgramUniform3iv(program, si, 1, expected_i);
    glProgramUniform4uiv(program, uu, 1, expected_u);
    glProgramUniformMatrix3x2fv(program, m32, 1, GL_FALSE, expected_m32);
    glProgramUniformMatrix2x4fv(program, m24, 1, GL_TRUE, input_m24_transposed);
    glProgramUniformMatrix4x2fv(program, m42, 1, GL_FALSE, expected_m42);
    glProgramUniformMatrix3x4fv(program, m34, 1, GL_FALSE, expected_m34);
    glProgramUniformMatrix4x3fv(program, m43, 1, GL_TRUE, input_m43_transposed);

    GLfloat got_f[12] = {0};
    GLint got_i[4] = {0};
    glGetUniformfv(program, uf, got_f);
    if (!floats_equal(got_f, expected_f, 4)) return 5;
    glGetUniformiv(program, si, got_i);
    for (int i = 0; i < 3; i++) if (got_i[i] != expected_i[i]) return 6;
    glGetUniformiv(program, uu, got_i);
    for (int i = 0; i < 4; i++) if ((GLuint)got_i[i] != expected_u[i]) return 7;
    glGetUniformfv(program, m32, got_f);
    if (!floats_equal(got_f, expected_m32, 6)) {
        print_floats("m32 expected", expected_m32, 6);
        print_floats("m32 actual", got_f, 6);
        return 8;
    }
    glGetUniformfv(program, m24, got_f);
    if (!floats_equal(got_f, expected_m24, 8)) return 11;
    glGetUniformfv(program, m42, got_f);
    if (!floats_equal(got_f, expected_m42, 8)) return 12;
    glGetUniformfv(program, m34, got_f);
    if (!floats_equal(got_f, expected_m34, 12)) return 13;
    glGetUniformfv(program, m43, got_f);
    if (!floats_equal(got_f, expected_m43, 12)) return 14;

    /* The new non-square ordinary uniform transport uses current-program
       semantics, independently of the direct-state-access family above. */
    const GLfloat expected_m23[6] = {6, 5, 4, 3, 2, 1};
    glUseProgram(program);
    glUniformMatrix2x3fv(m23, 1, GL_FALSE, expected_m23);
    glGetUniformfv(program, m23, got_f);
    if (!floats_equal(got_f, expected_m23, 6)) {
        print_floats("m23 expected", expected_m23, 6);
        print_floats("m23 actual", got_f, 6);
        return 9;
    }

    GLenum error = glGetError();
    if (error != GL_NO_ERROR) {
        fprintf(stderr, "unexpected GL error 0x%x\n", error);
        return 10;
    }
    puts("gl44 uniform transport ok");
    return 0;
}
