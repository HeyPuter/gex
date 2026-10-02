/* The hand-written wire commands crates/gex-gl/src/host.rs does not decode
 * itself, for the native host: copied from the browser's decoder
 * (web/mesa/gexgl-mesa.cpp's `one`) by the list of opcodes host.rs lacks, so
 * a Linux guest's and opengl32's batches run the same on both hosts. Keep
 * them in step with that file. Included by gexgl-native.cpp, inside its
 * switch. */
    case DEPTH_MASK: glDepthMask(a.u32()); break;
    case HINT: { GLenum target=a.u32(), mode=a.u32(); glHint(target,mode); break; }
    case COLOR_MASK: { GLboolean r=a.u32(), g=a.u32(), b=a.u32(), alpha=a.u32(); glColorMask(r,g,b,alpha); break; }
    case BLEND_EQUATION: glBlendEquation(a.u32()); break;
    case BLEND_COLOR: { GLfloat r=a.f32(), g=a.f32(), b=a.f32(), alpha=a.f32(); glBlendColor(r,g,b,alpha); break; }
    case BLEND_EQUATION_SEPARATE: { GLenum rgb=a.u32(), alpha=a.u32(); glBlendEquationSeparate(rgb,alpha); break; }
    case CLEAR_BUFFER_FV: {
        GLenum b=a.u32(); GLint d=a.i32(); GLfloat v[4]; for(float& x:v)x=a.f32();
        glClearBufferfv(b,d,v);
        break;
    }
    case CLEAR_BUFFER_IV: {
        GLenum b=a.u32(); GLint d=a.i32(); GLint v[4]; for(int& x:v)x=a.i32();
        glClearBufferiv(b,d,v);
        break;
    }
    case CLEAR_BUFFER_UIV: {
        GLenum b=a.u32(); GLint d=a.i32(); GLuint v[4]; for(unsigned& x:v)x=a.u32();
        glClearBufferuiv(b,d,v);
        break;
    }
    case CLEAR_BUFFER_FI: {
        GLenum b=a.u32(); GLint d=a.i32(); GLfloat depth=a.f32(); GLint stencil=a.i32();
        glClearBufferfi(b,d,depth,stencil);
        break;
    }
    case STENCIL_FUNC: { GLenum func=a.u32(); GLint ref=a.i32(); GLuint mask=a.u32(); glStencilFunc(func,ref,mask); break; }
    case STENCIL_FUNC_SEPARATE: { GLenum face=a.u32(), func=a.u32(); GLint ref=a.i32(); GLuint mask=a.u32(); glStencilFuncSeparate(face,func,ref,mask); break; }
    case STENCIL_MASK: glStencilMask(a.u32()); break;
    case STENCIL_MASK_SEPARATE: { GLenum face=a.u32(); GLuint mask=a.u32(); glStencilMaskSeparate(face,mask); break; }
    case STENCIL_OP: { GLenum fail=a.u32(), zfail=a.u32(), zpass=a.u32(); glStencilOp(fail,zfail,zpass); break; }
    case STENCIL_OP_SEPARATE: { GLenum face=a.u32(), fail=a.u32(), zfail=a.u32(), zpass=a.u32(); glStencilOpSeparate(face,fail,zfail,zpass); break; }
    case POLYGON_OFFSET: { GLfloat factor=a.f32(), units=a.f32(); glPolygonOffset(factor,units); break; }
    case LINE_WIDTH: glLineWidth(a.f32()); break;
    case CLEAR_STENCIL: current_clear_stencil=a.i32(); glClearStencil(current_clear_stencil); break;
    case DEPTH_RANGE: { GLfloat near_value=a.f32(), far_value=a.f32(); glDepthRangef(near_value,far_value); break; }
    case GEN_QUERIES: {
        GLsizei n = a.i32();
        if (n < 0) { glGenQueries(n, nullptr); break; }
        std::vector<GLuint> ids(static_cast<size_t>(n)); glGenQueries(n, ids.data());
        for (GLsizei i = 0; i < n; ++i) queries[a.u32()] = ids[static_cast<size_t>(i)];
        break;
    }
    case DELETE_QUERIES: {
        GLsizei n = a.i32();
        if (n < 0) { glDeleteQueries(n, nullptr); break; }
        std::vector<GLuint> ids; ids.reserve(static_cast<size_t>(n));
        for (GLsizei i = 0; i < n; ++i) { uint32_t g=a.u32(); ids.push_back(lookup(queries,g)); queries.erase(g); }
        glDeleteQueries(n, ids.data()); break;
    }
    case IS_QUERY: put_u32(glIsQuery(lookup(queries, a.u32()))); break;
    case GEN_TRANSFORM_FEEDBACKS: {
        GLsizei n = a.i32();
        if (n < 0) { glGenTransformFeedbacks(n, nullptr); break; }
        std::vector<GLuint> ids(static_cast<size_t>(n)); glGenTransformFeedbacks(n, ids.data());
        for (GLsizei i = 0; i < n; ++i) transform_feedbacks[a.u32()] = ids[static_cast<size_t>(i)];
        break;
    }
    case DELETE_TRANSFORM_FEEDBACKS: {
        GLsizei n = a.i32();
        if (n < 0) { glDeleteTransformFeedbacks(n, nullptr); break; }
        std::vector<GLuint> ids; ids.reserve(static_cast<size_t>(n));
        for (GLsizei i = 0; i < n; ++i) { uint32_t g=a.u32(); ids.push_back(lookup(transform_feedbacks,g)); transform_feedbacks.erase(g); }
        glDeleteTransformFeedbacks(n, ids.data()); break;
    }
    case BIND_TRANSFORM_FEEDBACK: {
        GLenum target=a.u32(); GLuint id=lookup(transform_feedbacks,a.u32());
        glBindTransformFeedback(target,id); break;
    }
    case IS_TRANSFORM_FEEDBACK: put_u32(glIsTransformFeedback(lookup(transform_feedbacks,a.u32()))); break;
    case PAUSE_TRANSFORM_FEEDBACK: glPauseTransformFeedback(); break;
    case RESUME_TRANSFORM_FEEDBACK: glResumeTransformFeedback(); break;
    case BIND_BUFFER_BASE: {
        GLenum target = a.u32(); GLuint index = a.u32();
        glBindBufferBase(target, index, lookup(buffers, a.u32())); break;
    }
    case GEN_SAMPLERS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenSamplers(c, p); })) break;
        std::vector<GLuint> ids(n); glGenSamplers(n, ids.data());
        for (uint32_t i = 0; i < n; ++i) samplers[a.u32()] = ids[i]; break;
    }
    case DELETE_SAMPLERS: {
        uint32_t n = a.u32(); std::vector<GLuint> ids; ids.reserve(n);
        for (uint32_t i = 0; i < n; ++i) { uint32_t g=a.u32(); ids.push_back(lookup(samplers,g)); samplers.erase(g); }
        glDeleteSamplers(n, ids.data()); break;
    }
    case BIND_SAMPLER: { GLuint unit=a.u32(); glBindSampler(unit, lookup(samplers,a.u32())); break; }
    case SAMPLER_PARAMETER_I: {
        GLuint sampler = lookup(samplers, a.u32()); GLenum pname = a.u32();
        GLint values[4]{}; const size_t count = pname == GL_TEXTURE_BORDER_COLOR ? 4u : 1u;
        for (size_t i = 0; i < count; ++i) values[i] = a.i32();
        if (count == 4) glSamplerParameteriv(sampler, pname, values);
        else glSamplerParameteri(sampler, pname, values[0]);
        break;
    }
    case SAMPLER_PARAMETER_F: {
        GLuint sampler = lookup(samplers, a.u32()); GLenum pname = a.u32();
        GLfloat values[4]{}; const size_t count = pname == GL_TEXTURE_BORDER_COLOR ? 4u : 1u;
        for (size_t i = 0; i < count; ++i) values[i] = a.f32();
        if (count == 4) glSamplerParameterfv(sampler, pname, values);
        else glSamplerParameterf(sampler, pname, values[0]);
        break;
    }
    case DETACH_SHADER: { GLuint p = lookup(programs, a.u32()); glDetachShader(p, lookup(shaders, a.u32())); break; }
    case GET_UNIFORM_BLOCK_INDEX: { GLuint p = lookup(programs, a.u32()); std::string n = a.string(); put_u32(glGetUniformBlockIndex(p, n.c_str())); break; }
    case UNIFORM_BLOCK_BINDING: { GLuint p = lookup(programs, a.u32()); GLuint i = a.u32(); glUniformBlockBinding(p, i, a.u32()); break; }
    case GET_ATTACHED_SHADERS: {
        uint32_t gp = a.u32(), max_count = a.u32(); GLuint p = lookup(programs, gp);
        std::vector<GLuint> ids(max_count); GLsizei count = 0;
        glGetAttachedShaders(p, max_count, &count, ids.data());
        put_u32(count);
        for (GLsizei i = 0; i < count; ++i) {
            uint32_t guest = 0;
            for (const auto &entry : shaders) if (entry.second == ids[i]) { guest = entry.first; break; }
            put_u32(guest);
        }
        break;
    }
    case UNIFORM_2I: { GLint l=a.i32(), x=a.i32(), y=a.i32(); glUniform2i(l,x,y); break; }
    case UNIFORM_3I: { GLint l=a.i32(), x=a.i32(), y=a.i32(), z=a.i32(); glUniform3i(l,x,y,z); break; }
    case UNIFORM_4I: { GLint l=a.i32(), x=a.i32(), y=a.i32(), z=a.i32(), w=a.i32(); glUniform4i(l,x,y,z,w); break; }
    case GET_ACTIVE_UNIFORM:
    case GET_ACTIVE_ATTRIB:
    case GET_TRANSFORM_FEEDBACK_VARYING: {
        GLuint program = lookup(programs, a.u32()); GLuint index = a.u32();
        GLenum length_query = op == GET_ACTIVE_UNIFORM
            ? GL_ACTIVE_UNIFORM_MAX_LENGTH
            : op == GET_ACTIVE_ATTRIB
            ? GL_ACTIVE_ATTRIBUTE_MAX_LENGTH
            : GL_TRANSFORM_FEEDBACK_VARYING_MAX_LENGTH;
        GLint max_len = 0;
        glGetProgramiv(program, length_query, &max_len);
        std::vector<char> name(std::max(max_len, 1)); GLsizei length = 0; GLint size = 0; GLenum type = 0;
        if (op == GET_ACTIVE_UNIFORM) glGetActiveUniform(program, index, name.size(), &length, &size, &type, name.data());
        else if (op == GET_ACTIVE_ATTRIB) glGetActiveAttrib(program, index, name.size(), &length, &size, &type, name.data());
        else glGetTransformFeedbackVarying(program, index, name.size(), &length, &size, &type, name.data());
        put_u32(size); put_u32(type); put_blob(name.data(), length); break;
    }
    case GET_ACTIVE_UNIFORM_NAME:
    case GET_ACTIVE_UNIFORM_BLOCK_NAME: {
        GLuint program = lookup(programs, a.u32()); GLuint index = a.u32();
        GLint max_len = 0;
        glGetProgramiv(program,
                       op == GET_ACTIVE_UNIFORM_NAME
                           ? GL_ACTIVE_UNIFORM_MAX_LENGTH
                           : GL_ACTIVE_UNIFORM_BLOCK_MAX_NAME_LENGTH,
                       &max_len);
        std::vector<char> name(std::max(max_len, 1));
        GLsizei length = 0;
        if (op == GET_ACTIVE_UNIFORM_NAME)
            glGetActiveUniformName(program, index, name.size(), &length, name.data());
        else
            glGetActiveUniformBlockName(program, index, name.size(), &length, name.data());
        put_blob(name.data(), length); break;
    }
    case GET_ACTIVE_UNIFORM_BLOCKIV: {
        GLuint program = lookup(programs, a.u32());
        GLuint index = a.u32();
        GLenum pname = a.u32();
        /* Only one query returns more than a single integer, and its length is
         * itself a query. */
        GLint count = 1;
        if (pname == GL_UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES)
            glGetActiveUniformBlockiv(program, index,
                                      GL_UNIFORM_BLOCK_ACTIVE_UNIFORMS, &count);
        std::vector<GLint> params(std::max(count, 1), 0);
        glGetActiveUniformBlockiv(program, index, pname, params.data());
        put_blob(params.data(), std::max(count, 0) * sizeof(GLint)); break;
    }
    case GET_ACTIVE_UNIFORMSIV: {
        GLuint program = lookup(programs, a.u32());
        GLenum pname = a.u32();
        uint32_t bytes = 0;
        const uint8_t* raw = a.blob(bytes);
        const GLsizei count = (GLsizei)(bytes / sizeof(GLuint));
        std::vector<GLuint> indices(count);
        if (count) memcpy(indices.data(), raw, (size_t)count * sizeof(GLuint));
        std::vector<GLint> params(std::max(count, 1), 0);
        glGetActiveUniformsiv(program, count, indices.data(), pname, params.data());
        put_blob(params.data(), (size_t)count * sizeof(GLint)); break;
    }
    case GET_UNIFORM_INDICES: {
        GLuint program = lookup(programs, a.u32());
        const GLsizei count = (GLsizei)a.u32();
        uint32_t bytes = 0;
        const uint8_t* raw = a.blob(bytes);
        /* One blob of NUL-terminated names; walk it rather than trusting a
         * separate length array the guest would have had to build. */
        std::vector<const GLchar*> names;
        const char* at = reinterpret_cast<const char*>(raw);
        const char* end = at + bytes;
        for (GLsizei i = 0; i < count && at < end; ++i) {
            names.push_back(at);
            at += strnlen(at, (size_t)(end - at)) + 1;
        }
        std::vector<GLuint> indices(std::max<size_t>(names.size(), 1),
                                    GL_INVALID_INDEX);
        if (!names.empty())
            glGetUniformIndices(program, (GLsizei)names.size(), names.data(),
                                indices.data());
        put_blob(indices.data(), names.size() * sizeof(GLuint)); break;
    }
    case GET_SHADER_SOURCE: {
        GLuint shader = lookup(shaders, a.u32());
        GLint max_len = 0;
        glGetShaderiv(shader, GL_SHADER_SOURCE_LENGTH, &max_len);
        std::vector<char> source(std::max(max_len, 1));
        GLsizei length = 0;
        glGetShaderSource(shader, source.size(), &length, source.data());
        put_blob(source.data(), length); break;
    }
