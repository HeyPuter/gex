#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/glcorearb.h>
#include <stdio.h>
#include <string.h>

#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "compute probe failed: %s (line %d)\n", message, __LINE__); return __LINE__; } \
} while (0)

static GLuint compile_compute(const char *source) {
    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    const GLchar *sources[] = {source};
    glShaderSource(shader, 1, sources, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        GLchar log[2048]; GLsizei length = 0;
        glGetShaderInfoLog(shader, sizeof(log), &length, log);
        fprintf(stderr, "compute compile: %.*s\n", (int)length, log);
        return 0;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glDeleteShader(shader);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        GLchar log[2048]; GLsizei length = 0;
        glGetProgramInfoLog(program, sizeof(log), &length, log);
        fprintf(stderr, "compute link: %.*s\n", (int)length, log);
        return 0;
    }
    return program;
}

static const char writer_source[] =
        "#version 430 core\n"
        "layout(local_size_x=4) in;\n"
        "layout(std430, binding=2) buffer Output { uint values[]; };\n"
        "layout(std430, binding=3) buffer Commands { uint commands[]; };\n"
        "void main() { uint i=gl_GlobalInvocationID.x; values[i]=0x51a70000u+i*17u; "
        "if(i==0u){commands[4]=2u;commands[5]=1u;commands[6]=1u;} }\n";
static const char worker_source[] =
        "#version 430 core\n"
        "layout(local_size_x=4) in;\n"
        "layout(std430, binding=2) buffer Output { uint values[]; };\n"
        "void main() { uint i=gl_GlobalInvocationID.x; values[i]=0x51a70000u+i*17u; }\n";
static const char image_source[] =
        "#version 430 core\n"
        "layout(local_size_x=4) in;\n"
        "layout(rgba8, binding=1) writeonly uniform image2D target_image;\n"
        "void main() { uint i=gl_GlobalInvocationID.x; vec4 c=vec4(0.0); "
        "c.r=(i==0u||i==3u)?1.0:0.0; c.g=(i==1u||i==3u)?1.0:0.0; "
        "c.b=(i==2u||i==3u)?1.0:0.0; c.a=1.0; "
        "imageStore(target_image, ivec2(int(i),0), c); }\n";
static const char uint_image_writer_source[] =
        "#version 430 core\n"
        "layout(local_size_x=4) in;\n"
        "layout(r32ui, binding=4) writeonly uniform uimage2D uint_target;\n"
        "void main() { uint i=gl_GlobalInvocationID.x; "
        "imageStore(uint_target, ivec2(int(i),0), uvec4(0x1200u+i*19u)); }\n";
static const char uint_image_reader_source[] =
        "#version 430 core\n"
        "layout(local_size_x=4) in;\n"
        "layout(r32ui, binding=4) readonly uniform uimage2D uint_target;\n"
        "layout(std430, binding=6) buffer ImageReadback { uint values[]; };\n"
        "void main() { uint i=gl_GlobalInvocationID.x; "
        "values[i]=imageLoad(uint_target, ivec2(int(i),0)).r; }\n";
static const char array_image_writer_source[] =
        "#version 430 core\n"
        "layout(local_size_x=2) in;\n"
        "layout(r32ui, binding=7) writeonly uniform uimage2DArray array_target;\n"
        "void main() { uint x=gl_GlobalInvocationID.x, layer=gl_GlobalInvocationID.z; "
        "imageStore(array_target, ivec3(int(x),0,int(layer)), "
        "uvec4(0x3300u+layer*101u+x*7u)); }\n";
static const char array_image_reader_source[] =
        "#version 430 core\n"
        "layout(local_size_x=2) in;\n"
        "layout(r32ui, binding=7) readonly uniform uimage2DArray array_target;\n"
        "layout(std430, binding=5) buffer ArrayReadback { uint values[]; };\n"
        "void main() { uint x=gl_GlobalInvocationID.x, layer=gl_GlobalInvocationID.z; "
        "values[layer*2u+x]=imageLoad(array_target, ivec3(int(x),0,int(layer))).r; }\n";
