#!/usr/bin/env python3
"""Generate the mechanically safe scalar part of the GL 4.4 transport.

Only commands with an explicit policy below are emitted. Pointer lengths and
object namespaces are independently declared so neither can accidentally be
treated as an ordinary integer by the generated host decoder.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import tempfile


SCALAR_COMMANDS = {
    "glBeginTransformFeedback",
    "glBlendEquationSeparatei", "glBlendEquationi", "glBlendFuncSeparatei",
    "glBlendFunci", "glClampColor", "glClearDepth", "glColorMaski", "glCopyBufferSubData",
    "glCopyTexImage1D", "glCopyTexImage2D", "glCopyTexSubImage1D",
    "glCopyTexSubImage2D", "glCopyTexSubImage3D", "glDepthRange", "glDisablei",
    "glDispatchCompute", "glDispatchComputeIndirect", "glDrawArraysInstancedBaseInstance", "glDrawBuffer",
    "glEnablei", "glLogicOp",
    "glEndQuery", "glEndTransformFeedback", "glMemoryBarrier", "glMinSampleShading",
    "glPointParameterf", "glPointParameteri", "glPointSize", "glPolygonMode",
    "glPixelStoref",
    "glPatchParameteri",
    "glPrimitiveRestartIndex",
    "glProvokingVertex", "glReadBuffer", "glRenderbufferStorageMultisample",
    "glSampleCoverage", "glSampleMaski", "glTexImage2DMultisample",
    "glTexImage3DMultisample", "glTexStorage1D", "glTexStorage2DMultisample",
    "glTexStorage3DMultisample", "glUniform1ui", "glUniform2ui", "glUniform3ui",
    "glUniform4ui", "glVertexAttrib1f", "glVertexAttrib2f", "glVertexAttrib3f",
    "glVertexAttrib4Nub", "glVertexAttribI4ui",
}

PROGRAM_UNIFORM_SCALARS = {
    f"glProgramUniform{components}{kind}"
    for components in range(1, 5)
    for kind in ("f", "i", "ui")
}
SCALAR_COMMANDS |= PROGRAM_UNIFORM_SCALARS | {
    "glShaderStorageBlockBinding",
}

# Pointer-free commands are mechanically lossless once object-bearing GLuints
# are assigned an explicit namespace below. Keep pipeline objects out until
# their create/delete/name-array policy is added as a unit.
SCALAR_COMMANDS |= {
    "glActiveShaderProgram", "glBindProgramPipeline",
    "glBeginConditionalRender", "glBeginQuery", "glBeginQueryIndexed",
    "glBindVertexBuffer", "glDepthRangeIndexed",
    "glDrawTransformFeedback", "glDrawTransformFeedbackInstanced",
    "glDrawTransformFeedbackStream", "glDrawTransformFeedbackStreamInstanced",
    "glEndConditionalRender", "glEndQueryIndexed", "glFramebufferParameteri",
    "glInvalidateBufferData", "glInvalidateBufferSubData", "glInvalidateTexImage",
    "glInvalidateTexSubImage", "glPopDebugGroup",
    "glProgramParameteri", "glQueryCounter", "glReleaseShaderCompiler",
    "glScissorIndexed", "glTexBufferRange", "glUseProgramStages",
    "glValidateProgramPipeline",
    "glTextureView", "glUniform1d", "glUniform2d", "glUniform3d", "glUniform4d",
    "glVertexAttrib1d", "glVertexAttrib1s", "glVertexAttrib2d", "glVertexAttrib2s",
    "glVertexAttrib3d", "glVertexAttrib3s", "glVertexAttrib4d", "glVertexAttrib4s",
    "glVertexAttribBinding", "glVertexAttribFormat", "glVertexAttribI1i",
    "glVertexAttribI1ui", "glVertexAttribI2i", "glVertexAttribI2ui",
    "glVertexAttribI3i", "glVertexAttribI3ui", "glVertexAttribIFormat",
    "glVertexAttribL1d", "glVertexAttribL2d", "glVertexAttribL3d",
    "glVertexAttribL4d", "glVertexAttribLFormat", "glVertexAttribP1ui",
    "glVertexAttribP2ui", "glVertexAttribP3ui", "glVertexAttribP4ui",
    "glVertexBindingDivisor", "glViewportIndexedf",
    *(f"glProgramUniform{components}d" for components in range(1, 5)),
}

# Synchronous scalar queries. These share scalar argument encoding with the
# void family, but flush the ordered stream and consume one reply word.
RETURN_COMMANDS = {
    "glIsBuffer", "glIsEnabled", "glIsEnabledi", "glIsFramebuffer",
    "glIsProgram", "glIsRenderbuffer", "glIsSampler", "glIsShader",
    "glIsTexture", "glIsVertexArray",
    "glIsProgramPipeline",
}

PROGRAM_PIPELINE_GENERATORS = {"glGenProgramPipelines"}
PROGRAM_PIPELINE_DELETERS = {"glDeleteProgramPipelines"}

# Program reflection is synchronous and has mixed string/array input and
# variable-sized output. Keep these policies explicit instead of pretending
# their pointers are ordinary inline arrays.
PROGRAM_INTERFACE_QUERIES = {"glGetProgramInterfaceiv"}
PROGRAM_NAMED_QUERIES = {
    "glGetProgramResourceIndex", "glGetProgramResourceLocation",
    "glGetProgramResourceLocationIndex",
}
PROGRAM_RESOURCE_NAME_QUERIES = {"glGetProgramResourceName"}
PROGRAM_RESOURCE_IV_QUERIES = {"glGetProgramResourceiv"}
INDEXED_INTEGER_QUERIES = {"glGetIntegeri_v", "glGetInteger64i_v"}
BUFFER_READBACK_QUERIES = {"glGetBufferSubData"}
PROGRAM_QUERY_COMMANDS = (PROGRAM_INTERFACE_QUERIES | PROGRAM_NAMED_QUERIES |
                          PROGRAM_RESOURCE_NAME_QUERIES | PROGRAM_RESOURCE_IV_QUERIES |
                          INDEXED_INTEGER_QUERIES | BUFFER_READBACK_QUERIES)

# Synchronous resource/state queries whose output arity is defined by a pname.
# Each policy is deliberately bounded to one or four values: this prevents a
# remote implementation from writing four values into an application buffer
# that is only one scalar wide. The vector cases use the numeric core enums so
# the generated guest library remains independent of host GL headers.
RESOURCE_OUTPUT_QUERIES = {
    "glGetBooleanv": "state",
    "glGetDoublev": "state",
    "glGetFloatv": "state",
    "glGetBufferParameteri64v": "one",
    "glGetBufferParameteriv": "one",
    "glGetFramebufferAttachmentParameteriv": "one",
    "glGetQueryiv": "one",
    "glGetRenderbufferParameteriv": "one",
    "glGetSamplerParameterIiv": "sampler",
    "glGetSamplerParameterIuiv": "sampler",
    "glGetSamplerParameterfv": "sampler",
    "glGetSamplerParameteriv": "sampler",
    "glGetTexLevelParameterfv": "one",
    "glGetTexLevelParameteriv": "one",
    "glGetTexParameterIiv": "texture",
    "glGetTexParameterIuiv": "texture",
    "glGetTexParameterfv": "texture",
    "glGetTexParameteriv": "texture",
    "glGetVertexAttribIiv": "vertex",
    "glGetVertexAttribIuiv": "vertex",
    "glGetVertexAttribfv": "vertex",
    "glGetVertexAttribiv": "vertex",
    "glGetActiveAtomicCounterBufferiv": "one",
    "glGetBooleani_v": "indexed_state",
    "glGetDoublei_v": "indexed_state",
    "glGetFloati_v": "indexed_state",
    "glGetFramebufferParameteriv": "one",
    "glGetInteger64v": "state",
    "glGetProgramStageiv": "one",
    "glGetQueryIndexediv": "one",
    "glGetQueryObjecti64v": "one",
    "glGetQueryObjectiv": "one",
    "glGetQueryObjectui64v": "one",
    "glGetQueryObjectuiv": "one",
    "glGetVertexAttribdv": "vertex",
    "glGetVertexAttribLdv": "vertex",
    "glGetProgramPipelineiv": "one",
    "glGetUniformSubroutineuiv": "one",
}

# A final const byte pointer whose Khronos `len` is a scalar byte count. This
# is the simplest pointer policy and remains distinct from arrays of objects,
# strings, output buffers, pixel-store-sized images, and opaque offsets.
BLOB_COMMANDS = {
    "glCompressedTexImage1D", "glCompressedTexImage2D", "glCompressedTexImage3D",
    "glCompressedTexSubImage1D", "glCompressedTexSubImage2D", "glCompressedTexSubImage3D",
}

TEXTURE_PARAMETER_ARRAYS = {
    "glSamplerParameterIiv", "glSamplerParameterIuiv",
    "glTexParameterIiv", "glTexParameterIuiv",
}
SAMPLER_MULTI_BIND_COMMANDS = {"glBindSamplers"}
MULTI_BIND_NAME_ARRAYS = {
    "glBindBuffersBase": "buffers",
    "glBindImageTextures": "textures",
    "glBindTextures": "textures",
}
INTERNAL_FORMAT_QUERIES = {"glGetInternalformativ", "glGetInternalformati64v"}
MULTISAMPLE_QUERIES = {"glGetMultisamplefv"}
COMPRESSED_TEXTURE_READBACK_QUERIES = {"glGetCompressedTexImage"}

# An opaque pointer interpreted by core OpenGL as a byte offset into the bound
# GL_DRAW_INDIRECT_BUFFER. It is transported as a 64-bit offset, never followed
# as guest memory.
OFFSET_COMMANDS = {
    "glDrawArraysIndirect", "glDrawElementsIndirect",
    "glMultiDrawArraysIndirect", "glMultiDrawElementsIndirect",
}
VERTEX_OFFSET_COMMANDS = {"glVertexAttribLPointer"}

# An opaque pointer interpreted as a byte offset into the vertex array's bound
# GL_ELEMENT_ARRAY_BUFFER.  In an OpenGL 4.4 core context these are never guest
# client-memory pointers.  Keep this separate from indirect-buffer offsets so
# validation catches a registry signature drifting into the wrong policy.
ELEMENT_OFFSET_COMMANDS = {
    "glDrawElementsBaseVertex",
    "glDrawElementsInstancedBaseInstance",
    "glDrawElementsInstancedBaseVertex",
    "glDrawElementsInstancedBaseVertexBaseInstance",
    "glDrawRangeElements",
    "glDrawRangeElementsBaseVertex",
}
OPAQUE_OFFSET_COMMANDS = OFFSET_COMMANDS | ELEMENT_OFFSET_COMMANDS | VERTEX_OFFSET_COMMANDS
BUFFER_STORAGE_COMMANDS = {"glBufferStorage"}
UNIFORM_VALUE_QUERIES = {
    "glGetUniformdv": ("double", "GLdouble"),
    "glGetUniformuiv": ("unsigned", "GLuint"),
}
CLEAR_VALUE_COMMANDS = {
    "glClearBufferData", "glClearBufferSubData",
    "glClearTexImage", "glClearTexSubImage",
}
PARALLEL_BIND_COMMANDS = {
    "glBindBuffersRange": ("GLintptr", "GLsizeiptr"),
    "glBindVertexBuffers": ("GLintptr", "GLsizei"),
}

# Parallel arrays of counts, element-buffer offsets and (optionally) base
# vertices. These need a dedicated packet: the values in `indices` are opaque
# offsets, while the pointer array holding them is ordinary guest memory.
MULTI_ELEMENT_COMMANDS = {"glMultiDrawElements", "glMultiDrawElementsBaseVertex"}
MULTI_ARRAY_COMMANDS = {"glMultiDrawArrays"}

# Typed, final const arrays whose registry length is an arithmetic expression
# over earlier scalar parameters. Every selected type is one 32-bit wire word,
# so the inline byte payload is naturally aligned for the host call.
ARRAY_COMMANDS = {
    *(f"glProgramUniform{components}{kind}v"
      for components in range(1, 5) for kind in ("f", "i", "ui")),
    "glProgramUniformMatrix2fv", "glProgramUniformMatrix3fv",
    "glProgramUniformMatrix4fv", "glProgramUniformMatrix2x3fv",
    "glProgramUniformMatrix2x4fv", "glProgramUniformMatrix3x2fv",
    "glProgramUniformMatrix3x4fv", "glProgramUniformMatrix4x2fv",
    "glProgramUniformMatrix4x3fv",
    "glUniform1uiv", "glUniform2uiv", "glUniform3uiv", "glUniform4uiv",
    "glUniformMatrix2x3fv", "glUniformMatrix2x4fv",
    "glUniformMatrix3x2fv", "glUniformMatrix3x4fv",
    "glUniformMatrix4x2fv", "glUniformMatrix4x3fv",
    "glVertexAttrib1fv", "glVertexAttrib2fv", "glVertexAttrib3fv",
    "glVertexAttrib4Nubv", "glVertexAttrib4fv", "glVertexAttrib4ubv",
    "glVertexAttribI4iv", "glVertexAttribI4uiv",
    *(f"glProgramUniform{components}dv" for components in range(1, 5)),
    "glProgramUniformMatrix2dv", "glProgramUniformMatrix3dv",
    "glProgramUniformMatrix4dv", "glProgramUniformMatrix2x3dv",
    "glProgramUniformMatrix2x4dv", "glProgramUniformMatrix3x2dv",
    "glProgramUniformMatrix3x4dv", "glProgramUniformMatrix4x2dv",
    "glProgramUniformMatrix4x3dv",
    *(f"glUniform{components}dv" for components in range(1, 5)),
    "glUniformMatrix2dv", "glUniformMatrix3dv", "glUniformMatrix4dv",
    "glUniformMatrix2x3dv", "glUniformMatrix2x4dv",
    "glUniformMatrix3x2dv", "glUniformMatrix3x4dv",
    "glUniformMatrix4x2dv", "glUniformMatrix4x3dv",
    "glUniformSubroutinesuiv",
    "glVertexAttrib1dv", "glVertexAttrib1sv", "glVertexAttrib2dv",
    "glVertexAttrib2sv", "glVertexAttrib3dv", "glVertexAttrib3sv",
    "glVertexAttrib4Nbv", "glVertexAttrib4Niv", "glVertexAttrib4Nsv",
    "glVertexAttrib4Nuiv", "glVertexAttrib4Nusv", "glVertexAttrib4bv",
    "glVertexAttrib4dv", "glVertexAttrib4iv", "glVertexAttrib4sv",
    "glVertexAttrib4uiv", "glVertexAttrib4usv", "glVertexAttribI1iv",
    "glVertexAttribI1uiv", "glVertexAttribI2iv", "glVertexAttribI2uiv",
    "glVertexAttribI3iv", "glVertexAttribI3uiv", "glVertexAttribI4bv",
    "glVertexAttribI4sv", "glVertexAttribI4ubv", "glVertexAttribI4usv",
    "glVertexAttribL1dv", "glVertexAttribL2dv", "glVertexAttribL3dv",
    "glVertexAttribL4dv", "glVertexAttribP1uiv", "glVertexAttribP2uiv",
    "glVertexAttribP3uiv", "glVertexAttribP4uiv",
    "glDepthRangeArrayv", "glScissorArrayv", "glScissorIndexedv",
    "glViewportArrayv", "glViewportIndexedfv",
    "glInvalidateFramebuffer",
}

SPECIAL_ARRAY_LENGTHS = {
    "glDepthRangeArrayv": ("count", 2),
    "glScissorArrayv": ("count", 4),
    "glViewportArrayv": ("count", 4),
}

# The number of patch defaults is selected by pname rather than by a scalar
# count argument: two inner tessellation levels, four outer levels.
PATCH_PARAMETER_ARRAYS = {"glPatchParameterfv"}
POINT_PARAMETER_ARRAYS = {"glPointParameterfv", "glPointParameteriv"}
PIXEL_IMAGE_COMMANDS = {"glTexImage1D", "glTexSubImage1D", "glTexSubImage3D"}
PIXEL_IMAGE_LENGTHS = {
    "glTexImage1D": "COMPSIZE(format,type,width)",
    "glTexSubImage1D": "COMPSIZE(format,type,width)",
    "glTexSubImage3D": "COMPSIZE(format,type,width,height,depth)",
}
TEXTURE_READBACK_QUERIES = {"glGetTexImage"}

# Arrays of separately terminated strings cannot use the typed-array policy:
# the pointer array itself is guest-addressed, while each pointed-to string has
# a different length.  Encode the strings consecutively, including each NUL,
# and reconstruct the transient host pointer array at dispatch time.
STRING_ARRAY_COMMANDS = {"glTransformFeedbackVaryings"}
STRING_INPUT_COMMANDS = {"glBindFragDataLocation", "glBindFragDataLocationIndexed"}
STRING_RETURN_COMMANDS = {
    "glGetFragDataIndex": "GLint",
    "glGetFragDataLocation": "GLint",
    "glGetSubroutineIndex": "GLuint",
    "glGetSubroutineUniformLocation": "GLint",
}

# Guest GLuint names are not MobileGL's names. Each object-bearing parameter
# names the host map through which it must be resolved before the GL call.
OBJECT_COMMANDS = {
    "glBindImageTexture": {"texture": "textures"},
    "glCopyImageSubData": {"srcName": "textures", "dstName": "textures"},
    "glBindBufferRange": {"buffer": "buffers"},
    "glFramebufferTexture": {"texture": "textures"},
    "glFramebufferTexture1D": {"texture": "textures"},
    "glFramebufferTexture3D": {"texture": "textures"},
    "glTexBuffer": {"buffer": "buffers"},
    "glValidateProgram": {"program": "programs"},
    "glTransformFeedbackVaryings": {"program": "programs"},
}
OBJECT_COMMANDS.update({
    "glActiveShaderProgram": {"pipeline": "program_pipelines", "program": "programs"},
    "glBeginConditionalRender": {"id": "queries"},
    "glBeginQuery": {"id": "queries"},
    "glBeginQueryIndexed": {"id": "queries"},
    "glBindProgramPipeline": {"pipeline": "program_pipelines"},
    "glBindVertexBuffer": {"buffer": "buffers"},
    "glDrawTransformFeedback": {"id": "transform_feedbacks"},
    "glDrawTransformFeedbackInstanced": {"id": "transform_feedbacks"},
    "glDrawTransformFeedbackStream": {"id": "transform_feedbacks"},
    "glDrawTransformFeedbackStreamInstanced": {"id": "transform_feedbacks"},
    "glInvalidateBufferData": {"buffer": "buffers"},
    "glInvalidateBufferSubData": {"buffer": "buffers"},
    "glInvalidateTexImage": {"texture": "textures"},
    "glInvalidateTexSubImage": {"texture": "textures"},
    "glIsProgramPipeline": {"pipeline": "program_pipelines"},
    "glProgramParameteri": {"program": "programs"},
    "glQueryCounter": {"id": "queries"},
    "glTexBufferRange": {"buffer": "buffers"},
    "glTextureView": {"texture": "textures", "origtexture": "textures"},
    "glUseProgramStages": {"pipeline": "program_pipelines", "program": "programs"},
    "glValidateProgramPipeline": {"pipeline": "program_pipelines"},
    "glGetActiveAtomicCounterBufferiv": {"program": "programs"},
    "glGetProgramStageiv": {"program": "programs"},
    "glGetQueryObjecti64v": {"id": "queries"},
    "glGetQueryObjectiv": {"id": "queries"},
    "glGetQueryObjectui64v": {"id": "queries"},
    "glGetQueryObjectuiv": {"id": "queries"},
    "glBindFragDataLocation": {"program": "programs"},
    "glBindFragDataLocationIndexed": {"program": "programs"},
    "glGetFragDataIndex": {"program": "programs"},
    "glGetFragDataLocation": {"program": "programs"},
    "glGetSubroutineIndex": {"program": "programs"},
    "glGetSubroutineUniformLocation": {"program": "programs"},
    "glGetProgramPipelineiv": {"pipeline": "program_pipelines"},
    "glClearTexImage": {"texture": "textures"},
    "glClearTexSubImage": {"texture": "textures"},
})
OBJECT_COMMANDS.update({
    name: {"program": "programs"}
    for name in SCALAR_COMMANDS | ARRAY_COMMANDS
    if name.startswith("glProgramUniform")
})
OBJECT_COMMANDS["glShaderStorageBlockBinding"] = {"program": "programs"}
OBJECT_COMMANDS.update({
    name: {"sampler": "samplers"}
    for name in RESOURCE_OUTPUT_QUERIES
    if name.startswith("glGetSamplerParameter")
})
OBJECT_COMMANDS.update({
    "glIsBuffer": {"buffer": "buffers"},
    "glIsFramebuffer": {"framebuffer": "framebuffers"},
    "glIsProgram": {"program": "programs"},
    "glIsRenderbuffer": {"renderbuffer": "renderbuffers"},
    "glIsSampler": {"sampler": "samplers"},
    "glIsShader": {"shader": "shaders"},
    "glIsTexture": {"texture": "textures"},
    "glIsVertexArray": {"array": "vertex_arrays"},
})

TYPE_INFO = {
    "GLenum": ("unsigned", "u32", "(uint32_t)({name})", 1),
    "GLuint": ("unsigned", "u32", "(uint32_t)({name})", 1),
    "GLbitfield": ("unsigned", "u32", "(uint32_t)({name})", 1),
    "GLint": ("int", "i32", "(uint32_t)({name})", 1),
    "GLint64": ("long long", "i64", "", 2),
    "GLuint64": ("unsigned long long", "u64", "", 2),
    "GLsizei": ("int", "i32", "(uint32_t)({name})", 1),
    "GLshort": ("short", "i32", "(uint32_t)(int32_t)({name})", 1),
    "GLboolean": ("unsigned char", "u32", "(uint32_t)({name})", 1),
    "GLbyte": ("signed char", "i32", "(uint32_t)(int32_t)({name})", 1),
    "GLubyte": ("unsigned char", "u32", "(uint32_t)({name})", 1),
    "GLushort": ("unsigned short", "u32", "(uint32_t)({name})", 1),
    "GLfloat": ("float", "f32", "as_bits({name})", 1),
    "GLdouble": ("double", "f64", "", 2),
    "GLintptr": ("long", "i64", "", 2),
    "GLsizeiptr": ("long", "i64", "", 2),
}


def array_length(length: str) -> tuple[str | None, int]:
    """Return the scalar count parameter and fixed component multiplier."""
    match = re.fullmatch(r"([A-Za-z_]\w*)(?:\*(\d+))?", length)
    if match:
        return match.group(1), int(match.group(2) or 1)
    if length.isdigit():
        return None, int(length)
    raise RuntimeError(f"unsupported array length expression: {length}")


def command_array_length(command_name: str, length: str) -> tuple[str | None, int]:
    return SPECIAL_ARRAY_LENGTHS.get(command_name, array_length(length) if not length.startswith("COMPSIZE(") else (None, 0))


def load_manifest(root: pathlib.Path) -> dict:
    with tempfile.NamedTemporaryFile(suffix=".json") as output:
        subprocess.run(
            [root / "tools/gen_gl44_manifest.py", "--output", output.name], check=True
        )
        return json.load(output)


def validate(commands: list[dict]) -> list[dict]:
    by_name = {command["name"]: command for command in commands}
    policies = (SCALAR_COMMANDS | RETURN_COMMANDS | PROGRAM_QUERY_COMMANDS |
                RESOURCE_OUTPUT_QUERIES.keys() | BLOB_COMMANDS |
                ARRAY_COMMANDS | OPAQUE_OFFSET_COMMANDS | MULTI_ARRAY_COMMANDS |
                MULTI_ELEMENT_COMMANDS |
                TEXTURE_PARAMETER_ARRAYS | SAMPLER_MULTI_BIND_COMMANDS |
                MULTI_BIND_NAME_ARRAYS.keys() |
                INTERNAL_FORMAT_QUERIES | MULTISAMPLE_QUERIES |
                COMPRESSED_TEXTURE_READBACK_QUERIES | OBJECT_COMMANDS.keys())
    policies |= PROGRAM_PIPELINE_GENERATORS | PROGRAM_PIPELINE_DELETERS
    policies |= BUFFER_STORAGE_COMMANDS
    policies |= STRING_INPUT_COMMANDS | STRING_RETURN_COMMANDS.keys()
    policies |= UNIFORM_VALUE_QUERIES.keys()
    policies |= CLEAR_VALUE_COMMANDS
    policies |= PARALLEL_BIND_COMMANDS.keys()
    policies |= (PATCH_PARAMETER_ARRAYS | POINT_PARAMETER_ARRAYS | PIXEL_IMAGE_COMMANDS |
                 STRING_ARRAY_COMMANDS | TEXTURE_READBACK_QUERIES)
    absent = policies - by_name.keys()
    if absent:
        raise RuntimeError("commands absent from GL 4.4 core: " + ", ".join(sorted(absent)))
    selected = [by_name[name] for name in sorted(policies)]
    for command in selected:
        if command["name"] == "glGetProgramResourceIndex":
            expected_return = "GLuint"
        elif command["name"] in {"glGetProgramResourceLocation", "glGetProgramResourceLocationIndex"}:
            expected_return = "GLint"
        elif command["name"] in STRING_RETURN_COMMANDS:
            expected_return = STRING_RETURN_COMMANDS[command["name"]]
        else:
            expected_return = "GLboolean" if command["name"] in RETURN_COMMANDS else "void"
        if command["return_type"] != expected_return:
            raise RuntimeError(
                f"{command['name']}: expected {expected_return} return, got {command['return_type']}"
            )
        pointers = [param for param in command["params"] if param["pointer"]]
        if command["name"] in PROGRAM_PIPELINE_GENERATORS | PROGRAM_PIPELINE_DELETERS:
            continue
        if command["name"] in BUFFER_STORAGE_COMMANDS:
            if (len(pointers) != 1 or pointers[0]["name"] != "data" or
                    pointers[0]["type"] != "void" or pointers[0]["length"] != "size"):
                raise RuntimeError(f"{command['name']}: unexpected storage data pointer")
            continue
        if command["name"] in STRING_INPUT_COMMANDS | STRING_RETURN_COMMANDS.keys():
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    pointers[0]["type"] != "GLchar" or not pointers[0]["const"]):
                raise RuntimeError(f"{command['name']}: unexpected string input")
            continue
        if command["name"] in UNIFORM_VALUE_QUERIES:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    pointers[0]["const"]):
                raise RuntimeError(f"{command['name']}: unexpected uniform output")
            continue
        if command["name"] in CLEAR_VALUE_COMMANDS:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    pointers[0]["type"] != "void" or not pointers[0]["const"]):
                raise RuntimeError(f"{command['name']}: unexpected clear value")
            continue
        if command["name"] in PARALLEL_BIND_COMMANDS:
            if ([p["name"] for p in pointers] != ["buffers", "offsets", "sizes"] and
                    [p["name"] for p in pointers] != ["buffers", "offsets", "strides"]):
                raise RuntimeError(f"{command['name']}: unexpected parallel bind arrays")
            if any(p["length"] != "count" for p in pointers):
                raise RuntimeError(f"{command['name']}: parallel array length is not count")
            continue
        if command["name"] in (PROGRAM_QUERY_COMMANDS | INTERNAL_FORMAT_QUERIES |
                               MULTISAMPLE_QUERIES | COMPRESSED_TEXTURE_READBACK_QUERIES):
            continue
        if command["name"] in TEXTURE_READBACK_QUERIES:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    pointers[0]["const"] or pointers[0]["type"] != "void"):
                raise RuntimeError(f"{command['name']}: unexpected texture readback shape")
            continue
        if command["name"] in RESOURCE_OUTPUT_QUERIES:
            if len(pointers) != 1 or pointers[0] is not command["params"][-1]:
                raise RuntimeError(f"{command['name']}: output must be the final parameter")
            if pointers[0]["const"] or pointers[0]["type"] not in TYPE_INFO:
                raise RuntimeError(f"{command['name']}: output needs a supported mutable scalar type")
            continue
        if command["name"] in TEXTURE_PARAMETER_ARRAYS:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    not pointers[0]["const"] or pointers[0]["length"] != "COMPSIZE(pname)" or
                    pointers[0]["type"] not in {"GLint", "GLuint"}):
                raise RuntimeError(f"{command['name']}: unexpected texture parameter array")
        elif command["name"] in SAMPLER_MULTI_BIND_COMMANDS:
            if (len(pointers) != 1 or pointers[0]["name"] != "samplers" or
                    pointers[0]["type"] != "GLuint" or pointers[0]["length"] != "count"):
                raise RuntimeError(f"{command['name']}: unexpected sampler name array")
        elif command["name"] in MULTI_BIND_NAME_ARRAYS:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    pointers[0]["type"] != "GLuint" or
                    pointers[0]["length"] != "count"):
                raise RuntimeError(f"{command['name']}: unexpected object name array")
        elif command["name"] in MULTI_ARRAY_COMMANDS:
            if ([pointer["name"] for pointer in pointers] != ["first", "count"] or
                    any(not pointer["const"] or pointer["length"] != "drawcount"
                        for pointer in pointers) or
                    [pointer["type"] for pointer in pointers] != ["GLint", "GLsizei"]):
                raise RuntimeError(f"{command['name']}: unexpected multi-array inputs")
        elif command["name"] in MULTI_ELEMENT_COMMANDS:
            expected = ["count", "indices"]
            if command["name"] == "glMultiDrawElementsBaseVertex":
                expected.append("basevertex")
            if ([pointer["name"] for pointer in pointers] != expected or
                    any(not pointer["const"] or pointer["length"] != "drawcount"
                        for pointer in pointers)):
                raise RuntimeError(f"{command['name']}: unexpected multi-element arrays")
            if (pointers[0]["type"] != "GLsizei" or pointers[1]["type"] != "void" or
                    (len(pointers) == 3 and pointers[2]["type"] != "GLint")):
                raise RuntimeError(f"{command['name']}: unexpected multi-element types")
        elif command["name"] in OPAQUE_OFFSET_COMMANDS:
            expected_name = ("indirect" if command["name"] in OFFSET_COMMANDS else
                             "pointer" if command["name"] in VERTEX_OFFSET_COMMANDS else
                             "indices")
            if len(pointers) != 1 or pointers[0]["name"] != expected_name:
                raise RuntimeError(f"{command['name']}: expected one opaque {expected_name} offset")
            if not pointers[0]["const"] or pointers[0]["type"] != "void":
                raise RuntimeError(f"{command['name']}: indirect offset must be const void *")
        elif command["name"] in BLOB_COMMANDS:
            if len(pointers) != 1 or pointers[0] is not command["params"][-1]:
                raise RuntimeError(f"{command['name']}: blob must be the final parameter")
            if not pointers[0]["const"] or pointers[0]["type"] != "void":
                raise RuntimeError(f"{command['name']}: blob must be const void *")
            if not pointers[0]["length"]:
                raise RuntimeError(f"{command['name']}: blob has no registry length")
        elif command["name"] in PIXEL_IMAGE_COMMANDS:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    not pointers[0]["const"] or pointers[0]["type"] != "void" or
                    pointers[0]["length"] != PIXEL_IMAGE_LENGTHS[command["name"]]):
                raise RuntimeError(f"{command['name']}: unexpected pixel image shape")
        elif command["name"] in PATCH_PARAMETER_ARRAYS:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    not pointers[0]["const"] or pointers[0]["type"] != "GLfloat" or
                    pointers[0]["length"] != "COMPSIZE(pname)"):
                raise RuntimeError(f"{command['name']}: unexpected patch parameter shape")
        elif command["name"] in POINT_PARAMETER_ARRAYS:
            if (len(pointers) != 1 or pointers[0] is not command["params"][-1] or
                    not pointers[0]["const"] or pointers[0]["type"] not in {"GLfloat", "GLint"} or
                    pointers[0]["length"] != "COMPSIZE(pname)"):
                raise RuntimeError(f"{command['name']}: unexpected point parameter shape")
        elif command["name"] in STRING_ARRAY_COMMANDS:
            if (len(pointers) != 1 or pointers[0]["name"] != "varyings" or
                    not pointers[0]["const"] or pointers[0]["type"] != "GLchar" or
                    pointers[0]["length"] != "count"):
                raise RuntimeError(f"{command['name']}: unexpected string array shape")
        elif command["name"] in ARRAY_COMMANDS:
            if len(pointers) != 1 or pointers[0] is not command["params"][-1]:
                raise RuntimeError(f"{command['name']}: array must be the final parameter")
            if not pointers[0]["const"] or pointers[0]["type"] not in TYPE_INFO:
                raise RuntimeError(f"{command['name']}: array needs a supported const scalar type")
            if not pointers[0]["length"]:
                raise RuntimeError(f"{command['name']}: array needs a registry length")
            count_name, components = command_array_length(command["name"], pointers[0]["length"])
            if components == 0:
                raise RuntimeError(f"{command['name']}: unsupported array length {pointers[0]['length']}")
            if count_name and count_name not in {p["name"] for p in command["params"] if not p["pointer"]}:
                raise RuntimeError(f"{command['name']}: array length references unknown scalar")
        elif pointers:
            raise RuntimeError(f"{command['name']}: pointer needs an explicit length policy")
        for param in command["params"]:
            if param["pointer"]:
                continue
            if param["type"] not in TYPE_INFO:
                raise RuntimeError(f"{command['name']}: unsupported type {param['type']}")
    return selected


def output_component_expression(policy: str, pname: str = "pname") -> str:
    if policy == "one":
        return "1u"
    if policy == "texture":
        # GL_TEXTURE_BORDER_COLOR, GL_TEXTURE_SWIZZLE_RGBA
        return f"(({pname}) == 0x1004u || ({pname}) == 0x8e46u ? 4u : 1u)"
    if policy == "sampler":
        # GL_TEXTURE_BORDER_COLOR
        return f"(({pname}) == 0x1004u ? 4u : 1u)"
    if policy == "vertex":
        # GL_CURRENT_VERTEX_ATTRIB
        return f"(({pname}) == 0x8626u ? 4u : 1u)"
    if policy == "state":
        return f"gexgl_state_query_components({pname})"
    if policy == "indexed_state":
        return f"gexgl_state_query_components(target)"
    raise RuntimeError(f"unknown output component policy: {policy}")


def guest_function(command: dict, opcode: int) -> str:
    name = command["name"]
    if name in PROGRAM_PIPELINE_GENERATORS:
        return f"""void {name}(int n, unsigned *pipelines) {{
    gen_named_objects({opcode}, n, pipelines);
}}
"""
    if name in PROGRAM_PIPELINE_DELETERS:
        return f"""void {name}(int n, const unsigned *pipelines) {{
    delete_named_objects({opcode}, n, pipelines);
}}
"""
    if name in BUFFER_STORAGE_COMMANDS:
        return f"""void {name}(unsigned target, long size, const void *data, unsigned flags) {{
    if (size < 0) return;
    size_t blob_len = data ? (size_t)size : 0;
    struct buffer_object_state *object = buffer_object(bound_buffer(target), 1);
    if (object) object->size = (size_t)size;
    uint32_t *a = begin({opcode}, 5, 4 + blob_len);
    if (!a) return;
    uint64_t bytes = (uint64_t)size;
    a[0] = target; a[1] = (uint32_t)bytes; a[2] = (uint32_t)(bytes >> 32);
    a[3] = flags; a[4] = data != NULL; put_blob(a + 5, data, blob_len);
}}
"""
    if name in STRING_INPUT_COMMANDS | STRING_RETURN_COMMANDS.keys():
        scalars = command["params"][:-1]
        declarations = [f"{TYPE_INFO[p['type']][0]} {p['name']}" for p in scalars]
        declarations.append("const char *name")
        words = sum(TYPE_INFO[p["type"]][3] for p in scalars)
        writes = []
        at = 0
        for param in scalars:
            encode, width = TYPE_INFO[param["type"]][2:]
            if width != 1:
                raise RuntimeError(f"{name}: string scalar must be one word")
            writes.append(f"    a[{at}] = {encode.format(name=param['name'])};")
            at += 1
        gl_return = STRING_RETURN_COMMANDS.get(name)
        return_type = TYPE_INFO[gl_return][0] if gl_return else "void"
        failure_value = "0xffffffffu" if gl_return == "GLuint" else "-1"
        failure = f" {failure_value}" if gl_return else ""
        tail = (f"    flush(); return ({return_type})reply_u32();\n"
                if gl_return else "")
        return f"""{return_type} {name}({', '.join(declarations)}) {{
    if (!name) return{failure};
    size_t blob_len = strlen(name);
    uint32_t *a = begin({opcode}, {words}, 4 + blob_len);
    if (!a) return{failure};
{chr(10).join(writes)}
    put_blob(a + {words}, name, blob_len);
{tail}}}
"""
    if name in UNIFORM_VALUE_QUERIES:
        c_type, _ = UNIFORM_VALUE_QUERIES[name]
        return f"""void {name}(unsigned program, int location, {c_type} *params) {{
    uint32_t *a = begin({opcode}, 2, 0);
    if (!a) return;
    a[0] = program; a[1] = (uint32_t)location; flush();
    uint32_t bytes = 0; const uint8_t *source = reply_blob(&bytes);
    if (params && source && bytes <= 16u * sizeof(*params)) memcpy(params, source, bytes);
}}
"""
    if name in CLEAR_VALUE_COMMANDS:
        scalars = command["params"][:-1]
        declarations = [f"{TYPE_INFO[p['type']][0]} {p['name']}" for p in scalars]
        declarations.append("const void *data")
        words = sum(TYPE_INFO[p["type"]][3] for p in scalars)
        writes = []
        at = 0
        for param in scalars:
            pname, ty = param["name"], param["type"]
            encode, width = TYPE_INFO[ty][2:]
            if width == 1:
                writes.append(f"    a[{at}] = {encode.format(name=pname)};")
                at += 1
            else:
                cast = "uint64_t" if ty in {"GLintptr", "GLsizeiptr"} else ""
                value = f"({cast})({pname})" if cast else pname
                writes += [f"    uint64_t bits_{pname};",
                           (f"    bits_{pname} = {value};" if cast else
                            f"    memcpy(&bits_{pname}, &{value}, sizeof(bits_{pname}));"),
                           f"    a[{at}] = (uint32_t)bits_{pname};",
                           f"    a[{at + 1}] = (uint32_t)(bits_{pname} >> 32);"]
                at += 2
        return f"""void {name}({', '.join(declarations)}) {{
    size_t value_bytes = gexgl_pixel_group_bytes(format, type);
    size_t blob_len = data ? value_bytes : 0;
    uint32_t *a = begin({opcode}, {words}, 4 + blob_len);
    if (!a) return;
{chr(10).join(writes)}
    put_blob(a + {words}, data, blob_len);
}}
"""
    if name in PARALLEL_BIND_COMMANDS:
        second_type, third_type = PARALLEL_BIND_COMMANDS[name]
        prefix = "unsigned target, " if name == "glBindBuffersRange" else ""
        third_name = "sizes" if name == "glBindBuffersRange" else "strides"
        scalar_words = 3 if name == "glBindBuffersRange" else 2
        scalar_write = ("    a[0] = target; a[1] = first; a[2] = (uint32_t)count;\n"
                        if name == "glBindBuffersRange" else
                        "    a[0] = first; a[1] = (uint32_t)count;\n")
        return f"""void {name}({prefix}unsigned first, int count, const unsigned *buffers,
                         const {TYPE_INFO[second_type][0]} *offsets,
                         const {TYPE_INFO[third_type][0]} *{third_name}) {{
    if (count < 0 || (buffers && (!offsets || !{third_name}))) return;
    size_t per_item = sizeof(*buffers) + sizeof(*offsets) + sizeof(*{third_name});
    if ((size_t)count > (UINT32_MAX - 12u) / per_item) return;
    size_t names_bytes = buffers ? (size_t)count * sizeof(*buffers) : 0;
    size_t offsets_bytes = buffers ? (size_t)count * sizeof(*offsets) : 0;
    size_t third_bytes = buffers ? (size_t)count * sizeof(*{third_name}) : 0;
    size_t tail = 12 + ((names_bytes + 3) & ~3u) +
                  ((offsets_bytes + 3) & ~3u) + ((third_bytes + 3) & ~3u);
    uint32_t *a = begin({opcode}, {scalar_words}, tail);
    if (!a) return;
{scalar_write}    uint32_t *wire = a + {scalar_words};
    put_blob(wire, buffers, names_bytes); wire += 1 + (names_bytes + 3) / 4;
    put_blob(wire, offsets, offsets_bytes); wire += 1 + (offsets_bytes + 3) / 4;
    put_blob(wire, {third_name}, third_bytes);
}}
"""
    if name == "glPixelStoref":
        return f"""void {name}(unsigned pname, float param) {{
    int rounded = 0;
    if (param == param && param >= (float)INT_MIN && param <= (float)INT_MAX) {{
        rounded = (int)(param >= 0.0f ? param + 0.5f : param - 0.5f);
        (void)gexgl_set_pixel_store(&pack_store, &unpack_store, pname, rounded);
    }}
    uint32_t *packet = begin({opcode}, 2, 0);
    if (!packet) return;
    packet[0] = pname; packet[1] = as_bits(param);
}}
"""
    if name == "glBindSamplers":
        return f"""void {name}(unsigned first, int count, const unsigned *samplers) {{
    if (count < 0 || (size_t)count > UINT32_MAX / sizeof(*samplers)) return;
    size_t blob_len = samplers ? (size_t)count * sizeof(*samplers) : 0;
    uint32_t *a = begin({opcode}, 2, 4 + blob_len);
    if (!a) return;
    a[0] = first; a[1] = (uint32_t)count; put_blob(a + 2, samplers, blob_len);
}}
"""
    if name in MULTI_BIND_NAME_ARRAYS:
        scalars = command["params"][:-1]
        declarations = [f"{TYPE_INFO[p['type']][0]} {p['name']}" for p in scalars]
        declarations.append("const unsigned *names")
        writes = []
        at = 0
        for param in scalars:
            encode, width = TYPE_INFO[param["type"]][2:]
            if width != 1:
                raise RuntimeError(f"{name}: multi-bind scalar must be one word")
            writes.append(f"    a[{at}] = {encode.format(name=param['name'])};")
            at += 1
        return f"""void {name}({', '.join(declarations)}) {{
    if (count < 0 || (size_t)count > UINT32_MAX / sizeof(*names)) return;
    size_t blob_len = names ? (size_t)count * sizeof(*names) : 0;
    uint32_t *a = begin({opcode}, {len(scalars)}, 4 + blob_len);
    if (!a) return;
{chr(10).join(writes)}
    put_blob(a + {len(scalars)}, names, blob_len);
}}
"""
    if name in TEXTURE_PARAMETER_ARRAYS:
        pointer = command["params"][-1]
        declarations = []
        for param in command["params"][:-1]:
            declarations.append(f"{TYPE_INFO[param['type']][0]} {param['name']}")
        declarations.append(f"const {TYPE_INFO[pointer['type']][0]} *{pointer['name']}")
        return f"""void {name}({', '.join(declarations)}) {{
    size_t components = pname == 0x1004u ? 4u : 1u; /* GL_TEXTURE_BORDER_COLOR */
    size_t blob_len = {pointer['name']} ? components * sizeof(*{pointer['name']}) : 0;
    uint32_t *a = begin({opcode}, 2, 4 + blob_len);
    if (!a) return;
    a[0] = {command['params'][0]['name']}; a[1] = pname;
    put_blob(a + 2, {pointer['name']}, blob_len);
}}
"""
    if name in INTERNAL_FORMAT_QUERIES:
        c_type = "long long" if name.endswith("i64v") else "int"
        return f"""void {name}(unsigned target, unsigned internalformat, unsigned pname,
                                  int count, {c_type} *params) {{
    if (count < 0 || count > 4096) return;
    uint32_t *a = begin({opcode}, 4, 0);
    if (!a) return;
    a[0] = target; a[1] = internalformat; a[2] = pname; a[3] = (uint32_t)count;
    flush();
    uint32_t bytes = 0; const uint8_t *source = reply_blob(&bytes);
    if (params && source && bytes <= (uint32_t)count * sizeof(*params)) memcpy(params, source, bytes);
}}
"""
    if name == "glGetMultisamplefv":
        return f"""void {name}(unsigned pname, unsigned index, float *val) {{
    uint32_t *a = begin({opcode}, 2, 0);
    if (!a) return;
    a[0] = pname; a[1] = index; flush();
    uint32_t bytes = 0; const uint8_t *source = reply_blob(&bytes);
    if (val && source && bytes == 2u * sizeof(*val)) memcpy(val, source, bytes);
}}
"""
    if name == "glGetCompressedTexImage":
        return f"""void {name}(unsigned target, int level, void *img) {{
    int pbo = bound_buffer(0x88EBu) != 0; /* GL_PIXEL_PACK_BUFFER */
    uint32_t *a = begin({opcode}, 5, 0);
    if (!a) return;
    uint64_t pointer = (uint64_t)(uintptr_t)img;
    a[0] = target; a[1] = (uint32_t)level; a[2] = (uint32_t)pbo;
    a[3] = (uint32_t)pointer; a[4] = (uint32_t)(pointer >> 32); flush();
    if (pbo) return;
    uint32_t bytes = 0; const uint8_t *source = reply_blob(&bytes);
    if (img && source && bytes) memcpy(img, source, bytes);
}}
"""
    if name in MULTI_ARRAY_COMMANDS:
        return f"""void {name}(unsigned mode, const int *first, const int *count,
                              int drawcount) {{
    if (drawcount > 0 && (!first || !count)) return;
    if (drawcount > 0 && (size_t)drawcount > UINT32_MAX / 8u) return;
    size_t blob_len = drawcount > 0 ? (size_t)drawcount * 8u : 0;
    uint32_t *a = begin({opcode}, 2, 4 + blob_len);
    if (!a) return;
    a[0] = mode; a[1] = (uint32_t)drawcount; a[2] = (uint32_t)blob_len;
    uint8_t *wire = (uint8_t *)(a + 3);
    for (int i = 0; i < drawcount; i++) {{
        int32_t first_value = first[i], count_value = count[i];
        memcpy(wire, &first_value, 4); wire += 4;
        memcpy(wire, &count_value, 4); wire += 4;
    }}
}}
"""
    if name in MULTI_ELEMENT_COMMANDS:
        with_base = name == "glMultiDrawElementsBaseVertex"
        declaration = ("unsigned mode, const int *count, unsigned type, "
                       "const void *const *indices, int drawcount" +
                       (", const int *basevertex" if with_base else ""))
        required = "count && indices" + (" && basevertex" if with_base else "")
        base_write = ("\n        int32_t base = basevertex[i]; memcpy(wire, &base, 4); wire += 4;"
                      if with_base else "")
        stride = 16 if with_base else 12
        return f"""void {name}({declaration}) {{
    if (drawcount < 0 || (drawcount > 0 && !({required}))) return;
    if ((size_t)drawcount > UINT32_MAX / {stride}u) return;
    size_t blob_len = (size_t)drawcount * {stride}u;
    uint32_t *a = begin({opcode}, 3, 4 + blob_len);
    if (!a) return;
    a[0] = mode; a[1] = type; a[2] = (uint32_t)drawcount; a[3] = (uint32_t)blob_len;
    uint8_t *wire = (uint8_t *)(a + 4);
    for (int i = 0; i < drawcount; i++) {{
        int32_t n = count[i]; memcpy(wire, &n, 4); wire += 4;
        uint64_t offset = (uint64_t)(uintptr_t)indices[i];
        memcpy(wire, &offset, 8); wire += 8;{base_write}
    }}
}}
"""
    if name == "glGetTexImage":
        return f"""void {name}(unsigned target, int level, unsigned format, unsigned type,
                       void *pixels) {{
    int pbo = bound_buffer(0x88EBu) != 0; /* GL_PIXEL_PACK_BUFFER */
    uint32_t *a = begin({opcode}, 7, 0);
    if (!a) return;
    a[0] = target; a[1] = (uint32_t)level; a[2] = format; a[3] = type;
    a[4] = (uint32_t)pbo;
    a[5] = (uint32_t)(uintptr_t)pixels;
    a[6] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    flush();
    if (pbo) return;
    /* Row by row, exactly the span GL defines as written -- not the whole
       [offset, end) run. The padding GL_PACK_ROW_LENGTH, _SKIP_PIXELS and
       _ALIGNMENT leave between rows belongs to the caller and must survive:
       copying the run wholesale overwrote it with the zero-fill of the host's
       staging vector, which is precisely what packed_pixels checks when it
       varies the pack modes. glReadPixels has always scattered this way; this
       did not. */
    uint32_t offset = reply_u32();
    uint32_t row_stride = reply_u32();
    uint32_t row_bytes = reply_u32();
    uint32_t rows = reply_u32();
    uint32_t image_stride = reply_u32();
    uint32_t images = reply_u32();
    uint32_t bytes = 0; const uint8_t *source = reply_blob(&bytes);
    if (!pixels || !source || !bytes) return;
    for (uint32_t image = 0; image < images; ++image) {{
        for (uint32_t row = 0; row < rows; ++row) {{
            uint64_t at = (uint64_t)image * image_stride +
                          (uint64_t)row * row_stride;
            if (at > bytes || row_bytes > bytes - at) return;
            memcpy((uint8_t *)pixels + offset + at, source + at, row_bytes);
        }}
    }}
}}
"""
    if name == "glTransformFeedbackVaryings":
        return f"""void {name}(unsigned program, int count, const char *const *varyings,
                                 unsigned bufferMode) {{
    size_t blob_len = 0;
    if (varyings && count > 0) {{
        for (int i = 0; i < count; i++) {{
            if (!varyings[i]) return;
            size_t bytes = strlen(varyings[i]) + 1;
            if (bytes > SIZE_MAX - blob_len) return;
            blob_len += bytes;
        }}
    }}
    uint32_t *a = begin({opcode}, 4, blob_len);
    if (!a) return;
    a[0] = program; a[1] = (uint32_t)count; a[2] = bufferMode;
    uint8_t *wire = (uint8_t *)(a + 4);
    a[3] = (uint32_t)blob_len;
    for (int i = 0; i < count; i++) {{
        size_t bytes = strlen(varyings[i]) + 1;
        memcpy(wire, varyings[i], bytes); wire += bytes;
    }}
}}
"""
    if name == "glPatchParameterfv":
        return f"""void {name}(unsigned pname, const float *values) {{
    size_t count = pname == 0x8e73u ? 2u : pname == 0x8e74u ? 4u : 0u;
    size_t blob_len = values ? count * sizeof(*values) : 0;
    uint32_t *a = begin({opcode}, 2, blob_len);
    if (!a) return;
    a[0] = pname; put_blob(a + 1, values, blob_len);
}}
"""
    if name in POINT_PARAMETER_ARRAYS:
        value_type = "float" if name.endswith("fv") else "int"
        return f"""void {name}(unsigned pname, const {value_type} *params) {{
    size_t blob_len = params ? sizeof(*params) : 0;
    uint32_t *a = begin({opcode}, 1, 4 + blob_len);
    if (!a) return;
    a[0] = pname; put_blob(a + 1, params, blob_len);
}}
"""
    if name == "glTexImage1D":
        return f"""void {name}(unsigned target, int level, int internalformat, int width, int border,
                         unsigned format, unsigned type, const void *pixels) {{
    int pbo = bound_buffer(0x88ECu) != 0; /* GL_PIXEL_UNPACK_BUFFER */
    size_t blob_len = pixels && !pbo ? pixel_transfer_bytes(format, type, width, 1, -1,
                                                             &unpack_store, NULL) : 0;
    uint32_t *a = begin({opcode}, 10, 4 + blob_len);
    if (!a) return;
    a[0] = target; a[1] = (uint32_t)level; a[2] = (uint32_t)internalformat;
    a[3] = (uint32_t)width; a[4] = (uint32_t)border; a[5] = format; a[6] = type;
    a[7] = (uint32_t)pbo;
    a[8] = (uint32_t)(uintptr_t)pixels;
    a[9] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(a + 10, pbo ? NULL : pixels, blob_len);
}}
"""
    if name == "glTexSubImage1D":
        return f"""void {name}(unsigned target, int level, int xoffset, int width,
                            unsigned format, unsigned type, const void *pixels) {{
    int pbo = bound_buffer(0x88ECu) != 0; /* GL_PIXEL_UNPACK_BUFFER */
    size_t blob_len = pixels && !pbo ? pixel_transfer_bytes(format, type, width, 1, -1,
                                                             &unpack_store, NULL) : 0;
    uint32_t *a = begin({opcode}, 9, 4 + blob_len);
    if (!a) return;
    a[0] = target; a[1] = (uint32_t)level; a[2] = (uint32_t)xoffset;
    a[3] = (uint32_t)width; a[4] = format; a[5] = type; a[6] = (uint32_t)pbo;
    a[7] = (uint32_t)(uintptr_t)pixels;
    a[8] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(a + 9, pbo ? NULL : pixels, blob_len);
}}
"""
    if name == "glTexSubImage3D":
        return f"""void {name}(unsigned target, int level, int xoffset, int yoffset, int zoffset,
                            int width, int height, int depth, unsigned format, unsigned type,
                            const void *pixels) {{
    int pbo = bound_buffer(0x88ECu) != 0; /* GL_PIXEL_UNPACK_BUFFER */
    size_t blob_len = pixels && !pbo ? pixel_transfer_bytes(format, type, width, height, depth,
                                                             &unpack_store, NULL) : 0;
    uint32_t *a = begin({opcode}, 13, 4 + blob_len);
    if (!a) return;
    a[0] = target; a[1] = (uint32_t)level; a[2] = (uint32_t)xoffset;
    a[3] = (uint32_t)yoffset; a[4] = (uint32_t)zoffset;
    a[5] = (uint32_t)width; a[6] = (uint32_t)height; a[7] = (uint32_t)depth;
    a[8] = format; a[9] = type; a[10] = (uint32_t)pbo;
    a[11] = (uint32_t)(uintptr_t)pixels;
    a[12] = (uint32_t)((uint64_t)(uintptr_t)pixels >> 32);
    put_blob(a + 13, pbo ? NULL : pixels, blob_len);
}}
"""
    if name == "glGetBufferSubData":
        return f"""void {name}(unsigned target, long offset, long size, void *data) {{
    uint32_t *a = begin({opcode}, 5, 0);
    if (!a) return;
    uint64_t off = (uint64_t)offset, bytes = (uint64_t)size;
    a[0] = target; a[1] = (uint32_t)off; a[2] = (uint32_t)(off >> 32);
    a[3] = (uint32_t)bytes; a[4] = (uint32_t)(bytes >> 32); flush();
    uint32_t reply_len = 0; const uint8_t *reply = reply_blob(&reply_len);
    if (data && size > 0 && reply && reply_len == (uint64_t)size) memcpy(data, reply, reply_len);
}}
"""
    if name in INDEXED_INTEGER_QUERIES:
        output_type = "long long" if name == "glGetInteger64i_v" else "int"
        reply = ("uint64_t value = reply_u32(); value |= (uint64_t)reply_u32() << 32;\n"
                 "    if (data) *data = (long long)value;"
                 if name == "glGetInteger64i_v" else
                 "if (data) *data = (int)reply_u32();")
        return f"""void {name}(unsigned target, unsigned index, {output_type} *data) {{
    uint32_t *a = begin({opcode}, 2, 0);
    if (!a) return;
    a[0] = target; a[1] = index; flush();
    {reply}
}}
"""
    if name == "glGetProgramInterfaceiv":
        return f"""void {name}(unsigned program, unsigned programInterface, unsigned pname, int *params) {{
    uint32_t *a = begin({opcode}, 3, 0);
    if (!a) return;
    a[0] = program; a[1] = programInterface; a[2] = pname;
    flush();
    if (params) *params = (int)reply_u32();
}}
"""
    if name in PROGRAM_NAMED_QUERIES:
        return_type = "unsigned" if name == "glGetProgramResourceIndex" else "int"
        failure = "0xffffffffu" if name == "glGetProgramResourceIndex" else "-1"
        return f"""{return_type} {name}(unsigned program, unsigned programInterface, const char *name) {{
    if (!name) return {failure};
    size_t blob_len = strlen(name);
    uint32_t *a = begin({opcode}, 2, 4 + blob_len);
    if (!a) return {failure};
    a[0] = program; a[1] = programInterface; put_blob(a + 2, name, blob_len);
    flush();
    return ({return_type})reply_u32();
}}
"""
    if name == "glGetProgramResourceName":
        return f"""void {name}(unsigned program, unsigned programInterface, unsigned index, int bufSize,
                              int *length, char *name) {{
    uint32_t *a = begin({opcode}, 3, 0);
    if (!a) return;
    a[0] = program; a[1] = programInterface; a[2] = index;
    flush();
    uint32_t bytes = 0; const uint8_t *source = reply_blob(&bytes);
    uint32_t copy = bytes;
    if (copy >= (uint32_t)(bufSize > 0 ? bufSize : 0)) copy = bufSize > 0 ? (uint32_t)bufSize - 1 : 0;
    if (name && bufSize > 0) {{ if (source && copy) memcpy(name, source, copy); name[copy] = 0; }}
    if (length) *length = (int)copy;
}}
"""
    if name == "glGetProgramResourceiv":
        return f"""void {name}(unsigned program, unsigned programInterface, unsigned index, int propCount,
                            const unsigned *props, int bufSize, int *length, int *params) {{
    size_t blob_len = props && propCount > 0 ? (size_t)propCount * sizeof(*props) : 0;
    uint32_t *a = begin({opcode}, 5, 4 + blob_len);
    if (!a) return;
    a[0] = program; a[1] = programInterface; a[2] = index;
    a[3] = (uint32_t)propCount; a[4] = (uint32_t)bufSize; put_blob(a + 5, props, blob_len);
    flush();
    uint32_t count = reply_u32(), bytes = 0; const uint8_t *source = reply_blob(&bytes);
    uint32_t available = bytes / sizeof(int);
    if (count > available) count = available;
    if (count > (uint32_t)(bufSize > 0 ? bufSize : 0)) count = bufSize > 0 ? (uint32_t)bufSize : 0;
    if (params && source && count) memcpy(params, source, count * sizeof(int));
    if (length) *length = (int)count;
}}
"""
    if name in RESOURCE_OUTPUT_QUERIES:
        output = command["params"][-1]
        scalar_params = command["params"][:-1]
        declarations = [f"{TYPE_INFO[p['type']][0]} {p['name']}" for p in scalar_params]
        declarations.append(f"{TYPE_INFO[output['type']][0]} *{output['name']}")
        words = sum(TYPE_INFO[p["type"]][3] for p in scalar_params)
        components = output_component_expression(RESOURCE_OUTPUT_QUERIES[name])
        # Sized before anything is put in the ring. gexgl_state_query_components
        # answers three pnames -- GL_COMPRESSED_TEXTURE_FORMATS,
        # GL_PROGRAM_BINARY_FORMATS, GL_SHADER_BINARY_FORMATS -- with a nested
        # glGetIntegerv, which appends its own command and flushes. Doing that
        # after begin() would send this command early and read the nested
        # query's reply as if it were this one's; doing it after reply_blob()
        # would overwrite the reply the returned pointer still refers to, and
        # then memcpy up to 4096 elements into the caller's array. Asking
        # first makes the nested request strictly complete before this one
        # starts.
        lines = [f"void {name}({', '.join(declarations)}) {{",
                 f"    const size_t expected = (size_t){components} * sizeof(*{output['name']});",
                 f"    uint32_t *packet = begin({opcode}, {words}, 0);",
                 "    if (!packet) return;",
                 "    size_t at = 0;"]
        for param in scalar_params:
            param_name, ty = param["name"], param["type"]
            encode, width = TYPE_INFO[ty][2:]
            if width != 1:
                raise RuntimeError(f"{name}: query scalar {param_name} is not one wire word")
            lines.append(f"    packet[at++] = {encode.format(name=param_name)};")
        lines += ["    flush();",
                  "    uint32_t reply_len = 0; const uint8_t *source = reply_blob(&reply_len);",
                  f"    if ({output['name']} && source && reply_len == expected)",
                  f"        memcpy({output['name']}, source, expected);",
                  "}", ""]
        return "\n".join(lines)
    params = command["params"]
    def declaration_for(param: dict) -> str:
        if not param["pointer"]:
            return f"{TYPE_INFO[param['type']][0]} {param['name']}"
        if command["name"] in BLOB_COMMANDS or command["name"] in OPAQUE_OFFSET_COMMANDS:
            return f"const void *{param['name']}"
        return f"const {TYPE_INFO[param['type']][0]} *{param['name']}"
    declaration = ", ".join(declaration_for(param) for param in params) or "void"
    scalar_params = [param for param in params if not param["pointer"]]
    words = sum(TYPE_INFO[param["type"]][3] for param in scalar_params)
    pointer = next((param for param in params if param["pointer"]), None)
    tail = "0"
    if pointer and command["name"] in OPAQUE_OFFSET_COMMANDS:
        words += 2
    elif pointer:
        length = pointer["length"]
        tail = "blob_len"
    return_type = TYPE_INFO[command["return_type"]][0] if command["name"] in RETURN_COMMANDS else "void"
    lines = [f"{return_type} {command['name']}({declaration}) {{"]
    if pointer and command["name"] not in OPAQUE_OFFSET_COMMANDS:
        if command["name"] in BLOB_COMMANDS:
            lines.append("    int pbo = bound_buffer(0x88ECu) != 0; /* GL_PIXEL_UNPACK_BUFFER */")
            lines.append(
                f"    size_t blob_len = !pbo && {pointer['name']} && {length} > 0 ? (size_t)({length}) : 0;"
            )
        else:
            count_name, components = command_array_length(command["name"], length)
            if count_name:
                lines += ["    size_t blob_len = 0;",
                          f"    if ({pointer['name']} && {count_name} > 0 &&",
                          f"        (size_t){count_name} <= SIZE_MAX / ({components}u * sizeof(*{pointer['name']})))",
                          f"        blob_len = (size_t){count_name} * {components}u * sizeof(*{pointer['name']});"]
            else:
                lines.append(
                    f"    size_t blob_len = {pointer['name']} ? {components}u * sizeof(*{pointer['name']}) : 0;"
                )
    failure_return = "0" if command["name"] in RETURN_COMMANDS else ""
    blob_word = (4 if pointer and command["name"] in BLOB_COMMANDS else
                 1 if pointer and command["name"] not in OPAQUE_OFFSET_COMMANDS else 0)
    lines += [f"    uint32_t *packet = begin({opcode}, {words + blob_word}, {tail});",
             f"    if (!packet) return{(' ' + failure_return) if failure_return else ''};"]
    if scalar_params or pointer:
        lines.append("    size_t at = 0;")
    for param in scalar_params:
        name, ty = param["name"], param["type"]
        encode, width = TYPE_INFO[ty][2:]
        if width == 1:
            lines.append(f"    packet[at++] = {encode.format(name=name)};")
        else:
            cast = "uint64_t" if ty in {"GLintptr", "GLsizeiptr"} else ""
            value = f"({cast})({name})" if cast else name
            lines += [f"    uint64_t bits_{name};",
                      f"    memcpy(&bits_{name}, &{value}, sizeof(bits_{name}));" if not cast else f"    bits_{name} = {value};",
                      f"    packet[at++] = (uint32_t)bits_{name};",
                      f"    packet[at++] = (uint32_t)(bits_{name} >> 32);"]
    if pointer and command["name"] in OPAQUE_OFFSET_COMMANDS:
        lines += [f"    uint64_t bits_{pointer['name']} = (uint64_t)(uintptr_t){pointer['name']};",
                  f"    packet[at++] = (uint32_t)bits_{pointer['name']};",
                  f"    packet[at++] = (uint32_t)(bits_{pointer['name']} >> 32);"]
    elif pointer and command["name"] in BLOB_COMMANDS:
        lines += ["    packet[at++] = (uint32_t)pbo;",
                  f"    uint64_t bits_{pointer['name']} = (uint64_t)(uintptr_t){pointer['name']};",
                  f"    packet[at++] = (uint32_t)bits_{pointer['name']};",
                  f"    packet[at++] = (uint32_t)(bits_{pointer['name']} >> 32);",
                  f"    put_blob(packet + at, {pointer['name']}, blob_len);"]
    elif pointer:
        lines.append(f"    put_blob(packet + at, {pointer['name']}, blob_len);")
    if command["name"] in RETURN_COMMANDS:
        lines += ["    flush();", f"    return ({return_type})reply_u32();"]
    lines += ["}", ""]
    return "\n".join(lines)


def host_case(command: dict, opcode: int) -> str:
    name = command["name"]
    if name in PROGRAM_PIPELINE_GENERATORS:
        return f"""case {opcode}: {{
    GLsizei count = a.i32();
    if (count < 0) {{ GEX_GL_CALL(glGenProgramPipelines, count, nullptr); break; }}
    std::vector<GLuint> host((size_t)count);
    GEX_GL_CALL(glGenProgramPipelines, count, host.data());
    for (GLsizei i = 0; i < count; ++i) program_pipelines[a.u32()] = host[(size_t)i];
    break;
}}"""
    if name in PROGRAM_PIPELINE_DELETERS:
        return f"""case {opcode}: {{
    GLsizei count = a.i32();
    if (count < 0) {{ GEX_GL_CALL(glDeleteProgramPipelines, count, nullptr); break; }}
    std::vector<GLuint> host; host.reserve((size_t)count);
    for (GLsizei i = 0; i < count; ++i) {{
        GLuint guest = a.u32(); host.push_back(lookup(program_pipelines, guest));
        program_pipelines.erase(guest);
    }}
    GEX_GL_CALL(glDeleteProgramPipelines, count, host.data());
    break;
}}"""
    if name in BUFFER_STORAGE_COMMANDS:
        return f"""case {opcode}: {{
    GLenum target = a.u32(); uint64_t size = a.u64(); GLbitfield flags = a.u32();
    const bool has_data = a.u32() != 0; uint32_t bytes = 0;
    const uint8_t *wire = a.blob(bytes);
    if (size > (uint64_t)std::numeric_limits<GLsizeiptr>::max() ||
        (has_data && size != bytes) || (!has_data && bytes != 0)) {{ a.ok = false; break; }}
    GEX_GL_CALL(glBufferStorage, target, (GLsizeiptr)size, has_data ? wire : nullptr, flags);
    break;
}}"""
    if name in STRING_INPUT_COMMANDS | STRING_RETURN_COMMANDS.keys():
        scalars = command["params"][:-1]
        reads = []
        calls = []
        for param in scalars:
            pname, ty = param["name"], param["type"]
            reads.append(f"    {ty} {pname} = a.{TYPE_INFO[ty][1]}();")
            namespace = OBJECT_COMMANDS.get(name, {}).get(pname)
            calls.append(f"lookup({namespace}, {pname})" if namespace else pname)
        reply_call = f"put_u32((uint32_t)GEX_GL_CALL({name}, {', '.join(calls + ['value.c_str()'])}));" if name in STRING_RETURN_COMMANDS else f"GEX_GL_CALL({name}, {', '.join(calls + ['value.c_str()'])});"
        return f"""case {opcode}: {{
{chr(10).join(reads)}
    uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    std::string value(reinterpret_cast<const char *>(wire), bytes);
    {reply_call}
    break;
}}"""
    if name in UNIFORM_VALUE_QUERIES:
        _, gl_type = UNIFORM_VALUE_QUERIES[name]
        return f"""case {opcode}: {{
    GLuint program = lookup(programs, a.u32()); GLint location = a.i32();
    uint32_t components = uniform_components(program, location);
    if (components > 16) {{ a.ok = false; break; }}
    {gl_type} values[16]{{}};
    GEX_GL_CALL({name}, program, location, values);
    put_blob(values, components * sizeof(values[0]));
    break;
}}"""
    if name in CLEAR_VALUE_COMMANDS:
        scalars = command["params"][:-1]
        reads = []
        calls = []
        for param in scalars:
            pname, ty = param["name"], param["type"]
            reads.append(f"    {ty} {pname} = a.{TYPE_INFO[ty][1]}();")
            namespace = OBJECT_COMMANDS.get(name, {}).get(pname)
            calls.append(f"lookup({namespace}, {pname})" if namespace else pname)
        return f"""case {opcode}: {{
{chr(10).join(reads)}
    uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    size_t expected = gexgl_pixel_group_bytes(format, type);
    if (bytes && bytes != expected) {{ a.ok = false; break; }}
    std::vector<uint64_t> aligned((bytes + sizeof(uint64_t) - 1) / sizeof(uint64_t));
    if (bytes) memcpy(aligned.data(), wire, bytes);
    GEX_GL_CALL({name}, {', '.join(calls)}, bytes ? aligned.data() : nullptr);
    break;
}}"""
    if name in PARALLEL_BIND_COMMANDS:
        second_type, third_type = PARALLEL_BIND_COMMANDS[name]
        third_name = "sizes" if name == "glBindBuffersRange" else "strides"
        prefix_read = ("    GLenum target = a.u32(); GLuint first = a.u32(); GLsizei count = a.i32();"
                       if name == "glBindBuffersRange" else
                       "    GLuint first = a.u32(); GLsizei count = a.i32();")
        call_prefix = "target, first, count" if name == "glBindBuffersRange" else "first, count"
        return f"""case {opcode}: {{
{prefix_read}
    uint32_t name_bytes = 0, offset_bytes = 0, third_bytes = 0;
    const uint8_t *name_wire = a.blob(name_bytes);
    const uint8_t *offset_wire = a.blob(offset_bytes);
    const uint8_t *third_wire = a.blob(third_bytes);
    if (count < 0 || (name_bytes && (name_bytes != (uint32_t)count * sizeof(GLuint) ||
        offset_bytes != (uint32_t)count * sizeof({second_type}) ||
        third_bytes != (uint32_t)count * sizeof({third_type}))) ||
        (!name_bytes && (offset_bytes || third_bytes))) {{ a.ok = false; break; }}
    std::vector<GLuint> names((size_t)std::max(count, 0));
    std::vector<{second_type}> offsets((size_t)std::max(count, 0));
    std::vector<{third_type}> {third_name}((size_t)std::max(count, 0));
    for (GLsizei i = 0; name_bytes && i < count; ++i) {{
        GLuint guest = 0; memcpy(&guest, name_wire + (size_t)i * 4, 4);
        names[(size_t)i] = lookup(buffers, guest);
    }}
    if (offset_bytes) memcpy(offsets.data(), offset_wire, offset_bytes);
    if (third_bytes) memcpy({third_name}.data(), third_wire, third_bytes);
    GEX_GL_CALL({name}, {call_prefix}, name_bytes ? names.data() : nullptr,
                name_bytes ? offsets.data() : nullptr,
                name_bytes ? {third_name}.data() : nullptr);
    break;
}}"""
    if name == "glBindSamplers":
        return f"""case {opcode}: {{
    GLuint first = a.u32(); GLsizei count = a.i32(); uint32_t bytes = 0;
    const uint8_t *wire = a.blob(bytes);
    if (count < 0 || (bytes && bytes != (uint32_t)count * sizeof(GLuint))) {{ a.ok = false; break; }}
    std::vector<GLuint> names((size_t)count, 0);
    if (bytes) {{
        const GLuint *guest = reinterpret_cast<const GLuint *>(wire);
        for (GLsizei i = 0; i < count; ++i) names[(size_t)i] = lookup(samplers, guest[i]);
    }}
    glBindSamplers(first, count, bytes ? names.data() : nullptr);
    break;
}}"""
    if name in MULTI_BIND_NAME_ARRAYS:
        namespace = MULTI_BIND_NAME_ARRAYS[name]
        scalars = command["params"][:-1]
        reads = [f"    {p['type']} {p['name']} = a.{TYPE_INFO[p['type']][1]}();" for p in scalars]
        call_args = [p["name"] for p in scalars]
        return f"""case {opcode}: {{
{chr(10).join(reads)}
    uint32_t bytes = 0;
    const uint8_t *wire = a.blob(bytes);
    if (count < 0 || (bytes && bytes != (uint32_t)count * sizeof(GLuint))) {{ a.ok = false; break; }}
    std::vector<GLuint> host((size_t)std::max(count, 0), 0);
    if (bytes) {{
        for (GLsizei i = 0; i < count; ++i) {{
            GLuint guest = 0; memcpy(&guest, wire + (size_t)i * sizeof(guest), sizeof(guest));
            host[(size_t)i] = lookup({namespace}, guest);
        }}
    }}
    GEX_GL_CALL({name}, {', '.join(call_args)}, bytes ? host.data() : nullptr);
    break;
}}"""
    if name in TEXTURE_PARAMETER_ARRAYS:
        pointer = command["params"][-1]
        scalar0 = command["params"][0]
        namespace = "samplers" if name.startswith("glSampler") else None
        first = f"lookup({namespace}, first)" if namespace else "first"
        return f"""case {opcode}: {{
    {scalar0['type']} first = a.{TYPE_INFO[scalar0['type']][1]}(); GLenum pname = a.u32();
    uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    const uint32_t expected = (pname == GL_TEXTURE_BORDER_COLOR ? 4u : 1u) * sizeof({pointer['type']});
    if (bytes != expected) {{ a.ok = false; break; }}
    {name}({first}, pname, reinterpret_cast<const {pointer['type']} *>(wire));
    break;
}}"""
    if name in INTERNAL_FORMAT_QUERIES:
        output_type = "GLint64" if name.endswith("i64v") else "GLint"
        return f"""case {opcode}: {{
    GLenum target = a.u32(), internalformat = a.u32(), pname = a.u32(); GLsizei count = a.i32();
    if (count < 0 || count > 4096) {{ a.ok = false; break; }}
    std::vector<{output_type}> values((size_t)count, 0);
    GEX_GL_CALL({name}, target, internalformat, pname, count, values.data());
    put_blob(values.data(), values.size() * sizeof({output_type}));
    break;
}}"""
    if name == "glGetMultisamplefv":
        return f"""case {opcode}: {{
    GLenum pname = a.u32(); GLuint index = a.u32(); GLfloat values[2]{{}};
    glGetMultisamplefv(pname, index, values); put_blob(values, sizeof(values));
    break;
}}"""
    if name == "glGetCompressedTexImage":
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLint level = a.i32(); const bool pbo = a.u32() != 0;
    const uintptr_t pointer = static_cast<uintptr_t>(a.u64());
    if (pbo) {{ glGetCompressedTexImage(target, level, reinterpret_cast<void *>(pointer)); break; }}
    GLint size = 0; glGetTexLevelParameteriv(target, level, GL_TEXTURE_COMPRESSED_IMAGE_SIZE, &size);
    if (size < 0 || size > (1 << 28)) {{ a.ok = false; break; }}
    std::vector<uint8_t> image((size_t)size); glGetCompressedTexImage(target, level, image.data());
    put_blob(image.data(), image.size());
    break;
}}"""
    if name in MULTI_ARRAY_COMMANDS:
        return f"""case {opcode}: {{
    GLenum mode = a.u32(); GLsizei drawcount = a.i32();
    uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    if (drawcount < 0) {{
        if (bytes != 0) {{ a.ok = false; break; }}
        {name}(mode, nullptr, nullptr, drawcount);
        break;
    }}
    if ((uint64_t)drawcount * 8u != bytes) {{ a.ok = false; break; }}
    std::vector<GLint> first((size_t)drawcount);
    std::vector<GLsizei> counts((size_t)drawcount);
    for (GLsizei i = 0; i < drawcount; i++) {{
        memcpy(&first[(size_t)i], wire, 4); wire += 4;
        memcpy(&counts[(size_t)i], wire, 4); wire += 4;
    }}
    {name}(mode, first.data(), counts.data(), drawcount);
    break;
}}"""
    if name in MULTI_ELEMENT_COMMANDS:
        with_base = name == "glMultiDrawElementsBaseVertex"
        stride = 16 if with_base else 12
        base_declarations = (" std::vector<GLint> basevertices((size_t)drawcount);"
                             if with_base else "")
        base_read = ("\n        memcpy(&basevertices[(size_t)i], wire, 4); wire += 4;"
                     if with_base else "")
        base_argument = ", basevertices.data()" if with_base else ""
        return f"""case {opcode}: {{
    GLenum mode = a.u32(), type = a.u32(); GLsizei drawcount = a.i32();
    uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    if (drawcount < 0 || (uint64_t)drawcount * {stride}u != bytes) {{ a.ok = false; break; }}
    std::vector<GLsizei> counts((size_t)drawcount);
    std::vector<const void *> indices((size_t)drawcount);{base_declarations}
    for (GLsizei i = 0; i < drawcount; i++) {{
        memcpy(&counts[(size_t)i], wire, 4); wire += 4;
        uint64_t offset = 0; memcpy(&offset, wire, 8); wire += 8;
        indices[(size_t)i] = reinterpret_cast<const void *>(static_cast<uintptr_t>(offset));{base_read}
    }}
    {name}(mode, counts.data(), type, indices.data(), drawcount{base_argument});
    break;
}}"""
    if name == "glGetTexImage":
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLint level = a.i32(); GLenum format = a.u32(), type = a.u32();
    const bool pbo = a.u32() != 0; const uintptr_t pointer = static_cast<uintptr_t>(a.u64());
    if (pbo) {{ glGetTexImage(target, level, format, type, reinterpret_cast<void*>(pointer)); break; }}
    GLint width = 0, height = 0, depth = 1;
    glGetTexLevelParameteriv(target, level, GL_TEXTURE_WIDTH, &width);
    glGetTexLevelParameteriv(target, level, GL_TEXTURE_HEIGHT, &height);
    glGetTexLevelParameteriv(target, level, GL_TEXTURE_DEPTH, &depth);
    if (depth < 1) depth = 1;
    const bool threeDimensional = target == GL_TEXTURE_3D || target == GL_TEXTURE_2D_ARRAY ||
                                  target == GL_TEXTURE_CUBE_MAP_ARRAY;
    gexgl_pixel_layout layout{{}};
    if (!gexgl_compute_pixel_layout(format, type, width, height, threeDimensional ? depth : -1,
                                    &pack_store, &layout) ||
        layout.end > UINT32_MAX) {{ a.ok = false; break; }}
    std::vector<uint8_t> pixels(layout.end, 0);
    glGetTexImage(target, level, format, type, pixels.data());
    const size_t group = gexgl_pixel_group_bytes(format, type);
    const uint32_t rows = height > 0 ? static_cast<uint32_t>(height) : 0;
    const uint32_t images = threeDimensional && depth > 0
        ? static_cast<uint32_t>(depth) : 1;
    put_u32(static_cast<uint32_t>(layout.offset));
    put_u32(static_cast<uint32_t>(layout.row_stride));
    put_u32(static_cast<uint32_t>(group * (width > 0 ? (size_t)width : 0)));
    put_u32(rows);
    put_u32(static_cast<uint32_t>(layout.image_stride));
    put_u32(images);
    put_blob(pixels.data() + layout.offset,
             static_cast<uint32_t>(layout.end - layout.offset));
    break;
}}"""
    if name == "glTransformFeedbackVaryings":
        return f"""case {opcode}: {{
    GLuint program = lookup(programs, a.u32()); GLsizei count = a.i32();
    GLenum bufferMode = a.u32(); uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    if (count < 0 || (count && !wire)) {{ a.ok = false; break; }}
    std::vector<const GLchar *> varyings; varyings.reserve((size_t)count);
    const char *at = reinterpret_cast<const char *>(wire);
    size_t left = bytes;
    for (GLsizei i = 0; i < count; i++) {{
        const void *nul = memchr(at, 0, left);
        if (!nul) {{ a.ok = false; break; }}
        varyings.push_back(at);
        size_t used = static_cast<const char *>(nul) - at + 1;
        at += used; left -= used;
    }}
    if (!a.ok || left) {{ a.ok = false; break; }}
    glTransformFeedbackVaryings(program, count, varyings.data(), bufferMode);
    break;
}}"""
    if name == "glPatchParameterfv":
        return f"""case {opcode}: {{
    GLenum pname = a.u32(); uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    const uint32_t expected = pname == GL_PATCH_DEFAULT_INNER_LEVEL ? 2u * sizeof(GLfloat) :
                              pname == GL_PATCH_DEFAULT_OUTER_LEVEL ? 4u * sizeof(GLfloat) : 0u;
    if (!expected || bytes != expected) {{ a.ok = false; break; }}
    glPatchParameterfv(pname, reinterpret_cast<const GLfloat *>(wire));
    break;
}}"""
    if name in POINT_PARAMETER_ARRAYS:
        value_type = "GLfloat" if name.endswith("fv") else "GLint"
        return f"""case {opcode}: {{
    GLenum pname = a.u32(); uint32_t bytes = 0; const uint8_t *wire = a.blob(bytes);
    if (bytes != sizeof({value_type})) {{ a.ok = false; break; }}
    {name}(pname, reinterpret_cast<const {value_type} *>(wire));
    break;
}}"""
    if name == "glTexImage1D":
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLint level = a.i32(), internalformat = a.i32();
    GLsizei width = a.i32(); GLint border = a.i32(); GLenum format = a.u32(), type = a.u32();
    const bool pbo = a.u32() != 0; const uintptr_t pointer = static_cast<uintptr_t>(a.u64());
    uint32_t bytes = 0; const uint8_t *pixels = a.blob(bytes);
    if (pbo) {{ glTexImage1D(target, level, internalformat, width, border, format, type,
                             reinterpret_cast<const void *>(pointer)); break; }}
    gexgl_pixel_layout layout{{}};
    const bool valid = gexgl_compute_pixel_layout(format, type, width, 1, -1,
                                                   &unpack_store, &layout);
    const size_t expected = valid ? layout.end : 0;
    if (bytes && bytes != expected) {{ a.ok = false; break; }}
    glTexImage1D(target, level, internalformat, width, border, format, type,
                 bytes ? pixels : nullptr);
    break;
}}"""
    if name == "glTexSubImage1D":
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLint level = a.i32(), xoffset = a.i32();
    GLsizei width = a.i32(); GLenum format = a.u32(), type = a.u32();
    const bool pbo = a.u32() != 0; const uintptr_t pointer = static_cast<uintptr_t>(a.u64());
    uint32_t bytes = 0; const uint8_t *pixels = a.blob(bytes);
    if (pbo) {{ glTexSubImage1D(target, level, xoffset, width, format, type,
                                reinterpret_cast<const void *>(pointer)); break; }}
    gexgl_pixel_layout layout{{}};
    const bool valid = gexgl_compute_pixel_layout(format, type, width, 1, -1,
                                                   &unpack_store, &layout);
    const size_t expected = valid ? layout.end : 0;
    if (bytes && bytes != expected) {{ a.ok = false; break; }}
    glTexSubImage1D(target, level, xoffset, width, format, type, bytes ? pixels : nullptr);
    break;
}}"""
    if name == "glTexSubImage3D":
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLint level = a.i32(), xoffset = a.i32(), yoffset = a.i32(), zoffset = a.i32();
    GLsizei width = a.i32(), height = a.i32(), depth = a.i32(); GLenum format = a.u32(), type = a.u32();
    const bool pbo = a.u32() != 0; const uintptr_t pointer = static_cast<uintptr_t>(a.u64());
    uint32_t bytes = 0; const uint8_t *pixels = a.blob(bytes);
    if (pbo) {{ glTexSubImage3D(target, level, xoffset, yoffset, zoffset, width, height, depth,
                                format, type, reinterpret_cast<const void *>(pointer)); break; }}
    gexgl_pixel_layout layout{{}};
    const bool valid = gexgl_compute_pixel_layout(format, type, width, height, depth,
                                                   &unpack_store, &layout);
    const size_t expected = valid ? layout.end : 0;
    if (bytes && bytes != expected) {{ a.ok = false; break; }}
    glTexSubImage3D(target, level, xoffset, yoffset, zoffset, width, height, depth,
                    format, type, bytes ? pixels : nullptr);
    break;
}}"""
    if name == "glGetBufferSubData":
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLintptr offset = (GLintptr)a.u64(); GLsizeiptr size = (GLsizeiptr)a.u64();
    if (offset < 0 || size < 0 || (uint64_t)size > (1u << 28)) {{ a.ok = false; break; }}
    std::vector<uint8_t> value((size_t)size); glGetBufferSubData(target, offset, size, value.data());
    put_blob(value.data(), value.size());
    break;
}}"""
    if name in INDEXED_INTEGER_QUERIES:
        value_type = "GLint64" if name == "glGetInteger64i_v" else "GLint"
        put = "put_u64((uint64_t)value);" if name == "glGetInteger64i_v" else "put_u32((uint32_t)value);"
        image_translation = ("\n    if (target == GL_IMAGE_BINDING_NAME) "
                             "value = external_name(textures, (GLuint)value);"
                             if name == "glGetIntegeri_v" else "")
        sampler_translation = ("\n    if (target == GL_SAMPLER_BINDING) "
                               "value = external_name(samplers, (GLuint)value);"
                               if name == "glGetIntegeri_v" else "")
        return f"""case {opcode}: {{
    GLenum target = a.u32(); GLuint index = a.u32(); {value_type} value = 0;
    {name}(target, index, &value);
    if (target == GL_UNIFORM_BUFFER_BINDING || target == GL_TRANSFORM_FEEDBACK_BUFFER_BINDING ||
        target == GL_ATOMIC_COUNTER_BUFFER_BINDING || target == GL_SHADER_STORAGE_BUFFER_BINDING)
        value = external_name(buffers, (GLuint)value);{image_translation}{sampler_translation}
    {put}
    break;
}}"""
    if name == "glGetProgramInterfaceiv":
        return f"""case {opcode}: {{
    GLuint program = lookup(programs, a.u32()); GLenum interface = a.u32(), pname = a.u32();
    GLint value = 0; glGetProgramInterfaceiv(program, interface, pname, &value); put_u32((uint32_t)value);
    break;
}}"""
    if name in PROGRAM_NAMED_QUERIES:
        return f"""case {opcode}: {{
    GLuint program = lookup(programs, a.u32()); GLenum interface = a.u32(); std::string resourceName = a.string();
    put_u32((uint32_t){name}(program, interface, resourceName.c_str()));
    break;
}}"""
    if name == "glGetProgramResourceName":
        return f"""case {opcode}: {{
    GLuint program = lookup(programs, a.u32()); GLenum interface = a.u32(); GLuint index = a.u32();
    GLint maximum = 0; glGetProgramInterfaceiv(program, interface, GL_MAX_NAME_LENGTH, &maximum);
    std::vector<GLchar> value(std::max(maximum, 1)); GLsizei length = 0;
    glGetProgramResourceName(program, interface, index, value.size(), &length, value.data());
    put_blob(value.data(), std::max(length, 0));
    break;
}}"""
    if name == "glGetProgramResourceiv":
        return f"""case {opcode}: {{
    GLuint program = lookup(programs, a.u32()); GLenum interface = a.u32(); GLuint index = a.u32();
    GLsizei propCount = a.i32(), bufSize = a.i32(); uint32_t propBytes = 0;
    const uint8_t *propWire = a.blob(propBytes);
    if (propCount < 0 || bufSize < 0 || propBytes != (uint32_t)propCount * sizeof(GLenum)) {{ a.ok = false; break; }}
    const GLenum *props = reinterpret_cast<const GLenum *>(propWire);
    std::vector<GLint> values(bufSize); GLsizei length = 0;
    glGetProgramResourceiv(program, interface, index, propCount, props, bufSize, &length, values.data());
    length = std::clamp<GLsizei>(length, 0, bufSize); put_u32((uint32_t)length);
    put_blob(values.data(), (uint32_t)length * sizeof(GLint));
    break;
}}"""
    if name in RESOURCE_OUTPUT_QUERIES:
        output = command["params"][-1]
        scalar_params = command["params"][:-1]
        lines = [f"case {opcode}: {{"]
        call_names = []
        for param in scalar_params:
            param_name, ty = param["name"], param["type"]
            lines.append(f"    {ty} {param_name} = a.{TYPE_INFO[ty][1]}();")
            namespace = OBJECT_COMMANDS.get(name, {}).get(param_name)
            call_names.append(f"lookup({namespace}, {param_name})" if namespace else param_name)
        components = output_component_expression(RESOURCE_OUTPUT_QUERIES[name])
        output_type = output["type"]
        lines.append(f"    const uint32_t components = {components};")
        if RESOURCE_OUTPUT_QUERIES[name] == "state":
            lines += [f"    std::vector<{output_type}> values(components);",
                      f"    if (components) GEX_GL_CALL({name}, {', '.join(call_names + ['values.data()'])});"]
        else:
            lines += [f"    {output_type} values[4]{{}};",
                      f"    GEX_GL_CALL({name}, {', '.join(call_names + ['values'])});"]
        if name == "glGetFramebufferAttachmentParameteriv":
            lines += ["    if (pname == GL_FRAMEBUFFER_ATTACHMENT_OBJECT_NAME) {",
                      "        GLint objectType = GL_NONE;",
                      "        glGetFramebufferAttachmentParameteriv(target, attachment,",
                      "                                                GL_FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE,",
                      "                                                &objectType);",
                      "        if (objectType == GL_TEXTURE)",
                      "            values[0] = (GLint)external_name(textures, (GLuint)values[0]);",
                      "        else if (objectType == GL_RENDERBUFFER)",
                      "            values[0] = (GLint)external_name(renderbuffers, (GLuint)values[0]);",
                      "    }"]
        if name in {"glGetVertexAttribiv", "glGetVertexAttribIiv", "glGetVertexAttribIuiv"}:
            lines += ["    if (pname == GL_VERTEX_ATTRIB_ARRAY_BUFFER_BINDING)",
                      f"        values[0] = ({output_type})external_name(buffers, (GLuint)values[0]);"]
        value_pointer = "values.data()" if RESOURCE_OUTPUT_QUERIES[name] == "state" else "values"
        lines += [f"    put_blob({value_pointer}, components * sizeof({output_type}));",
                  "    break;", "}"]
        return "\n".join(lines)
    lines = [f"case {opcode}: {{"]
    if command["name"] in BLOB_COMMANDS:
        scalar_names = []
        for param in command["params"][:-1]:
            param_name, ty = param["name"], param["type"]
            lines.append(f"    {ty} {param_name} = a.{TYPE_INFO[ty][1]}();")
            scalar_names.append(param_name)
        pointer = command["params"][-1]
        pointer_name = pointer["name"]
        length = pointer["length"]
        lines += ["    const bool pbo = a.u32() != 0;",
                  "    const uintptr_t pointer = static_cast<uintptr_t>(a.u64());",
                  f"    uint32_t {pointer_name}_bytes = 0;",
                  f"    const uint8_t *{pointer_name}_wire = a.blob({pointer_name}_bytes);",
                  f"    if (!pbo && {pointer_name}_bytes &&",
                  f"        ({length} < 0 || {pointer_name}_bytes != static_cast<uint32_t>({length}))) {{",
                  "        a.ok = false; break;",
                  "    }",
                  f"    const void *{pointer_name} = pbo ? reinterpret_cast<const void *>(pointer) :",
                  f"                                      ({pointer_name}_bytes ? {pointer_name}_wire : nullptr);",
                  f"    {command['name']}({', '.join(scalar_names + [pointer_name])});",
                  "    break;", "}"]
        return "\n".join(lines)
    if command["name"] in OPAQUE_OFFSET_COMMANDS:
        for param in command["params"]:
            if param["pointer"]:
                continue
            reader = TYPE_INFO[param["type"]][1]
            lines.append(f"    {param['type']} {param['name']} = a.{reader}();")
        pointer = next(param for param in command["params"] if param["pointer"])
        lines.append(f"    const void *{pointer['name']} = reinterpret_cast<const void *>(static_cast<uintptr_t>(a.u64()));")
        names = [param["name"] for param in command["params"]]
        lines += [f"    GEX_GL_CALL({command['name']}, {', '.join(names)});", "    break;", "}"]
        return "\n".join(lines)
    names = []
    for param in command["params"]:
        name, ty = param["name"], param["type"]
        local_name = f"arg_{name}" if name == "a" else name
        if param["pointer"]:
            lines += [f"    uint32_t {name}_bytes = 0;",
                      f"    const uint8_t *{name}_wire = a.blob({name}_bytes);"]
            if command["name"] in ARRAY_COMMANDS:
                count_name, components = command_array_length(command["name"], param["length"])
                if count_name:
                    lines += [f"    size_t {name}_expected = 0;",
                              f"    if ({count_name} > 0 &&",
                              f"        (size_t){count_name} <= SIZE_MAX / ({components}u * sizeof({ty})))",
                              f"        {name}_expected = (size_t){count_name} * {components}u * sizeof({ty});"]
                else:
                    lines.append(f"    const size_t {name}_expected = {components}u * sizeof({ty});")
                lines.append(f"    if ({name}_bytes != {name}_expected) {{ a.ok = false; break; }}")
                lines += [f"    std::vector<{ty}> {name}_aligned({name}_bytes / sizeof({ty}));",
                          f"    if ({name}_bytes) memcpy({name}_aligned.data(), {name}_wire, {name}_bytes);",
                          f"    const {ty} *{name} = {name}_bytes ? {name}_aligned.data() : nullptr;"]
            else:
                lines.append(
                    (f"    const void *{name} = {name}_bytes ? {name}_wire : nullptr;"
                     if command["name"] in BLOB_COMMANDS else
                     f"    const {ty} *{name} = {name}_bytes "
                     f"? reinterpret_cast<const {ty} *>({name}_wire) : nullptr;")
                )
            names.append(name)
            continue
        reader = TYPE_INFO[ty][1]
        lines.append(f"    {ty} {local_name} = a.{reader}();")
        namespace = OBJECT_COMMANDS.get(command["name"], {}).get(name)
        names.append(f"lookup({namespace}, {local_name})" if namespace else local_name)
    call = f"GEX_GL_CALL({command['name']}, {', '.join(names)})"
    if command["name"] in RETURN_COMMANDS:
        lines.append(f"    put_u32((uint32_t){call});")
    else:
        lines.append(f"    {call};")
    lines += ["    break;", "}"]
    return "\n".join(lines)


def main() -> int:
    root = pathlib.Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="fail if checked-in output differs")
    args = parser.parse_args()
    selected = validate(load_manifest(root)["commands"])
    # The ordinal is over the complete alphabetic 4.4 command set. Adding a
    # policy for another existing command therefore cannot renumber this ABI.
    all_names = sorted(c["name"] for c in load_manifest(root)["commands"])
    policies = (SCALAR_COMMANDS | RETURN_COMMANDS | PROGRAM_QUERY_COMMANDS |
                RESOURCE_OUTPUT_QUERIES.keys() | BLOB_COMMANDS |
                ARRAY_COMMANDS | OPAQUE_OFFSET_COMMANDS | MULTI_ARRAY_COMMANDS |
                MULTI_ELEMENT_COMMANDS |
                TEXTURE_PARAMETER_ARRAYS | SAMPLER_MULTI_BIND_COMMANDS |
                MULTI_BIND_NAME_ARRAYS.keys() |
                INTERNAL_FORMAT_QUERIES | MULTISAMPLE_QUERIES |
                COMPRESSED_TEXTURE_READBACK_QUERIES | OBJECT_COMMANDS.keys())
    policies |= PROGRAM_PIPELINE_GENERATORS | PROGRAM_PIPELINE_DELETERS
    policies |= BUFFER_STORAGE_COMMANDS
    policies |= STRING_INPUT_COMMANDS | STRING_RETURN_COMMANDS.keys()
    policies |= UNIFORM_VALUE_QUERIES.keys()
    policies |= CLEAR_VALUE_COMMANDS
    policies |= PARALLEL_BIND_COMMANDS.keys()
    policies |= (PATCH_PARAMETER_ARRAYS | POINT_PARAMETER_ARRAYS | PIXEL_IMAGE_COMMANDS |
                 STRING_ARRAY_COMMANDS | TEXTURE_READBACK_QUERIES)
    opcodes = {name: 1024 + all_names.index(name) for name in policies}
    guest = "/* Generated by tools/gen_gl44_transport.py; do not edit. */\n\n"
    host = "/* Generated by tools/gen_gl44_transport.py; do not edit. */\n"
    for command in selected:
        guest += guest_function(command, opcodes[command["name"]])
        host += host_case(command, opcodes[command["name"]]) + "\n"
    outputs = {
        root / "guest/gl/gl44-generated.inc": guest,
        root / "web/mobilegl/gl44-generated-cases.inc": host,
    }
    if args.check:
        stale = [str(path.relative_to(root)) for path, value in outputs.items()
                 if not path.is_file() or path.read_text() != value]
        if stale:
            parser.error("stale generated files: " + ", ".join(stale))
    else:
        for path, value in outputs.items():
            path.write_text(value)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
