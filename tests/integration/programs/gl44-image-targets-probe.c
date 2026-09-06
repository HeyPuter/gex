/* Image store and load against the targets no other probe covers.
 *
 * WGSL has four storage texture shapes -- 1d, 2d, 2d-array and 3d -- and GL
 * has more, so this driver rewrites the rest onto them. The existing probes
 * cover image1D, image1DArray, image2D, image2DRect and imageCube; **3D,
 * 2DArray and CubeArray were untested**, and they are the three shapes whose
 * coordinate has a component the rewrite has to place rather than copy.
 *
 * KHR-GL46.shader_image_load_store's `basic-allTargets-*` group is ten cases
 * over exactly these eight targets, and it fails on values rather than on a
 * refusal -- every image is collected without complaint. So the question is
 * which target's store lands in the wrong place, and a probe that writes a
 * *distinct* value per target and per layer is what answers it.
 *
 * Each case stores a known value at a known coordinate and reads it back
 * through imageLoad in a second dispatch, so the readback goes through the
 * same rewrite as the store. Reading with glGetTexImage instead would test
 * the driver's texture path rather than its image path, and pass for the
 * wrong reason if the store went to the right texel of the wrong view.
 *
 * **All six cases pass, so this probe is coverage rather than a reproduction,
 * and what it rules out is the point.** The `basic-allTargets-*` failures are
 * not the 3D, 2D-array or cube-array rewrite, and not the packed path either:
 * both halves here agree with llvmpipe, including the slice a store lands on,
 * which is the thing a coordinate rewrite gets wrong when it gets anything
 * wrong. Whatever those ten cases are failing on is elsewhere -- the
 * remaining differences from them are that they declare all *eight* targets
 * in one shader, and that they run in the vertex, geometry and tessellation
 * stages as well as compute. Neither is tested here.
 *
 * Also worth knowing before chasing them: every image in those cases is
 * collected without a refusal (`GEX_WGPU_IMAGE_TRACE` shows all eight), so
 * they fail on values, and the CTS log carries no message beyond "Fail".
 */
#define GL_GLEXT_PROTOTYPES 1
#include <EGL/egl.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <stdio.h>

static int cases, passed;
static void result(const char *name, int ok) {
    ++cases; if (ok) ++passed;
    printf("CASE %s %s\n", name, ok ? "PASS" : "FAIL");
}

/* One dispatch stores into every target, a second reads them all back into a
 * flat 2D result image, one texel per value. */
static const char *cs_src =
    "#version 430 core\n"
    "layout(local_size_x = 1) in;\n"
    "layout(r32ui) uniform uimage3D img3d;\n"
    "layout(r32ui) uniform uimage2DArray img2darray;\n"
    "layout(r32ui) uniform uimageCubeArray imgcubearray;\n"
    "layout(r32ui) writeonly uniform uimage2D result;\n"
    "uniform int pass_index;\n"
    "void main() {\n"
    "  if (pass_index == 0) {\n"
    /* A different value per slice, so a store that lands on the wrong slice
     * is visible rather than merely absent. */
    "    imageStore(img3d, ivec3(0, 0, 0), uvec4(11u));\n"
    "    imageStore(img3d, ivec3(0, 0, 1), uvec4(12u));\n"
    "    imageStore(img2darray, ivec3(0, 0, 0), uvec4(21u));\n"
    "    imageStore(img2darray, ivec3(0, 0, 1), uvec4(22u));\n"
    /* A cube array's z is face + 6*layer; face 2 of layer 0, face 0 of 1. */
    "    imageStore(imgcubearray, ivec3(0, 0, 2), uvec4(31u));\n"
    "    imageStore(imgcubearray, ivec3(0, 0, 6), uvec4(32u));\n"
    "  } else {\n"
    "    imageStore(result, ivec2(0, 0), imageLoad(img3d, ivec3(0, 0, 0)));\n"
    "    imageStore(result, ivec2(1, 0), imageLoad(img3d, ivec3(0, 0, 1)));\n"
    "    imageStore(result, ivec2(2, 0),\n"
    "               imageLoad(img2darray, ivec3(0, 0, 0)));\n"
    "    imageStore(result, ivec2(3, 0),\n"
    "               imageLoad(img2darray, ivec3(0, 0, 1)));\n"
    "    imageStore(result, ivec2(4, 0),\n"
    "               imageLoad(imgcubearray, ivec3(0, 0, 2)));\n"
    "    imageStore(result, ivec2(5, 0),\n"
    "               imageLoad(imgcubearray, ivec3(0, 0, 6)));\n"
    "  }\n"
    "}\n";