static const char array_layer_writer_source[] =
        "#version 430 core\n"
        "layout(local_size_x=2) in;\n"
        "layout(r32ui, binding=0) writeonly uniform uimage2D selected_layer;\n"
        "void main() { uint x=gl_GlobalInvocationID.x; "
        "imageStore(selected_layer, ivec2(int(x),0), uvec4(0x4400u+x*13u)); }\n";
static const char array_layer_reader_source[] =
        "#version 430 core\n"
        "layout(local_size_x=2) in;\n"
        "layout(r32ui, binding=0) readonly uniform uimage2D selected_layer;\n"
        "layout(std430, binding=5) buffer LayerReadback { uint values[]; };\n"
        "void main() { uint x=gl_GlobalInvocationID.x; "
        "values[x]=imageLoad(selected_layer, ivec2(int(x),0)).r; }\n";
static const char image3d_writer_source[] =
        "#version 430 core\n"
        "layout(local_size_x=2) in;\n"
        "layout(r32ui, binding=2) writeonly uniform uimage3D volume;\n"
        "void main() { uint x=gl_GlobalInvocationID.x, z=gl_GlobalInvocationID.z; "
        "imageStore(volume, ivec3(int(x),0,int(z)), uvec4(0x5500u+z*23u+x*3u)); }\n";
