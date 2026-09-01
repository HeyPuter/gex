/* ARB_texture_cube_map_array: samplerCubeArray sampling.
 *
 * A cube array is how point-light shadow maps and reflection probes are
 * stored -- one cube per light, indexed by a fourth coordinate -- so it is
 * the cube-map extension real renderers actually need. WebGPU has the
 * `cube-array` view dimension and WGSL has texture_cube_array, and this
 * driver already routes PIPE_TEXTURE_CUBE_ARRAY through view creation,
 * resource creation and the sampler-view path; only the cap st reads was
 * missing.
 *
 * Every one of the twelve face-layer slices gets a distinct colour, so a
 * lookup that picks the right face of the wrong layer -- or transposes the
 * two, which is the mistake a flat 2D-array view would make -- fails rather
 * than passing by luck. The faces are 1x1 so filtering cannot blur the
 * answer, and each direction is exactly a face axis.
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

static const char *vertex_source =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";

static const char *fragment_source =
    "#version 330 core\n"
    "#extension GL_ARB_texture_cube_map_array : require\n"
    "uniform samplerCubeArray tex;\n"
    "uniform vec3 dir;\n"
    "uniform float layer;\n"
    "out vec4 fragColour;\n"
    "void main() { fragColour = texture(tex, vec4(dir, layer)); }\n";

static GLuint compile(GLenum stage, const char *source) {
    GLuint shader = glCreateShader(stage);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024] = {0};
        glGetShaderInfoLog(shader, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO shader compile failed: %s\n", log);
        return 0;
    }
    return shader;
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

    /* The extension has to be advertised before the shader that requires it,
     * and a core context only answers glGetStringi. */
    GLint extension_count = 0;
    int advertised = 0;
    glGetIntegerv(GL_NUM_EXTENSIONS, &extension_count);
    for (GLint i = 0; i < extension_count; ++i) {
        const char *name = (const char *)glGetStringi(GL_EXTENSIONS,
                                                      (GLuint)i);
        if (name && strcmp(name, "GL_ARB_texture_cube_map_array") == 0)
            advertised = 1;
    }
    fprintf(stderr, "INFO advertised=%d extensions=%d\n", advertised,
            extension_count);
    result("cube_array_advertised", advertised);

    GLuint colour = 0, framebuffer = 0;
    glGenTextures(1, &colour);
    glBindTexture(GL_TEXTURE_2D, colour);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, 2, 2, 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           colour, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "INFO framebuffer incomplete\n");
        return 2;
    }
    glViewport(0, 0, 2, 2);

    GLuint vao = 0;
    glGenVertexArrays(1, &vao);
    glBindVertexArray(vao);
    static const float positions[12] = {
        -1.0f, -1.0f,  1.0f, -1.0f,  -1.0f, 1.0f,
         1.0f, -1.0f,  1.0f,  1.0f,  -1.0f, 1.0f,
    };
    GLuint position_buffer = 0;
    glGenBuffers(1, &position_buffer);
    glBindBuffer(GL_ARRAY_BUFFER, position_buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof(positions), positions, GL_STATIC_DRAW);
    glEnableVertexAttribArray(0);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, NULL);

    /* Two layers of six 1x1 faces, slice index = layer * 6 + face. */
    enum { LAYERS = 2, FACES = 6, SLICES = LAYERS * FACES };
    unsigned char texels[SLICES * 4];
    for (int slice = 0; slice < SLICES; ++slice) {
        const int layer = slice / FACES, face = slice % FACES;
        texels[slice * 4 + 0] = (unsigned char)(10 + face * 40);
        texels[slice * 4 + 1] = (unsigned char)(20 + layer * 100);
        texels[slice * 4 + 2] = (unsigned char)(200 - face * 30);
        texels[slice * 4 + 3] = 255;
    }
    GLuint texture = 0;
    glGenTextures(1, &texture);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_CUBE_MAP_ARRAY, texture);
    glTexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MIN_FILTER,
                    GL_NEAREST);
    glTexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MAG_FILTER,
                    GL_NEAREST);
    glTexParameteri(GL_TEXTURE_CUBE_MAP_ARRAY, GL_TEXTURE_MAX_LEVEL, 0);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage3D(GL_TEXTURE_CUBE_MAP_ARRAY, 0, GL_RGBA8, 1, 1, SLICES, 0,
                 GL_RGBA, GL_UNSIGNED_BYTE, texels);
    const GLenum upload_error = glGetError();
    if (upload_error != GL_NO_ERROR) {
        fprintf(stderr, "INFO cube array upload err=0x%04x\n",
                (unsigned)upload_error);
        result("cube_array_upload", 0);
    } else {
        result("cube_array_upload", 1);
    }

    GLuint vertex = compile(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = compile(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) {
        result("cube_array_sample", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[512] = {0};
        glGetProgramInfoLog(program, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link failed: %s\n", log);
        result("cube_array_sample", 0);
        printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
               cases - passed);
        return 1;
    }
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "tex"), 0);
    const GLint dir_location = glGetUniformLocation(program, "dir");
    const GLint layer_location = glGetUniformLocation(program, "layer");

    static const float face_directions[FACES][3] = {
        { 1.0f,  0.0f,  0.0f}, {-1.0f,  0.0f,  0.0f},
        { 0.0f,  1.0f,  0.0f}, { 0.0f, -1.0f,  0.0f},
        { 0.0f,  0.0f,  1.0f}, { 0.0f,  0.0f, -1.0f},
    };
    static const char *face_names[FACES] = {
        "posx", "negx", "posy", "negy", "posz", "negz",
    };

    int mismatches = 0;
    for (int layer = 0; layer < LAYERS; ++layer) {
        for (int face = 0; face < FACES; ++face) {
            glUniform3f(dir_location, face_directions[face][0],
                        face_directions[face][1], face_directions[face][2]);
            glUniform1f(layer_location, (float)layer);
            glClearColor(0.0f, 0.0f, 0.0f, 1.0f);
            glClear(GL_COLOR_BUFFER_BIT);
            glDrawArrays(GL_TRIANGLES, 0, 6);

            unsigned char got[4] = {0xAB, 0xAB, 0xAB, 0xAB};
            glReadPixels(1, 1, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
            const GLenum error = glGetError();
            const int slice = layer * FACES + face;
            int ok = error == GL_NO_ERROR;
            for (int c = 0; c < 4; ++c) {
                const int delta = (int)got[c] - (int)texels[slice * 4 + c];
                if (delta > 2 || delta < -2) ok = 0;
            }
            if (!ok) {
                ++mismatches;
                fprintf(stderr,
                        "INFO layer%d.%s got=%u,%u,%u,%u want=%u,%u,%u,%u "
                        "err=0x%04x\n", layer, face_names[face], got[0],
                        got[1], got[2], got[3], texels[slice * 4 + 0],
                        texels[slice * 4 + 1], texels[slice * 4 + 2],
                        texels[slice * 4 + 3], (unsigned)error);
            }
        }
    }
    fprintf(stderr, "INFO cube_array_sample slices=%d mismatches=%d\n",
            SLICES, mismatches);
    result("cube_array_sample", mismatches == 0);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
