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

    /* Fill a buffer by name and draw from it. Everything before this only
     * *configured* objects; this is the first case that would fail if the
     * bytes never arrived, which is what makes it the one worth having --
     * without glNamedBufferData an engine on the 4.5 path stops at its first
     * vertex upload however much of the object model works. */
    static const float triangle[] = {
        -1.0f, -1.0f, 0.0f,   3.0f, -1.0f, 0.0f,   -1.0f, 3.0f, 0.0f,
    };
    glNamedBufferData(buffer, (GLsizeiptr)sizeof triangle, triangle,
                      GL_STATIC_DRAW);
    GLint stored_size = 0;
    glGetNamedBufferParameteriv(buffer, GL_BUFFER_SIZE, &stored_size);
    {
        char detail[64];
        snprintf(detail, sizeof detail, "buffer holds %d bytes", stored_size);
        report("named_buffer_data",
               stored_size == (GLint)sizeof triangle && glGetError() == GL_NO_ERROR,
               detail);
    }

    glVertexArrayVertexBuffer(vao, 0, buffer, 0, 3 * (GLsizei)sizeof(float));
    report("vertex_array_vertex_buffer", glGetError() == GL_NO_ERROR, "");

    /* And the bytes really are the ones sent: read a corner back. */
    glNamedBufferSubData(buffer, 0, sizeof(float), &(const float){-0.5f});
    report("named_buffer_sub_data", glGetError() == GL_NO_ERROR, "");

    /* A draw that covers the framebuffer, sourced entirely from objects
     * configured without ever binding them except the VAO the draw needs. */
    const char* vs_source =
        "#version 450\n"
        "layout(location = 0) in vec3 position;\n"
        "void main() { gl_Position = vec4(position, 1.0); }\n";
    const char* fs_source =
        "#version 450\n"
        "out vec4 colour;\n"
        "void main() { colour = vec4(0.0, 1.0, 0.0, 1.0); }\n";
    GLuint vs = glCreateShader(GL_VERTEX_SHADER);
    GLuint fs = glCreateShader(GL_FRAGMENT_SHADER);
    glShaderSource(vs, 1, &vs_source, NULL);
    glShaderSource(fs, 1, &fs_source, NULL);
    glCompileShader(vs);
    glCompileShader(fs);
    GLuint program = glCreateProgram();
    glAttachShader(program, vs);
    glAttachShader(program, fs);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    glUseProgram(program);
    glBindVertexArray(vao);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glViewport(0, 0, 4, 4);
    const GLfloat black[4] = {0.0f, 0.0f, 0.0f, 1.0f};
    glClearBufferfv(GL_COLOR, 0, black);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    unsigned char drawn[4] = {0, 0, 0, 0};
    glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, drawn);
    {
        char detail[96];
        snprintf(detail, sizeof detail, "linked=%d pixel %u,%u,%u,%u", linked,
                 drawn[0], drawn[1], drawn[2], drawn[3]);
        report("draw_from_named_buffer",
               linked && drawn[1] > 200 && drawn[0] < 60 && drawn[2] < 60, detail);
    }

    /* Texture data by name, and read back. Everything above proved the object
     * model and the buffer path; this is the one that fails if pixels never
     * arrive -- and it is what an engine does immediately after
     * glTextureStorage2D. Read through a framebuffer because glGetTexImage has
     * no bearing on whether the *upload* worked and would mask a bad path. */
    GLuint uploaded = 0, upload_fbo = 0;
    glCreateTextures(GL_TEXTURE_2D, 1, &uploaded);
    glTextureStorage2D(uploaded, 1, GL_RGBA8, 2, 2);
    static const unsigned char texels[16] = {
        255, 0, 0, 255,     0, 255, 0, 255,
        0, 0, 255, 255,     255, 255, 0, 255,
    };
    glTextureSubImage2D(uploaded, 0, 0, 0, 2, 2, GL_RGBA, GL_UNSIGNED_BYTE,
                        texels);
    glCreateFramebuffers(1, &upload_fbo);
    glNamedFramebufferTexture(upload_fbo, GL_COLOR_ATTACHMENT0, uploaded, 0);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, upload_fbo);
    unsigned char got[4] = {0, 0, 0, 0};
    glReadPixels(1, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, got);
    {
        char detail[96];
        snprintf(detail, sizeof detail, "texel(1,0) = %u,%u,%u,%u", got[0],
                 got[1], got[2], got[3]);
        report("texture_sub_image_by_name",
               got[0] == 0 && got[1] == 255 && got[2] == 0 && got[3] == 255,
               detail);
    }

    /* The two round-trip calls: both flush the ring and wait for an answer,
     * so a wrong reply length or a missed lookup shows up as a wrong value
     * here rather than as a silent no-op. */
    {
        const GLenum status = glCheckNamedFramebufferStatus(upload_fbo,
                                                            GL_FRAMEBUFFER);
        char detail[64];
        snprintf(detail, sizeof detail, "status 0x%x", (unsigned)status);
        report("check_named_framebuffer_status",
               status == GL_FRAMEBUFFER_COMPLETE, detail);
    }
    {
        /* Read the vertex data back out of the buffer it was uploaded into.
         * The first float was overwritten by glNamedBufferSubData above, so
         * this also confirms that landed rather than merely not erroring. */
        float readback[3] = {0.0f, 0.0f, 0.0f};
        glGetNamedBufferSubData(buffer, 0, (GLsizeiptr)sizeof readback,
                                readback);
        char detail[96];
        snprintf(detail, sizeof detail, "read %.2f,%.2f,%.2f", readback[0],
                 readback[1], readback[2]);
        report("get_named_buffer_sub_data",
               readback[0] == -0.5f && readback[1] == -1.0f &&
                   readback[2] == 0.0f, detail);
    }
    /* And a clear addressed by name, read back through the same framebuffer. */
    {
        const GLfloat magenta[4] = {1.0f, 0.0f, 1.0f, 1.0f};
        glClearNamedFramebufferfv(upload_fbo, GL_COLOR, 0, magenta);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, upload_fbo);
        unsigned char cleared[4] = {0, 0, 0, 0};
        glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, cleared);
        char detail[96];
        snprintf(detail, sizeof detail, "read %u,%u,%u,%u", cleared[0],
                 cleared[1], cleared[2], cleared[3]);
        report("clear_named_framebuffer_fv",
               cleared[0] > 200 && cleared[1] < 60 && cleared[2] > 200, detail);
    }

    /* Map by name, write through the mapping, unmap, and read it back with a
     * different call. A GL mapping here is a shadow this library owns, so the
     * bytes only reach the buffer on unmap -- if that path were missing the
     * write would vanish and no error would say so. */
    {
        GLuint mapped = 0;
        glCreateBuffers(1, &mapped);
        static const float initial[4] = {1.0f, 2.0f, 3.0f, 4.0f};
        glNamedBufferData(mapped, (GLsizeiptr)sizeof initial, initial,
                          GL_DYNAMIC_DRAW);
        float* view = (float*)glMapNamedBufferRange(
            mapped, 0, (GLsizeiptr)sizeof initial,
            GL_MAP_READ_BIT | GL_MAP_WRITE_BIT);
        const int mapped_ok = view != NULL;
        /* GL_MAP_READ_BIT means the mapping must already hold what the buffer
         * holds -- that is a round trip, and getting it wrong shows here. */
        const int read_through = mapped_ok && view[2] == 3.0f;
        if (mapped_ok) { view[1] = 42.0f; }
        const GLboolean unmapped = mapped_ok ? glUnmapNamedBuffer(mapped)
                                             : GL_FALSE;
        float after[4] = {0.0f, 0.0f, 0.0f, 0.0f};
        glGetNamedBufferSubData(mapped, 0, (GLsizeiptr)sizeof after, after);
        char detail[128];
        snprintf(detail, sizeof detail,
                 "mapped=%d read=%d unmapped=%d after=%.0f,%.0f,%.0f,%.0f",
                 mapped_ok, read_through, (int)unmapped, after[0], after[1],
                 after[2], after[3]);
        report("map_named_buffer_range",
               mapped_ok && read_through && unmapped && after[1] == 42.0f &&
                   after[0] == 1.0f && after[3] == 4.0f, detail);
    }

    /* The extension string itself. An engine takes the DSA path on the
     * version, but a library that asks by name only gets there if every one
     * of the 97 commands ARB_direct_state_access requires is exported --
     * gen_gl_extensions.py withholds the whole extension otherwise, so this
     * one line stands in for all of them. */
    {
        GLint extension_count = 0;
        int found = 0;
        glGetIntegerv(GL_NUM_EXTENSIONS, &extension_count);
        for (GLint i = 0; i < extension_count && !found; ++i) {
            const char* name = (const char*)glGetStringi(GL_EXTENSIONS, (GLuint)i);
            if (name && strcmp(name, "GL_ARB_direct_state_access") == 0) found = 1;
        }
        char detail[64];
        snprintf(detail, sizeof detail, "%d extensions advertised",
                 (int)extension_count);
        report("arb_direct_state_access_advertised", found, detail);
    }

    /* glVertexArrayVertexBuffers: three parallel arrays, sent interleaved. */
    {
        GLuint many = 0;
        glCreateVertexArrays(1, &many);
        const GLuint names[2] = {buffer, buffer};
        const GLintptr offsets[2] = {0, 12};
        const GLsizei strides[2] = {12, 12};
        glVertexArrayVertexBuffers(many, 0, 2, names, offsets, strides);
        const GLenum err = glGetError();
        GLint bound_stride = 0;
        glGetVertexArrayIndexediv(many, 1, GL_VERTEX_BINDING_STRIDE,
                                  &bound_stride);
        char detail[80];
        snprintf(detail, sizeof detail, "err=0x%x binding1 stride=%d",
                 (unsigned)err, bound_stride);
        report("vertex_array_vertex_buffers",
               err == GL_NO_ERROR && bound_stride == 12, detail);
    }

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, cases - failures,
           failures);
    return failures ? 1 : 0;
}