static const char image3d_reader_source[] =
        "#version 430 core\n"
        "layout(local_size_x=2) in;\n"
        "layout(r32ui, binding=2) readonly uniform uimage3D volume;\n"
        "layout(std430, binding=4) buffer VolumeReadback { uint values[]; };\n"
        "void main() { uint x=gl_GlobalInvocationID.x, z=gl_GlobalInvocationID.z; "
        "values[z*2u+x]=imageLoad(volume, ivec3(int(x),0,int(z))).r; }\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    CHECK(eglInitialize(display, NULL, NULL), "eglInitialize");
    CHECK(eglBindAPI(EGL_OPENGL_API), "eglBindAPI");
    EGLConfig config = NULL; EGLint config_count = 0;
    /* An explicit pbuffer config, and a checked config_count. With a NULL
     * attribute list eglChooseConfig defaults to EGL_WINDOW_BIT, which matches
     * nothing under EGL_PLATFORM=surfaceless: it returned zero configs, the
     * config stayed NULL, the pbuffer failed -- and eglMakeCurrent succeeded
     * anyway, leaving a context with no default framebuffer. */
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8,
        EGL_DEPTH_SIZE, 24, EGL_STENCIL_SIZE, 8, EGL_NONE,
    };
    const EGLint gex_pbuffer_attributes[] = {EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE};
    CHECK(eglChooseConfig(display, config_attributes, &config, 1, &config_count), "eglChooseConfig");
    CHECK(config_count >= 1 && config != NULL, "no pbuffer config");
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    EGLSurface surface = eglCreatePbufferSurface(display, config, gex_pbuffer_attributes);
    CHECK(eglMakeCurrent(display, surface, surface, context), "eglMakeCurrent");

    GLuint program = compile_compute(writer_source);
    GLuint indirect_program = compile_compute(worker_source);
    GLuint image_program = compile_compute(image_source);
    GLuint uint_image_writer = compile_compute(uint_image_writer_source);
    GLuint uint_image_reader = compile_compute(uint_image_reader_source);
    GLuint array_image_writer = compile_compute(array_image_writer_source);
    GLuint array_image_reader = compile_compute(array_image_reader_source);
    GLuint array_layer_writer = compile_compute(array_layer_writer_source);
    GLuint array_layer_reader = compile_compute(array_layer_reader_source);
    GLuint image3d_writer = compile_compute(image3d_writer_source);
    GLuint image3d_reader = compile_compute(image3d_reader_source);
    CHECK(program != 0, "compute program");
    CHECK(indirect_program != 0, "indirect compute program");
    CHECK(image_program != 0, "image compute program");
    CHECK(uint_image_writer != 0, "uint image writer program");
    CHECK(uint_image_reader != 0, "uint image reader program");
    CHECK(array_image_writer != 0, "array image writer program");
    CHECK(array_image_reader != 0, "array image reader program");
    CHECK(array_layer_writer != 0, "selected array layer writer program");
    CHECK(array_layer_reader != 0, "selected array layer reader program");
    CHECK(image3d_writer != 0, "3D image writer program");
    CHECK(image3d_reader != 0, "3D image reader program");
    GLuint buffer = 0;
    GLuint initial[8] = {0};
    glGenBuffers(1, &buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(initial), initial, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 2, buffer);
    GLuint indirect_buffer = 0;
    const GLuint indirect_words[7] = {0};
    glGenBuffers(1, &indirect_buffer);
    glBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, indirect_buffer);
    glBufferData(GL_DISPATCH_INDIRECT_BUFFER, sizeof(indirect_words), indirect_words, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 3, indirect_buffer);
    glUseProgram(program);
    glDispatchCompute(2, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);

    GLuint result[8];
    memset(result, 0, sizeof(result));
    /* glBindBufferBase sets the *generic* binding point as well as the
       indexed one, so the indirect buffer bound at index 3 above is what
       GL_SHADER_STORAGE_BUFFER still names. Reading without re-binding read
       that buffer instead -- 28 bytes asked for 32 --
       "GL_INVALID_VALUE in glGetBufferSubData(offset 0 + size 32 > buffer
       size 28)", which failed on every driver and stopped this probe before
       any of the cases after it. Every other readback below already binds
       first; this one did not. */
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(result), result);
    for (GLuint i = 0; i < 8; ++i) {
        CHECK(result[i] == 0x51a70000u + i * 17u, "SSBO result/readback");
    }
    GLuint command_result[7] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, indirect_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(command_result), command_result);
    CHECK(command_result[4] == 2 && command_result[5] == 1 && command_result[6] == 1,
          "GPU-written indirect command readback");

    /* Repeat through a deliberately nonzero indirect-command offset. Resetting
       the output makes this a semantic indirect-dispatch check rather than a
       second read of the direct dispatch result. */
    memset(initial, 0, sizeof(initial));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(initial), initial);
    glBindBuffer(GL_DISPATCH_INDIRECT_BUFFER, indirect_buffer);
    glUseProgram(indirect_program);
    glDispatchComputeIndirect(4 * sizeof(GLuint));
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    memset(result, 0, sizeof(result));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(result), result);
    for (GLuint i = 0; i < 8; ++i) {
        CHECK(result[i] == 0x51a70000u + i * 17u, "indirect SSBO result/readback");
    }

    GLuint texture = 0;
    const GLubyte zero_pixels[16] = {0};
    const GLubyte expected_pixels[16] = {
        255, 0, 0, 255, 0, 255, 0, 255,
        0, 0, 255, 255, 255, 255, 255, 255
    };
    GLubyte pixels[16] = {0};
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 4, 1, 0, GL_RGBA, GL_UNSIGNED_BYTE, zero_pixels);
    /* One level with the default GL_NEAREST_MIPMAP_LINEAR filter is a
       mipmap-*incomplete* texture, and glCopyImageSubData below requires a
       complete one -- "GL_INVALID_OPERATION in glCopyImageSubData(srcName
       incomplete)", which left the readback all zeros on every driver and
       stopped this probe before the seven cases after it. */
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glBindImageTexture(1, texture, 0, GL_FALSE, 0, GL_WRITE_ONLY, GL_RGBA8);
    glUseProgram(image_program);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT | GL_TEXTURE_UPDATE_BARRIER_BIT);
    GLuint copied_texture = 0;
    glGenTextures(1, &copied_texture);
    glBindTexture(GL_TEXTURE_2D, copied_texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 4, 1, 0, GL_RGBA, GL_UNSIGNED_BYTE, zero_pixels);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glCopyImageSubData(texture, GL_TEXTURE_2D, 0, 0, 0, 0,
                       copied_texture, GL_TEXTURE_2D, 0, 0, 0, 0, 4, 1, 1);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT | GL_FRAMEBUFFER_BARRIER_BIT);
    GLuint framebuffer = 0;
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, copied_texture, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "storage image readback framebuffer");
    glReadPixels(0, 0, 4, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
    if (memcmp(pixels, expected_pixels, sizeof(pixels)) != 0) {
        fprintf(stderr, "storage image pixels:");
        for (unsigned i = 0; i < sizeof(pixels); ++i) fprintf(stderr, " %u", pixels[i]);
        fputc('\n', stderr);
    }
    CHECK(memcmp(pixels, expected_pixels, sizeof(pixels)) == 0, "storage image result/readback");

    const GLubyte copy_zero[24] = {0};
    GLubyte copy_result[24] = {0};
    GLubyte copy_expected[24] = {0};
    memcpy(copy_expected + 2 * 4, expected_pixels + 1 * 4, 2 * 4);
    GLuint framebuffer_copy = 0;
    glGenTextures(1, &framebuffer_copy);
    glBindTexture(GL_TEXTURE_2D, framebuffer_copy);
    /* Complete, or st drops it: see the note at the first image texture. */
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 6, 1, 0,
                 GL_RGBA, GL_UNSIGNED_BYTE, copy_zero);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, framebuffer);
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    glCopyTexSubImage2D(GL_TEXTURE_2D, 0, 2, 0, 1, 0, 2, 1);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
    GLuint copy_framebuffer = 0;
    glGenFramebuffers(1, &copy_framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, copy_framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, framebuffer_copy, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "subimage copy readback framebuffer");
    glReadPixels(0, 0, 6, 1, GL_RGBA, GL_UNSIGNED_BYTE, copy_result);
    CHECK(memcmp(copy_result, copy_expected, sizeof(copy_result)) == 0,
          "framebuffer to texture subimage copy");

    GLuint framebuffer_image_copy = 0;
    GLubyte image_copy_result[16] = {0};
    glGenTextures(1, &framebuffer_image_copy);
    glBindTexture(GL_TEXTURE_2D, framebuffer_image_copy);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, framebuffer);
    glReadBuffer(GL_COLOR_ATTACHMENT0);
    glCopyTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 0, 0, 4, 1, 0);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
    glBindFramebuffer(GL_FRAMEBUFFER, copy_framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, framebuffer_image_copy, 0);
    CHECK(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE,
          "image copy readback framebuffer");
    glReadPixels(0, 0, 4, 1, GL_RGBA, GL_UNSIGNED_BYTE, image_copy_result);
    CHECK(memcmp(image_copy_result, expected_pixels, sizeof(image_copy_result)) == 0,
          "framebuffer to newly allocated texture image copy");

    GLuint uint_texture = 0;
    const GLuint uint_zero[4] = {0};
    const GLuint uint_zero_level0[16] = {0};
    glGenTextures(1, &uint_texture);
    glBindTexture(GL_TEXTURE_2D, uint_texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_R32UI, 8, 2, 0,
                 GL_RED_INTEGER, GL_UNSIGNED_INT, uint_zero_level0);
    glTexImage2D(GL_TEXTURE_2D, 1, GL_R32UI, 4, 1, 0,
                 GL_RED_INTEGER, GL_UNSIGNED_INT, uint_zero);
    /* Levels 0 and 1 of an 8x2 texture are not the whole chain, so with the
       default GL_NEAREST_MIPMAP_LINEAR filter this is mipmap-incomplete --
       and st_convert_image_from_unit drops an incomplete texture, handing the
       driver an empty image view with nothing to say why. */
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL, 1);
    glBindImageTexture(4, uint_texture, 1, GL_FALSE, 0, GL_WRITE_ONLY, GL_R32UI);
    glUseProgram(uint_image_writer);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);

    GLuint image_readback_buffer = 0;
    glGenBuffers(1, &image_readback_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, image_readback_buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(uint_zero), uint_zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 6, image_readback_buffer);
    glBindImageTexture(4, uint_texture, 1, GL_FALSE, 0, GL_READ_ONLY, GL_R32UI);
    glUseProgram(uint_image_reader);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    GLuint uint_result[4] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, image_readback_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(uint_result), uint_result);
    for (GLuint i = 0; i < 4; ++i) {
        CHECK(uint_result[i] == 0x1200u + i * 19u,
              "uint storage image load/store at nonzero mip");
    }

    GLuint array_texture = 0;
    const GLuint array_zero[6] = {0};
    glGenTextures(1, &array_texture);
    glBindTexture(GL_TEXTURE_2D_ARRAY, array_texture);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D_ARRAY, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage3D(GL_TEXTURE_2D_ARRAY, 0, GL_R32UI, 2, 1, 3, 0,
                 GL_RED_INTEGER, GL_UNSIGNED_INT, array_zero);
    glBindImageTexture(7, array_texture, 0, GL_TRUE, 0, GL_WRITE_ONLY, GL_R32UI);
    glUseProgram(array_image_writer);
    glDispatchCompute(1, 1, 3);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);

    GLuint array_readback_buffer = 0;
    glGenBuffers(1, &array_readback_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, array_readback_buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(array_zero), array_zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 5, array_readback_buffer);
    glBindImageTexture(7, array_texture, 0, GL_TRUE, 0, GL_READ_ONLY, GL_R32UI);
    glUseProgram(array_image_reader);
    glDispatchCompute(1, 1, 3);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    GLuint array_result[6] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, array_readback_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(array_result), array_result);
    for (GLuint layer = 0; layer < 3; ++layer) {
        for (GLuint x = 0; x < 2; ++x) {
            CHECK(array_result[layer * 2 + x] == 0x3300u + layer * 101u + x * 7u,
                  "layered uint storage image load/store");
        }
    }

    const GLuint selected_zero[2] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, array_readback_buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(selected_zero), selected_zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 5, array_readback_buffer);
    glBindImageTexture(0, array_texture, 0, GL_FALSE, 1, GL_WRITE_ONLY, GL_R32UI);
    glUseProgram(array_layer_writer);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);
    glBindImageTexture(0, array_texture, 0, GL_FALSE, 1, GL_READ_ONLY, GL_R32UI);
    glUseProgram(array_layer_reader);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    GLuint selected_result[2] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, array_readback_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(selected_result), selected_result);
    for (GLuint x = 0; x < 2; ++x) {
        CHECK(selected_result[x] == 0x4400u + x * 13u,
              "selected 2D-array layer as non-layered image2D");
    }

    GLuint volume_texture = 0;
    const GLuint volume_zero[6] = {0};
    const GLuint volume_initial[6] = {0x6600u, 0x6605u, 0x6610u, 0x6615u, 0x6620u, 0x6625u};
    glGenTextures(1, &volume_texture);
    glBindTexture(GL_TEXTURE_3D, volume_texture);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_3D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage3D(GL_TEXTURE_3D, 0, GL_R32UI, 2, 1, 3, 0,
                 GL_RED_INTEGER, GL_UNSIGNED_INT, volume_initial);

    GLuint volume_readback_buffer = 0;
    glGenBuffers(1, &volume_readback_buffer);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, volume_readback_buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(volume_zero), volume_zero, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 4, volume_readback_buffer);
    glBindImageTexture(2, volume_texture, 0, GL_TRUE, 0, GL_READ_ONLY, GL_R32UI);
    glUseProgram(image3d_reader);
    glDispatchCompute(1, 1, 3);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    GLuint volume_result[6] = {0};
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, volume_readback_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(volume_result), volume_result);
    CHECK(memcmp(volume_result, volume_initial, sizeof(volume_result)) == 0,
          "3D texture CPU upload/imageLoad");

    glBindImageTexture(2, volume_texture, 0, GL_TRUE, 0, GL_WRITE_ONLY, GL_R32UI);
    glUseProgram(image3d_writer);
    glDispatchCompute(1, 1, 3);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);
    glBindImageTexture(2, volume_texture, 0, GL_TRUE, 0, GL_READ_ONLY, GL_R32UI);
    glUseProgram(image3d_reader);
    glDispatchCompute(1, 1, 3);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    memset(volume_result, 0, sizeof(volume_result));
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, volume_readback_buffer);
    glGetBufferSubData(GL_SHADER_STORAGE_BUFFER, 0, sizeof(volume_result), volume_result);
    for (GLuint z = 0; z < 3; ++z) {
        for (GLuint x = 0; x < 2; ++x) {
            CHECK(volume_result[z * 2 + x] == 0x5500u + z * 23u + x * 3u,
                  "3D storage image load/store");
        }
    }
    CHECK(glGetError() == GL_NO_ERROR, "unexpected GL error");
    puts("gl44 compute image load/store/copy formats ok");
    return 0;
}
