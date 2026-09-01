#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int cases, passed, failed;

static GLuint shader(GLenum type, const char *source) {
    GLuint object = glCreateShader(type);
    glShaderSource(object, 1, &source, NULL);
    glCompileShader(object);
    GLint ok = 0;
    glGetShaderiv(object, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetShaderInfoLog(object, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL shader: %s\n", log);
        exit(2);
    }
    return object;
}

static GLuint program(const char *input_type, const char *conversion) {
    char vertex[2048];
    snprintf(vertex, sizeof(vertex),
        "#version 440 core\n"
        "layout(location=0) in vec2 position;\n"
        "layout(location=1) in %s value;\n"
        "out vec4 color;\n"
        "void main(){gl_Position=vec4(position,0,1);color=%s;}\n",
        input_type, conversion);
    const char *fragment =
        "#version 440 core\n"
        "in vec4 color; out vec4 output_color;\n"
        "void main(){output_color=color;}\n";
    GLuint vs = shader(GL_VERTEX_SHADER, vertex);
    GLuint fs = shader(GL_FRAGMENT_SHADER, fragment);
    GLuint object = glCreateProgram();
    glAttachShader(object, vs);
    glAttachShader(object, fs);
    glLinkProgram(object);
    GLint ok = 0;
    glGetProgramiv(object, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[4096] = {0};
        glGetProgramInfoLog(object, sizeof(log), NULL, log);
        fprintf(stderr, "FATAL link: %s\n", log);
        exit(2);
    }
    return object;
}

static void result(const char *name, const unsigned char expected[4], int tolerance) {
    unsigned char actual[4] = {0};
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, actual);
    GLenum error = glGetError();
    int ok = error == GL_NO_ERROR;
    for (int i = 0; i < 4; ++i) {
        int delta = (int)actual[i] - (int)expected[i];
        if (delta < 0) delta = -delta;
        if (delta > tolerance) ok = 0;
    }
    ++cases;
    if (ok) ++passed; else ++failed;
    printf("CASE %-36s %s expected=%u,%u,%u,%u actual=%u,%u,%u,%u gl=0x%04x\n",
           name, ok ? "PASS" : "FAIL", expected[0], expected[1], expected[2], expected[3],
           actual[0], actual[1], actual[2], actual[3], error);
}

static void draw(GLuint object) {
    glUseProgram(object);
    glClearColor(0, 0, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArrays(GL_TRIANGLES, 0, 3);
}

static void upload_attrib(GLuint buffer, const void *data, size_t bytes, GLint size,
                          GLenum type, GLboolean normalized, GLsizei stride, size_t offset) {
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)bytes, data, GL_STATIC_DRAW);
    glVertexAttribPointer(1, size, type, normalized, stride, (const void *)offset);
    glEnableVertexAttribArray(1);
    glVertexAttribDivisor(1, 0);
}