#define GEX_UNIFORM_VECTOR(OP, FN, TYPE) case OP: { GLint l=a.i32(); GLsizei n=a.i32(); uint32_t bytes; const uint8_t*p=a.blob(bytes); FN(l,n,reinterpret_cast<const TYPE*>(p)); break; }
    GEX_UNIFORM_VECTOR(UNIFORM_1IV, glUniform1iv, GLint)
    GEX_UNIFORM_VECTOR(UNIFORM_1FV, glUniform1fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_2FV, glUniform2fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_3FV, glUniform3fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_4FV, glUniform4fv, GLfloat)
    GEX_UNIFORM_VECTOR(UNIFORM_2IV, glUniform2iv, GLint)
    GEX_UNIFORM_VECTOR(UNIFORM_3IV, glUniform3iv, GLint)
    GEX_UNIFORM_VECTOR(UNIFORM_4IV, glUniform4iv, GLint)
#undef GEX_UNIFORM_VECTOR
    case UNIFORM_MATRIX2FV: {GLint l=a.i32();GLsizei n=a.i32();GLboolean t=a.u32();uint32_t bytes;const uint8_t*p=a.blob(bytes);glUniformMatrix2fv(l,n,t,reinterpret_cast<const GLfloat*>(p));break;}
    case UNIFORM_MATRIX3FV: {GLint l=a.i32();GLsizei n=a.i32();GLboolean t=a.u32();uint32_t bytes;const uint8_t*p=a.blob(bytes);glUniformMatrix3fv(l,n,t,reinterpret_cast<const GLfloat*>(p));break;}
    case VERTEX_ATTRIB_I_POINTER: {
        GLuint i = a.u32(); GLint size = a.i32(); GLenum type = a.u32();
        GLsizei stride = a.i32(); uintptr_t off = a.u32();
        glVertexAttribIPointer(i, size, type, stride, reinterpret_cast<void*>(off)); break;
    }
    case VERTEX_ATTRIB_DIVISOR: { GLuint index=a.u32(), divisor=a.u32();
        glVertexAttribDivisor(index,divisor); break; }
    case VERTEX_ATTRIB_4F: { GLuint index=a.u32(); GLfloat x=a.f32(), y=a.f32(), z=a.f32(), w=a.f32(); glVertexAttrib4f(index,x,y,z,w); break; }
    case VERTEX_ATTRIB_I4I: { GLuint index=a.u32(); GLint x=a.i32(), y=a.i32(), z=a.i32(), w=a.i32(); glVertexAttribI4i(index,x,y,z,w); break; }
    case GEN_VERTEX_ARRAYS: {
        GLsizei n = a.i32();
        if (gexgl_negative_count(n, [](GLsizei c, GLuint *p) { glGenVertexArrays(c, p); })) break;
        std::vector<GLuint> ids(n); glGenVertexArrays(n, ids.data());
        for(uint32_t i=0;i<n;++i) vertex_arrays[a.u32()]=ids[i]; break;
    }
    case DELETE_VERTEX_ARRAYS: {
        uint32_t n=a.u32(); std::vector<GLuint> ids; ids.reserve(n);
        for(uint32_t i=0;i<n;++i){uint32_t g=a.u32();ids.push_back(lookup(vertex_arrays,g));vertex_arrays.erase(g);} glDeleteVertexArrays(n,ids.data());break;
    }
    case BIND_VERTEX_ARRAY: { uint32_t g=a.u32(); glBindVertexArray(g ? lookup(vertex_arrays,g) : default_vao); break; }
    case TEX_PARAMETER_F: { GLenum target=a.u32(), pname=a.u32(); GLfloat value=a.f32(); glTexParameterf(target,pname,value); break; }
    case TEX_PARAMETER_FV: {
        GLenum target=a.u32(), pname=a.u32(); GLfloat value[4]{};
        const size_t count = (pname == GL_TEXTURE_BORDER_COLOR || pname == GL_TEXTURE_SWIZZLE_RGBA) ? 4 : 1;
        for (size_t i = 0; i < count; ++i) value[i]=a.f32();
        glTexParameterfv(target,pname,value);
        break;
    }
    case TEX_PARAMETER_IV: {
        GLenum target=a.u32(), pname=a.u32(); GLint value[4]{};
        const size_t count = (pname == GL_TEXTURE_BORDER_COLOR || pname == GL_TEXTURE_SWIZZLE_RGBA) ? 4 : 1;
        for (size_t i = 0; i < count; ++i) value[i]=a.i32();
        glTexParameteriv(target,pname,value);
        break;
    }
    case DRAW_BUFFERS: {
        GLsizei n = a.i32(); current_draw_buffers.resize(std::max(n, 0));
        for (GLsizei i = 0; i < n; ++i) current_draw_buffers[i] = a.u32();
        glDrawBuffers(n, current_draw_buffers.data()); break;
    }
    case DRAW_ARRAYS_INSTANCED: { GLenum mode=a.u32(); GLint first=a.i32(); GLsizei count=a.i32(), instances=a.i32(); glDrawArraysInstanced(mode,first,count,instances); break; }
    case DRAW_ELEMENTS_INSTANCED: {
        GLenum mode=a.u32(); GLsizei n=a.i32(); GLenum type=a.u32(); uintptr_t off=a.u32(); GLsizei instances=a.i32();
        glDrawElementsInstanced(mode,n,type,reinterpret_cast<void*>(off),instances); break;
    }
    case BLIT_FRAMEBUFFER: {
        GLint sx0=a.i32(),sy0=a.i32(),sx1=a.i32(),sy1=a.i32();
        GLint dx0=a.i32(),dy0=a.i32(),dx1=a.i32(),dy1=a.i32();
        GLbitfield mask=a.u32(); GLenum filter=a.u32();
        glBlitFramebuffer(sx0,sy0,sx1,sy1,dx0,dy0,dx1,dy1,mask,filter); break;
    }
    case GENERATE_MIPMAP: glGenerateMipmap(a.u32()); break;
    case TEX_STORAGE_2D: { GLenum target=a.u32(); GLsizei levels=a.i32(); GLenum format=a.u32(); GLsizei w=a.i32(), h=a.i32(); glTexStorage2D(target,levels,format,w,h); break; }
    case TEX_STORAGE_3D: { GLenum target=a.u32(); GLsizei levels=a.i32(); GLenum format=a.u32(); GLsizei w=a.i32(), h=a.i32(), d=a.i32(); glTexStorage3D(target,levels,format,w,h,d); break; }
    case TEX_IMAGE_3D: {
        GLenum t=a.u32();GLint l=a.i32(),in=a.i32();GLsizei w=a.i32(),h=a.i32(),d=a.i32();
        GLint border=a.i32();GLenum f=a.u32(),ty=a.u32();bool pbo=a.u32()!=0;
        uintptr_t pointer=static_cast<uintptr_t>(a.u64());uint32_t n;const uint8_t* bytes=a.blob(n);
        gexgl_pixel_layout layout{};
        const bool valid=gexgl_compute_pixel_layout(f,ty,w,h,d,&unpack_store,&layout);
        if ((pbo && n != 0) || (!pbo && pointer && (!valid || layout.end != n))) {
            a.ok=false;break;
        }
        const void* data=pbo?reinterpret_cast<const void*>(pointer):(pointer?bytes:nullptr);
        glTexImage3D(t,l,in,w,h,d,border,f,ty,data);break;
    }
    case FRAMEBUFFER_TEXTURE_LAYER: {GLenum t=a.u32(),att=a.u32();GLuint tex=lookup(textures,a.u32());GLint level=a.i32(),layer=a.i32();glFramebufferTextureLayer(t,att,tex,level,layer);break;}
    case GET_STRING_I: {
        GLenum name = a.u32(); GLuint index = a.u32();
        const char* s = reinterpret_cast<const char*>(glGetStringi(name, index));
        put_blob(s, s ? std::strlen(s) : 0);
        break;
    }
    case GET_UNIFORM_FV:
    case GET_UNIFORM_IV: {
        GLuint program=lookup_program_or_shader(a.u32()); GLint location=a.i32();
        uint32_t components=uniform_components(program,location);
        if (op == GET_UNIFORM_FV) {
            GLfloat values[16] = {}; glGetUniformfv(program,location,values);
            put_blob(values,components*sizeof(values[0]));
        } else {
            GLint values[16] = {}; glGetUniformiv(program,location,values);
            put_blob(values,components*sizeof(values[0]));
        }
        break;
    }