/* The same three shapes at rgba32f with read-write access. WebGPU grants a
 * read_write storage texture only for the r32 formats, so this one cannot be
 * a storage texture at all and goes through the packed storage buffer the
 * shader indexes itself -- a different path from the r32ui cases above, and
 * the one KHR-GL46's basic-allTargets-* group actually exercises. */
static const char *cs_packed_src =
    "#version 430 core\n"
    "layout(local_size_x = 1) in;\n"
    "layout(rgba32f) uniform image3D img3d;\n"
    "layout(rgba32f) uniform image2DArray img2darray;\n"
    "layout(rgba32f) uniform imageCubeArray imgcubearray;\n"
    "layout(r32ui) writeonly uniform uimage2D result;\n"
    "uniform int pass_index;\n"
    "void main() {\n"
    "  if (pass_index == 0) {\n"
    "    imageStore(img3d, ivec3(0, 0, 0), vec4(11.0, 0.0, 0.0, 0.0));\n"
    "    imageStore(img3d, ivec3(0, 0, 1), vec4(12.0, 0.0, 0.0, 0.0));\n"
    "    imageStore(img2darray, ivec3(0, 0, 0), vec4(21.0, 0.0, 0.0, 0.0));\n"
    "    imageStore(img2darray, ivec3(0, 0, 1), vec4(22.0, 0.0, 0.0, 0.0));\n"
    "    imageStore(imgcubearray, ivec3(0, 0, 2), vec4(31.0, 0.0, 0.0, 0.0));\n"
    "    imageStore(imgcubearray, ivec3(0, 0, 6), vec4(32.0, 0.0, 0.0, 0.0));\n"
    "  } else {\n"
    "    imageStore(result, ivec2(0, 0),\n"
    "               uvec4(uint(imageLoad(img3d, ivec3(0, 0, 0)).r)));\n"
    "    imageStore(result, ivec2(1, 0),\n"
    "               uvec4(uint(imageLoad(img3d, ivec3(0, 0, 1)).r)));\n"
    "    imageStore(result, ivec2(2, 0),\n"
    "               uvec4(uint(imageLoad(img2darray, ivec3(0, 0, 0)).r)));\n"
    "    imageStore(result, ivec2(3, 0),\n"
    "               uvec4(uint(imageLoad(img2darray, ivec3(0, 0, 1)).r)));\n"
    "    imageStore(result, ivec2(4, 0),\n"
    "               uvec4(uint(imageLoad(imgcubearray, ivec3(0, 0, 2)).r)));\n"
    "    imageStore(result, ivec2(5, 0),\n"
    "               uvec4(uint(imageLoad(imgcubearray, ivec3(0, 0, 6)).r)));\n"
    "  }\n"
    "}\n";

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    EGLDisplay d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (d == EGL_NO_DISPLAY || !eglInitialize(d, NULL, NULL) ||
        !eglBindAPI(EGL_OPENGL_API)) return 2;
    const EGLint ca[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
                          EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT, EGL_NONE };
    EGLConfig cfg = NULL; EGLint n = 0;
    if (!eglChooseConfig(d, ca, &cfg, 1, &n) || n < 1) return 2;
    /* 4.3 for compute; imageCubeArray needs ARB_shader_image_load_store's
     * cube array support, core since 4.0. */
    const EGLint cx[] = { EGL_CONTEXT_MAJOR_VERSION, 4,
                          EGL_CONTEXT_MINOR_VERSION, 3, EGL_NONE };
    const EGLint sa[] = { EGL_WIDTH, 8, EGL_HEIGHT, 8, EGL_NONE };
    EGLContext c = eglCreateContext(d, cfg, EGL_NO_CONTEXT, cx);
    EGLSurface s = eglCreatePbufferSurface(d, cfg, sa);
    if (c == EGL_NO_CONTEXT || s == EGL_NO_SURFACE ||
        !eglMakeCurrent(d, s, s, c)) return 2;

    GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
    if (!sh) { printf("SUMMARY cases=0 passed=0 failed=0\n"); return 1; }
    glShaderSource(sh, 1, &cs_src, NULL);
    glCompileShader(sh);
    GLint ok = 0;
    glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1200] = {0};
        glGetShaderInfoLog(sh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO compile: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    GLuint prog = glCreateProgram();
    glAttachShader(prog, sh);
    glLinkProgram(prog);
    glGetProgramiv(prog, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[1200] = {0};
        glGetProgramInfoLog(prog, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO link: %s\n", log);
        printf("SUMMARY cases=0 passed=0 failed=0\n");
        return 1;
    }
    glUseProgram(prog);

    GLuint tex3d = 0, tex2da = 0, texca = 0, res = 0;
    glGenTextures(1, &tex3d);
    glBindTexture(GL_TEXTURE_3D, tex3d);
    glTexStorage3D(GL_TEXTURE_3D, 1, GL_R32UI, 1, 1, 2);
    glGenTextures(1, &tex2da);
    glBindTexture(GL_TEXTURE_2D_ARRAY, tex2da);
    glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_R32UI, 1, 1, 2);
    glGenTextures(1, &texca);
    glBindTexture(GL_TEXTURE_CUBE_MAP_ARRAY, texca);
    glTexStorage3D(GL_TEXTURE_CUBE_MAP_ARRAY, 1, GL_R32UI, 1, 1, 12);
    glGenTextures(1, &res);
    glBindTexture(GL_TEXTURE_2D, res);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_R32UI, 8, 1);

    glBindImageTexture(0, tex3d,  0, GL_TRUE, 0, GL_READ_WRITE, GL_R32UI);
    glBindImageTexture(1, tex2da, 0, GL_TRUE, 0, GL_READ_WRITE, GL_R32UI);
    glBindImageTexture(2, texca,  0, GL_TRUE, 0, GL_READ_WRITE, GL_R32UI);
    glBindImageTexture(3, res,    0, GL_FALSE, 0, GL_WRITE_ONLY, GL_R32UI);
    glUniform1i(glGetUniformLocation(prog, "img3d"), 0);
    glUniform1i(glGetUniformLocation(prog, "img2darray"), 1);
    glUniform1i(glGetUniformLocation(prog, "imgcubearray"), 2);
    glUniform1i(glGetUniformLocation(prog, "result"), 3);

    const GLint pass_loc = glGetUniformLocation(prog, "pass_index");
    glUniform1i(pass_loc, 0);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);
    glUniform1i(pass_loc, 1);
    glDispatchCompute(1, 1, 1);
    glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);

    GLuint got[8] = {0};
    glBindTexture(GL_TEXTURE_2D, res);
    glGetTexImage(GL_TEXTURE_2D, 0, GL_RED_INTEGER, GL_UNSIGNED_INT, got);
    const GLenum error = glGetError();
    fprintf(stderr, "INFO 3d=%u,%u 2darray=%u,%u cubearray=%u,%u err=0x%04x\n",
            got[0], got[1], got[2], got[3], got[4], got[5], (unsigned)error);

    const int no_error = error == GL_NO_ERROR;
    result("image3d",        no_error && got[0] == 11 && got[1] == 12);
    result("image2darray",   no_error && got[2] == 21 && got[3] == 22);
    result("imagecubearray", no_error && got[4] == 31 && got[5] == 32);

    /* A plain cube map, written through a layered image and read back one
     * face at a time with glGetTexImage.
     *
     * Everything above reads its result out of another image, which says the
     * store landed but nothing about whether the face can be read back by
     * name. KHR-GL44.shader_image_load_store.basic-allTargets-store does it
     * the second way and fails in the browser with
     * "GL_TEXTURE_CUBE_MAP_POSITIVE_X target failed. Value is: [0,0,0,0]",
     * which is [[zero-means-nobody-wrote]] and could be either half. */
    {
        static const char *cube_src =
            "#version 430 core\n"
            "layout(local_size_x = 1) in;\n"
            "layout(r32ui, binding = 0) uniform uimageCube img;\n"
            "void main() {\n"
            "  for (int face = 0; face < 6; ++face)\n"
            "    imageStore(img, ivec3(0, 0, face), uvec4(100u + uint(face)));\n"
            "}\n";
        GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
        glShaderSource(sh, 1, &cube_src, NULL);
        glCompileShader(sh);
        GLint ok = 0; glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        GLuint cprog = 0;
        if (ok) {
            cprog = glCreateProgram();
            glAttachShader(cprog, sh);
            glLinkProgram(cprog);
            GLint linked = 0; glGetProgramiv(cprog, GL_LINK_STATUS, &linked);
            if (!linked) {
                char log[1024] = {0};
                glGetProgramInfoLog(cprog, sizeof log, NULL, log);
                fprintf(stderr, "INFO cube link failed: %s\n", log);
                cprog = 0;
            }
        } else {
            char log[1024] = {0};
            glGetShaderInfoLog(sh, sizeof log, NULL, log);
            fprintf(stderr, "INFO cube compile failed: %s\n", log);
        }

        GLuint cube = 0;
        unsigned faces[6] = {0, 0, 0, 0, 0, 0};
        GLenum cube_error = GL_NO_ERROR;
        if (cprog) {
            glGenTextures(1, &cube);
            glBindTexture(GL_TEXTURE_CUBE_MAP, cube);
            glTexStorage2D(GL_TEXTURE_CUBE_MAP, 1, GL_R32UI, 1, 1);
            glBindImageTexture(0, cube, 0, GL_TRUE, 0, GL_WRITE_ONLY,
                               GL_R32UI);
            glUseProgram(cprog);
            glUniform1i(glGetUniformLocation(cprog, "img"), 0);
            glDispatchCompute(1, 1, 1);
            glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
            while (glGetError() != GL_NO_ERROR) {}
            glBindTexture(GL_TEXTURE_CUBE_MAP, cube);
            for (int face = 0; face < 6; ++face)
                glGetTexImage(GL_TEXTURE_CUBE_MAP_POSITIVE_X + face, 0,
                              GL_RED_INTEGER, GL_UNSIGNED_INT, &faces[face]);
            cube_error = glGetError();
        }
        fprintf(stderr, "INFO cube faces=%u,%u,%u,%u,%u,%u want=100..105 "
                "err=0x%04x\n", faces[0], faces[1], faces[2], faces[3],
                faces[4], faces[5], (unsigned)cube_error);
        int right = 0;
        for (int face = 0; face < 6; ++face)
            if (faces[face] == 100u + (unsigned)face) ++right;
        result("imagecube_face0", cprog != 0 && cube_error == GL_NO_ERROR &&
                                  faces[0] == 100u);
        result("imagecube_all_faces", cprog != 0 && right == 6);
        if (cube) glDeleteTextures(1, &cube);

        /* And the signed four-component form, which is the one the suite
         * actually uses: rgba32i, storing ivec4(-1, 2, 3, -4). r32ui above
         * passes, so if this does not, the shape is not the problem and the
         * format is. */
        static const char *cube_i_src =
            "#version 430 core\n"
            "layout(local_size_x = 1) in;\n"
            "layout(rgba32i, binding = 0) uniform iimageCube img;\n"
            "void main() {\n"
            "  for (int face = 0; face < 6; ++face)\n"
            "    imageStore(img, ivec3(0, 0, face), ivec4(-1, 2, 3, -4));\n"
            "}\n";
        GLuint ish = glCreateShader(GL_COMPUTE_SHADER);
        glShaderSource(ish, 1, &cube_i_src, NULL);
        glCompileShader(ish);
        GLint iok = 0; glGetShaderiv(ish, GL_COMPILE_STATUS, &iok);
        GLuint iprog = 0;
        if (iok) {
            iprog = glCreateProgram();
            glAttachShader(iprog, ish);
            glLinkProgram(iprog);
            GLint linked = 0; glGetProgramiv(iprog, GL_LINK_STATUS, &linked);
            if (!linked) {
                char log[1024] = {0};
                glGetProgramInfoLog(iprog, sizeof log, NULL, log);
                fprintf(stderr, "INFO cube_i link failed: %s\n", log);
                iprog = 0;
            }
        } else {
            char log[1024] = {0};
            glGetShaderInfoLog(ish, sizeof log, NULL, log);
            fprintf(stderr, "INFO cube_i compile failed: %s\n", log);
        }
        GLuint icube = 0;
        GLint face0[4] = {0, 0, 0, 0};
        GLenum ierror = GL_NO_ERROR;
        if (iprog) {
            glGenTextures(1, &icube);
            glBindTexture(GL_TEXTURE_CUBE_MAP, icube);
            /* Six glTexImage2D faces, not glTexStorage2D: mutable storage is
             * what the suite uses, and a cube map assembled face by face is
             * only complete once the sixth arrives. st drops an incomplete
             * texture into an empty image view without a word, so the
             * difference between the two ways of making the same texture is
             * the difference between a store landing and going nowhere. */
            glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MIN_FILTER,
                            GL_NEAREST);
            glTexParameteri(GL_TEXTURE_CUBE_MAP, GL_TEXTURE_MAG_FILTER,
                            GL_NEAREST);
            for (int face = 0; face < 6; ++face)
                glTexImage2D(GL_TEXTURE_CUBE_MAP_POSITIVE_X + face, 0,
                             GL_RGBA32I, 1, 1, 0, GL_RGBA_INTEGER, GL_INT,
                             NULL);
            glBindImageTexture(0, icube, 0, GL_TRUE, 0, GL_WRITE_ONLY,
                               GL_RGBA32I);
            glUseProgram(iprog);
            glUniform1i(glGetUniformLocation(iprog, "img"), 0);
            glDispatchCompute(1, 1, 1);
            glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
            while (glGetError() != GL_NO_ERROR) {}
            glBindTexture(GL_TEXTURE_CUBE_MAP, icube);
            glGetTexImage(GL_TEXTURE_CUBE_MAP_POSITIVE_X, 0, GL_RGBA_INTEGER,
                          GL_INT, face0);
            ierror = glGetError();
        }
        fprintf(stderr, "INFO cube_i +X=[%d,%d,%d,%d] want=[-1,2,3,-4] "
                "err=0x%04x\n", face0[0], face0[1], face0[2], face0[3],
                (unsigned)ierror);
        result("imagecube_rgba32i", iprog != 0 && ierror == GL_NO_ERROR &&
                                    face0[0] == -1 && face0[1] == 2 &&
                                    face0[2] == 3 && face0[3] == -4);
        if (icube) glDeleteTextures(1, &icube);
        glUseProgram(prog);
        while (glGetError() != GL_NO_ERROR) {}
    }

    /* And the same shapes at rgba32f read-write, which cannot be a storage
     * texture in WebGPU and goes through the packed path instead. */
    GLuint psh = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(psh, 1, &cs_packed_src, NULL);
    glCompileShader(psh);
    GLint pok = 0;
    glGetShaderiv(psh, GL_COMPILE_STATUS, &pok);
    GLuint pprog = 0;
    if (pok) {
        pprog = glCreateProgram();
        glAttachShader(pprog, psh);
        glLinkProgram(pprog);
        glGetProgramiv(pprog, GL_LINK_STATUS, &pok);
        if (!pok) {
            char log[1200] = {0};
            glGetProgramInfoLog(pprog, sizeof(log) - 1, NULL, log);
            fprintf(stderr, "INFO packed link: %s\n", log);
        }
    } else {
        char log[1200] = {0};
        glGetShaderInfoLog(psh, sizeof(log) - 1, NULL, log);
        fprintf(stderr, "INFO packed compile: %s\n", log);
    }
    GLuint pgot[8] = {0};
    GLenum perror = GL_INVALID_OPERATION;
    if (pok) {
        GLuint f3d = 0, f2da = 0, fca = 0;
        glGenTextures(1, &f3d);
        glBindTexture(GL_TEXTURE_3D, f3d);
        glTexStorage3D(GL_TEXTURE_3D, 1, GL_RGBA32F, 1, 1, 2);
        glGenTextures(1, &f2da);
        glBindTexture(GL_TEXTURE_2D_ARRAY, f2da);
        glTexStorage3D(GL_TEXTURE_2D_ARRAY, 1, GL_RGBA32F, 1, 1, 2);
        glGenTextures(1, &fca);
        glBindTexture(GL_TEXTURE_CUBE_MAP_ARRAY, fca);
        glTexStorage3D(GL_TEXTURE_CUBE_MAP_ARRAY, 1, GL_RGBA32F, 1, 1, 12);
        glUseProgram(pprog);
        glBindImageTexture(0, f3d,  0, GL_TRUE, 0, GL_READ_WRITE, GL_RGBA32F);
        glBindImageTexture(1, f2da, 0, GL_TRUE, 0, GL_READ_WRITE, GL_RGBA32F);
        glBindImageTexture(2, fca,  0, GL_TRUE, 0, GL_READ_WRITE, GL_RGBA32F);
        glBindImageTexture(3, res,  0, GL_FALSE, 0, GL_WRITE_ONLY, GL_R32UI);
        glUniform1i(glGetUniformLocation(pprog, "img3d"), 0);
        glUniform1i(glGetUniformLocation(pprog, "img2darray"), 1);
        glUniform1i(glGetUniformLocation(pprog, "imgcubearray"), 2);
        glUniform1i(glGetUniformLocation(pprog, "result"), 3);
        const GLint ploc = glGetUniformLocation(pprog, "pass_index");
        glUniform1i(ploc, 0);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_SHADER_IMAGE_ACCESS_BARRIER_BIT);
        glUniform1i(ploc, 1);
        glDispatchCompute(1, 1, 1);
        glMemoryBarrier(GL_TEXTURE_UPDATE_BARRIER_BIT);
        glBindTexture(GL_TEXTURE_2D, res);
        glGetTexImage(GL_TEXTURE_2D, 0, GL_RED_INTEGER, GL_UNSIGNED_INT, pgot);
        perror = glGetError();
    }
    fprintf(stderr, "INFO packed 3d=%u,%u 2darray=%u,%u cubearray=%u,%u "
            "err=0x%04x\n", pgot[0], pgot[1], pgot[2], pgot[3], pgot[4],
            pgot[5], (unsigned)perror);
    const int pno_error = pok && perror == GL_NO_ERROR;
    result("packed_image3d",        pno_error && pgot[0] == 11 && pgot[1] == 12);
    result("packed_image2darray",   pno_error && pgot[2] == 21 && pgot[3] == 22);
    result("packed_imagecubearray", pno_error && pgot[4] == 31 && pgot[5] == 32);

    printf("SUMMARY cases=%d passed=%d failed=%d\n", cases, passed,
           cases - passed);
    return cases == passed ? 0 : 1;
}