static void upload_iattrib(GLuint buffer, const void *data, size_t bytes, GLint size,
                           GLenum type, GLsizei stride, size_t offset) {
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, (GLsizeiptr)bytes, data, GL_STATIC_DRAW);
    glVertexAttribIPointer(1, size, type, stride, (const void *)offset);
    glEnableVertexAttribArray(1);
    glVertexAttribDivisor(1, 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 2;
    EGLConfig config = NULL; EGLint count = 0;
    eglChooseConfig(display, NULL, &config, 1, &count);
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    const EGLint attrs[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, attrs);
    if (!eglMakeCurrent(display, surface, surface, context)) return 2;

    printf("INFO vendor=%s renderer=%s version=%s\n", glGetString(GL_VENDOR),
           glGetString(GL_RENDERER), glGetString(GL_VERSION));

    GLuint float_program = program("vec4", "value");
    GLuint uint_program = program("uvec4", "vec4(value)/255.0");
    GLuint int_program = program("ivec4", "vec4(value+ivec4(128))/255.0");
    GLuint vao, position_buffer, attribute_buffer;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    const float positions[] = {-1,-1, 3,-1, -1,3};
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
    glEnableVertexAttribArray(0);
    glGenBuffers(1, &attribute_buffer);
    GLuint color_texture, framebuffer;
    glGenTextures(1, &color_texture);
    glBindTexture(GL_TEXTURE_2D, color_texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 1, 1, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, color_texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return 2;
    glViewport(0, 0, 1, 1);

    const float floats[] = {0.125f,0.375f,0.75f,1, 0.125f,0.375f,0.75f,1, 0.125f,0.375f,0.75f,1};
    const unsigned char e_float[] = {32,96,191,255};
    upload_attrib(attribute_buffer, floats, sizeof(floats), 4, GL_FLOAT, GL_FALSE, 0, 0);
    draw(float_program); result("float4_tight", e_float, 1);

    struct OddU8 { unsigned char pad, rgba[4]; };
    const struct OddU8 odd_u8[] = {{9,{17,93,201,255}}, {8,{17,93,201,255}}, {7,{17,93,201,255}}};
    const unsigned char e_u8[] = {17,93,201,255};
    upload_attrib(attribute_buffer, odd_u8, sizeof(odd_u8), 4, GL_UNSIGNED_BYTE, GL_TRUE, 5, 1);
    draw(float_program); result("normalized_u8_odd_offset_stride", e_u8, 0);

    struct OddI8 { unsigned char pad; int8_t rgba[4]; };
    const struct OddI8 odd_i8[] = {{0,{-128,0,127,64}}, {0,{-128,0,127,64}}, {0,{-128,0,127,64}}};
    const unsigned char e_i8[] = {0,128,255,192};
    upload_attrib(attribute_buffer, odd_i8, sizeof(odd_i8), 4, GL_BYTE, GL_TRUE, 5, 1);
    glUseProgram(float_program);
    /* Convert signed normalized [-1,1] to visible [0,1] in a dedicated program. */
    GLuint snorm_program = program("vec4", "value*0.5+0.5");
    draw(snorm_program); result("normalized_i8_odd_offset_stride", e_i8, 1);

    struct OddU16 { uint16_t pad, rgba[4]; };
    const struct OddU16 odd_u16[] = {{1,{0,16384,49151,65535}}, {2,{0,16384,49151,65535}}, {3,{0,16384,49151,65535}}};
    const unsigned char e_u16[] = {0,64,191,255};
    upload_attrib(attribute_buffer, odd_u16, sizeof(odd_u16), 4, GL_UNSIGNED_SHORT, GL_TRUE, 10, 2);
    draw(float_program); result("normalized_u16_offset_stride", e_u16, 1);

    const uint16_t halfs[] = {0x0000,0x3800,0x3c00,0x3c00, 0x0000,0x3800,0x3c00,0x3c00, 0x0000,0x3800,0x3c00,0x3c00};
    const unsigned char e_half[] = {0,128,255,255};
    upload_attrib(attribute_buffer, halfs, sizeof(halfs), 4, GL_HALF_FLOAT, GL_FALSE, 0, 0);
    draw(float_program); result("half_float", e_half, 1);

    const uint32_t packed_value = 1023u | (512u << 10) | (0u << 20) | (3u << 30);
    const uint32_t packed[] = {packed_value, packed_value, packed_value};
    const unsigned char e_packed[] = {255,128,0,255};
    upload_attrib(attribute_buffer, packed, sizeof(packed), 4,
                  GL_UNSIGNED_INT_2_10_10_10_REV, GL_TRUE, 0, 0);
    draw(float_program); result("packed_2_10_10_10_rev", e_packed, 1);

    const uint32_t signed_packed_value = 0x200u | (0x000u << 10) | (0x1ffu << 20) | (1u << 30);
    const uint32_t signed_packed[] = {signed_packed_value, signed_packed_value, signed_packed_value};
    const unsigned char e_signed_packed[] = {0,128,255,255};
    upload_attrib(attribute_buffer, signed_packed, sizeof(signed_packed), 4,
                  GL_INT_2_10_10_10_REV, GL_TRUE, 0, 0);
    draw(snorm_program); result("signed_packed_normalized", e_signed_packed, 1);

    GLuint raw_packed_program = program("vec4", "value/vec4(1023,1023,1023,3)");
    upload_attrib(attribute_buffer, packed, sizeof(packed), 4,
                  GL_UNSIGNED_INT_2_10_10_10_REV, GL_FALSE, 0, 0);
    draw(raw_packed_program); result("packed_non_normalized", e_packed, 1);

    struct OddUI { unsigned char pad; uint8_t rgba[4]; };
    const struct OddUI odd_ui[] = {{1,{11,77,149,255}}, {2,{11,77,149,255}}, {3,{11,77,149,255}}};
    const unsigned char e_ui[] = {11,77,149,255};
    upload_iattrib(attribute_buffer, odd_ui, sizeof(odd_ui), 4, GL_UNSIGNED_BYTE, 5, 1);
    draw(uint_program); result("integer_u8_odd_offset_stride", e_ui, 0);

    struct OddSI { unsigned char pad; int8_t rgba[4]; };
    const struct OddSI odd_si[] = {{1,{-128,-64,0,127}}, {2,{-128,-64,0,127}}, {3,{-128,-64,0,127}}};
    const unsigned char e_si[] = {0,64,128,255};
    upload_iattrib(attribute_buffer, odd_si, sizeof(odd_si), 4, GL_BYTE, 5, 1);
    draw(int_program); result("integer_i8_odd_offset_stride", e_si, 0);

    glDisableVertexAttribArray(1);
    glVertexAttrib4f(1, 0.2f, 0.4f, 0.6f, 0.8f);
    const unsigned char e_current[] = {51,102,153,204};
    draw(float_program); result("current_generic_float", e_current, 1);

    const unsigned char instance_colors[] = {255,0,0,255, 0,255,0,255};
    upload_attrib(attribute_buffer, instance_colors, sizeof(instance_colors), 4,
                  GL_UNSIGNED_BYTE, GL_TRUE, 4, 0);
    glVertexAttribDivisor(1, 2);
    glUseProgram(float_program); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstanced(GL_TRIANGLES, 0, 3, 3);
    const unsigned char e_green[] = {0,255,0,255};
    result("instance_divisor_two", e_green, 0);

    const struct OddU8 odd_instances[] = {{1,{255,0,0,255}}, {2,{0,255,0,255}}};
    upload_attrib(attribute_buffer, odd_instances, sizeof(odd_instances), 4,
                  GL_UNSIGNED_BYTE, GL_TRUE, 5, 1);
    glVertexAttribDivisor(1, 2);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstanced(GL_TRIANGLES, 0, 3, 3);
    result("instance_divisor_deinterleaved", e_green, 0);

    const unsigned char integer_instances[] = {255,0,0,255, 0,255,0,255};
    GLuint raw_u8_program = program("vec4", "value/255.0");
    upload_attrib(attribute_buffer, integer_instances, sizeof(integer_instances), 4,
                  GL_UNSIGNED_BYTE, GL_FALSE, 4, 0);
    glVertexAttribDivisor(1, 2);
    glUseProgram(raw_u8_program); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstanced(GL_TRIANGLES, 0, 3, 3);
    result("instance_divisor_integer_convert", e_green, 0);

    const uint32_t packed_instances[] = {
        1023u | (3u << 30),
        (1023u << 10) | (3u << 30),
    };
    upload_attrib(attribute_buffer, packed_instances, sizeof(packed_instances), 4,
                  GL_UNSIGNED_INT_2_10_10_10_REV, GL_TRUE, 4, 0);
    glVertexAttribDivisor(1, 2);
    glUseProgram(float_program); glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstanced(GL_TRIANGLES, 0, 3, 3);
    result("instance_divisor_packed", e_green, 0);

    glVertexAttribDivisor(1, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    glDrawArraysInstancedBaseInstance(GL_TRIANGLES, 0, 3, 1, 1);
    result("base_instance_fetch", e_green, 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed, failed);
    return failed ? 1 : 0;
}
