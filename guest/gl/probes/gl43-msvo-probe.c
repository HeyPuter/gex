/* gl43-msvo-probe: the compute shapes a screen-space AO pass uses (Unity's
 * PostProcessing MultiScaleVO is the one that found them): float images in
 * r32f, r16f and r8, written by compute and read back both by download and by
 * sampling; a 2D-array image; shared memory across a barrier; a depth texture
 * and textureGather sampled from compute; and a write-only image with no
 * format qualifier, whose format comes from the image unit.
 * Each case prints "msvo: NAME ok" or "msvo: NAME FAIL ..."; exit 0 only if
 * all pass. */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

static int failures;

static GLuint compute(const char *name, const char *source) {
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        GLchar log[2048]; GLsizei n = 0;
        glGetShaderInfoLog(shader, sizeof(log), &n, log);
        printf("msvo: %s FAIL compile: %.*s\n", name, (int)n, log);
        failures++;
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glDeleteShader(shader);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        printf("msvo: %s FAIL link\n", name);
        failures++;
        return 0;
    }
    return program;
}

static void report(const char *name, const float *got, const float *want, int n, float tolerance) {
    for (int i = 0; i < n; ++i) {
        if (!(fabsf(got[i] - want[i]) <= tolerance)) {
            printf("msvo: %s FAIL at %d: got %g want %g (", name, i, got[i], want[i]);
            for (int j = 0; j < n && j < 8; ++j) printf("%s%g", j ? " " : "", got[j]);
            printf(")\n");
            failures++;
            return;
        }
    }
    printf("msvo: %s ok\n", name);
}

static GLuint texture2d(GLenum internal, int w, int h) {
    GLuint t = 0;
    glGenTextures(1, &t);
    glBindTexture(GL_TEXTURE_2D, t);
    glTexStorage2D(GL_TEXTURE_2D, 1, internal, w, h);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    return t;
}

static GLuint ssbo(int binding, int floats) {
    GLuint b = 0;
    float zero[256] = {0};
    glGenBuffers(1, &b);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, b);
    glBufferData(GL_SHADER_STORAGE_BUFFER, floats * 4, zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, binding, b);
    return b;
}

static void read_ssbo(GLuint b, float *out, int floats) {
    glMemoryBarrier(GL_ALL_BARRIER_BITS);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, b);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, floats * 4, out);
}

/* Writes value(x, y) = (x + 4y + 1) / 16 into a 4x4 image. */
#define STORE_BODY \
    "void main() { ivec2 p = ivec2(gl_GlobalInvocationID.xy); " \
    "imageStore(img, p, vec4(float(p.x + 4 * p.y + 1) / 16.0, 0.25, 0.5, 1.0)); }\n"

/* Samples a 4x4 texture into an SSBO: texelFetch, then the same by textureLod. */
static const char sample_source[] =
    "#version 430 core\n"
    "layout(local_size_x=4, local_size_y=4) in;\n"
    "layout(binding=0) uniform sampler2D tex;\n"
    "layout(std430, binding=1) buffer Out { float v[]; };\n"
    "void main() { ivec2 p = ivec2(gl_GlobalInvocationID.xy); uint i = uint(p.x + 4 * p.y); "
    "v[i] = texelFetch(tex, p, 0).r; "
    "v[16u + i] = textureLod(tex, (vec2(p) + 0.5) / 4.0, 0.0).r; }\n";

static void float_image_case(const char *name, GLenum internal, const char *qualifier,
                             GLuint sampler_program, float tolerance) {
    char source[1024];
    snprintf(source, sizeof(source),
             "#version 430 core\n"
             "layout(local_size_x=4, local_size_y=4) in;\n"
             "layout(%s binding=2) writeonly uniform image2D img;\n" STORE_BODY,
             qualifier);
    GLuint program = compute(name, source);
    if (!program || !sampler_program) return;
    GLuint t = texture2d(internal, 4, 4);
    glBindImageTexture(2, t, 0, GL_FALSE, 0, GL_WRITE_ONLY, internal);
    glUseProgram(program);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_ALL_BARRIER_BITS);

    float want[32];
    for (int i = 0; i < 16; ++i) want[i] = want[16 + i] = (float)(i + 1) / 16.0f;
    char label[128];

    /* Downloaded. */
    float got[32] = {0};
    glBindTexture(GL_TEXTURE_2D, t);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RED, GL_FLOAT, got);
    snprintf(label, sizeof(label), "%s download", name);
    report(label, got, want, 16, tolerance);

    /* Sampled by a later dispatch, as the next pass of an AO chain does. */
    GLuint out = ssbo(1, 32);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_2D, t);
    glUseProgram(sampler_program);
    glDispatchCompute(1, 1, 1);
    memset(got, 0, sizeof(got));
    read_ssbo(out, got, 32);
    snprintf(label, sizeof(label), "%s sampled", name);
    report(label, got, want, 32, tolerance);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) {
        puts("msvo: FAIL egl");
        return 1;
    }
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 24, EGL_NONE,
    };
    const EGLint pbuffer_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4, EGL_CONTEXT_MINOR_VERSION, 3,
        EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE,
    };
    EGLConfig config = NULL; EGLint count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &count) || count < 1) {
        puts("msvo: FAIL config");
        return 1;
    }
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, pbuffer_attributes);
    if (context == EGL_NO_CONTEXT || !eglMakeCurrent(display, surface, surface, context)) {
        puts("msvo: FAIL context");
        return 1;
    }
    printf("msvo: GL %s\n", (const char *)glGetString(GL_VERSION));

    GLuint sampler_program = compute("sampler", sample_source);

    float_image_case("r32f", GL_R32F, "r32f,", sampler_program, 1e-6f);
    float_image_case("r16f", GL_R16F, "r16f,", sampler_program, 1e-3f);
    float_image_case("r8", GL_R8, "r8,", sampler_program, 1.0f / 255.0f);
    float_image_case("rg16f", GL_RG16F, "rg16f,", sampler_program, 1e-3f);
    float_image_case("rgba16f", GL_RGBA16F, "rgba16f,", sampler_program, 1e-3f);
    /* No qualifier: the format is the unit's. */
    float_image_case("unformatted r32f", GL_R32F, "", sampler_program, 1e-6f);
    float_image_case("unformatted r16f", GL_R16F, "", sampler_program, 1e-3f);
    float_image_case("unformatted r8", GL_R8, "", sampler_program, 1.0f / 255.0f);

    /* A 2D-array r16f image, every layer written in one dispatch. */
    {
        GLuint program = compute("r16f array",
            "#version 430 core\n"
            "layout(local_size_x=4, local_size_y=4) in;\n"
            "layout(r16f, binding=3) writeonly uniform image2DArray img;\n"
            "void main() { ivec3 p = ivec3(gl_GlobalInvocationID); "
            "imageStore(img, p, vec4(float(p.x + 4 * p.y + 16 * p.z) / 64.0)); }\n");
        GLuint reader = compute("r16f array reader",
            "#version 430 core\n"
            "layout(local_size_x=4, local_size_y=4) in;\n"
            "layout(binding=0) uniform sampler2DArray tex;\n"
            "layout(std430, binding=1) buffer Out { float v[]; };\n"
            "void main() { ivec3 p = ivec3(gl_GlobalInvocationID); "
            "v[p.x + 4 * p.y + 16 * p.z] = texelFetch(tex, p, 0).r; }\n");
        if (program && reader) {
            GLuint t = 0;
            glGenTextures(1, &t);
            glBindTexture(GL_TEXTURE_2D_ARRAY, t);
            glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_R16F, 4, 4, 4);
            glBindImageTexture(3, t, 0, GL_TRUE, 0, GL_WRITE_ONLY, GL_R16F);
            glUseProgram(program);
            glDispatchCompute(1, 1, 4);
            glMemoryBarrier(GL_ALL_BARRIER_BITS);
            GLuint out = ssbo(1, 64);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D_ARRAY, t);
            glUseProgram(reader);
            glDispatchCompute(1, 1, 4);
            float got[64], want[64];
            read_ssbo(out, got, 64);
            for (int i = 0; i < 64; ++i) want[i] = (float)i / 64.0f;
            report("r16f array sampled", got, want, 64, 1e-3f);
        }
    }

    /* Shared memory across a barrier: each invocation reads its mirror's value. */
    {
        GLuint program = compute("shared",
            "#version 430 core\n"
            "layout(local_size_x=8, local_size_y=8) in;\n"
            "shared float cache[64];\n"
            "layout(std430, binding=1) buffer Out { float v[]; };\n"
            "void main() { uint i = gl_LocalInvocationIndex; "
            "cache[i] = float(i) * 0.5; "
            "memoryBarrierShared(); barrier(); "
            "v[i] = cache[63u - i]; }\n");
        if (program) {
            GLuint out = ssbo(1, 64);
            glUseProgram(program);
            glDispatchCompute(1, 1, 1);
            float got[64], want[64];
            read_ssbo(out, got, 64);
            for (int i = 0; i < 64; ++i) want[i] = (float)(63 - i) * 0.5f;
            report("shared barrier", got, want, 64, 0.0f);
        }
    }

    /* A depth texture, rendered by a clear, sampled from compute. */
    {
        GLuint program = compute("depth",
            "#version 430 core\n"
            "layout(local_size_x=4, local_size_y=4) in;\n"
            "layout(binding=0) uniform sampler2D depth;\n"
            "layout(std430, binding=1) buffer Out { float v[]; };\n"
            "void main() { ivec2 p = ivec2(gl_GlobalInvocationID.xy); uint i = uint(p.x + 4 * p.y); "
            "v[i] = texelFetch(depth, p, 0).r; "
            "v[16u + i] = textureLod(depth, (vec2(p) + 0.5) / 4.0, 0.0).r; }\n");
        if (program) {
            GLenum formats[2] = {GL_DEPTH_COMPONENT24, GL_DEPTH_COMPONENT32F};
            const char *names[2] = {"depth24 sampled", "depth32f sampled"};
            for (int f = 0; f < 2; ++f) {
                GLuint t = texture2d(formats[f], 4, 4);
                GLuint fb = 0;
                glGenFramebuffers(1, &fb);
                glBindFramebuffer(GL_FRAMEBUFFER, fb);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_DEPTH_ATTACHMENT, GL_TEXTURE_2D, t, 0);
                glDrawBuffer(GL_NONE);
                glReadBuffer(GL_NONE);
                glClearDepth(0.375);
                glClear(GL_DEPTH_BUFFER_BIT);
                glBindFramebuffer(GL_FRAMEBUFFER, 0);
                GLuint out = ssbo(1, 32);
                glActiveTexture(GL_TEXTURE0);
                glBindTexture(GL_TEXTURE_2D, t);
                glUseProgram(program);
                glDispatchCompute(1, 1, 1);
                float got[32], want[32];
                read_ssbo(out, got, 32);
                for (int i = 0; i < 32; ++i) want[i] = 0.375f;
                report(names[f], got, want, 32, 1e-5f);
            }
        }
    }

    /* textureGather on an r16f 2D array, written by compute first. */
    {
        GLuint writer = compute("gather writer",
            "#version 430 core\n"
            "layout(local_size_x=4, local_size_y=4) in;\n"
            "layout(r16f, binding=3) writeonly uniform image2DArray img;\n"
            "void main() { ivec3 p = ivec3(gl_GlobalInvocationID); "
            "imageStore(img, p, vec4(float(p.x + 4 * p.y + 16 * p.z) / 64.0)); }\n");
        GLuint program = compute("gather",
            "#version 430 core\n"
            "layout(local_size_x=1) in;\n"
            "layout(binding=0) uniform sampler2DArray tex;\n"
            "layout(std430, binding=1) buffer Out { vec4 v[]; };\n"
            "void main() { v[0] = textureGather(tex, vec3(0.5, 0.5, 1.0)); }\n");
        if (writer && program) {
            GLuint t = 0;
            glGenTextures(1, &t);
            glBindTexture(GL_TEXTURE_2D_ARRAY, t);
            glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_R16F, 4, 4, 2);
            glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            glBindImageTexture(3, t, 0, GL_TRUE, 0, GL_WRITE_ONLY, GL_R16F);
            glUseProgram(writer);
            glDispatchCompute(1, 1, 2);
            glMemoryBarrier(GL_ALL_BARRIER_BITS);
            GLuint out = ssbo(1, 4);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D_ARRAY, t);
            glUseProgram(program);
            glDispatchCompute(1, 1, 1);
            float got[4];
            read_ssbo(out, got, 4);
            /* The 2x2 around the centre of layer 1: texels (1,2) (2,2) (2,1) (1,1). */
            float want[4] = {(1 + 8 + 16) / 64.0f, (2 + 8 + 16) / 64.0f,
                             (2 + 4 + 16) / 64.0f, (1 + 4 + 16) / 64.0f};
            report("gather r16f array", got, want, 4, 1e-3f);
        }
    }

    GLenum error = glGetError();
    if (error != GL_NO_ERROR) {
        printf("msvo: FAIL GL error 0x%x\n", error);
        failures++;
    }
    printf("msvo: %d failure(s)\n", failures);
    return failures ? 1 : 0;
}
