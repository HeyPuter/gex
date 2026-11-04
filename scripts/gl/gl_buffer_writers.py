"""Commands that may change a buffer object's contents other than through
glBufferData/glBufferSubData: GPU writes (copies, clears, transform feedback,
compute, pixel and query readbacks into a bound buffer), the barriers that
publish shader writes, and the direct-state-access uploads and maps the guest
does not track. The guest calls buffer_contents_unknown() (guest/gl/gexgl.c)
before each, which drops its copies of mapped buffers' contents.
(glBufferStorage replaces only the bound buffer's: buffer_new_storage.) Used by
scripts/gl/gen_gl44_transport.py and scripts/gl/gen_gl_compat.py."""

import re

BUFFER_WRITERS = re.compile(
    r"^gl(NamedBufferStorage|CopyBufferSubData|CopyNamedBufferSubData|NamedCopyBufferSubDataEXT"
    r"|ClearBuffer(Sub)?Data|ClearNamedBuffer(Sub)?Data|(Begin|End)TransformFeedback"
    r"|DispatchCompute|MemoryBarrier|Readn?Pixels|Getn?TexImage|GetTexture(Sub)?Image"
    r"|Getn?CompressedTexImage|GetCompressedTexture(Sub)?Image|GetQueryObject"
    r"|GetQueryBufferObject|MapNamedBuffer|UnmapNamedBuffer|FlushMappedNamedBuffer"
    r"|NamedBuffer(Sub)?Data|NamedBufferPageCommitment|BufferPageCommitment"
    r"|NamedBufferAttachMemory|BufferAttachMemory)")

PACK_READS = re.compile(
    r"^gl(Readn?Pixels|Getn?TexImage|GetTexture(Sub)?Image|Getn?CompressedTexImage"
    r"|GetCompressedTexture(Sub)?Image)")


def mark_buffer_writer(name: str, function: str) -> str:
    """FUNCTION (C source of NAME) with the call added as its first statement."""
    if not BUFFER_WRITERS.match(name):
        return function
    # Readbacks write a buffer only into a bound pack or query buffer, and
    # they are frequent (occlusion queries every frame).
    call = "buffer_contents_unknown();"
    if name == "glCopyBufferSubData":
        call = "buffer_copy(readTarget, writeTarget, readOffset, writeOffset, size);"
    if PACK_READS.match(name):
        call = "if (bound_buffer(0x88EBu)) " + call  # GL_PIXEL_PACK_BUFFER
    elif name.startswith("glGetQueryObject"):
        call = "if (bound_buffer(0x9192u)) " + call  # GL_QUERY_BUFFER
    # A barrier also publishes the application's writes to persistent
    # mappings (persistent_barrier, guest/gl/gexgl.c).
    if name == "glMemoryBarrier":
        call += " persistent_barrier(barriers);"
    at = function.index("{\n") + 2
    return function[:at] + "    " + call + "\n" + function[at:]


# Uploads gexgl.c splits when larger than one command can carry: each such
# command first calls split_<name>(its arguments), which returns 1 when it
# sent the upload itself (guest/gl/gexgl.c).
SPLIT_UPLOADS = {"glTexSubImage3D", "glCompressedTexImage2D", "glCompressedTexImage3D",
                 "glCompressedTexSubImage2D", "glCompressedTexSubImage3D"}


def mark_split_upload(name: str, function: str) -> str:
    """FUNCTION with the split_<NAME> call added as its first statement."""
    if name not in SPLIT_UPLOADS:
        return function
    head = function[:function.index("{\n")]
    params = head[head.index("(") + 1:head.rindex(")")]
    args = ", ".join(re.findall(r"(\w+)\s*(?:,|$)", params.replace("\n", " ")))
    at = function.index("{\n") + 2
    return function[:at] + f"    if (split_{name}({args})) return;\n" + function[at:]


# Commands that read buffer memory a coherent persistent mapping may have
# changed: each first sends what it can read (persistent_draw_elements and
# the rest, guest/gl/gexgl.c), or every mapping when that is unknown.
PERSISTENT_READS = {
    "glDrawArraysInstancedBaseInstance": "persistent_draw_arrays(first, count, instancecount, baseinstance);",
    "glDrawElementsBaseVertex": "persistent_draw_elements(count, type, indices, basevertex, 1, 0, 0, 0, 0);",
    "glDrawElementsInstancedBaseInstance":
        "persistent_draw_elements(count, type, indices, 0, instancecount, baseinstance, 0, 0, 0);",
    "glDrawElementsInstancedBaseVertex":
        "persistent_draw_elements(count, type, indices, basevertex, instancecount, 0, 0, 0, 0);",
    "glDrawElementsInstancedBaseVertexBaseInstance":
        "persistent_draw_elements(count, type, indices, basevertex, instancecount, baseinstance, 0, 0, 0);",
    "glDrawRangeElements": "persistent_draw_elements(count, type, indices, 0, 1, 0, 1, start, end);",
    "glDrawRangeElementsBaseVertex":
        "persistent_draw_elements(count, type, indices, basevertex, 1, 0, 1, start, end);",
    "glCopyBufferSubData": "persistent_read_target(readTarget, readOffset, size);",
    "glGetBufferSubData": "persistent_read_target(target, offset, size);",
    "glBindBufferRange": "persistent_indexed(target, index, buffer, offset, size);",
    "glTexBuffer": "persistent_texbuffer(buffer, 0, 0);",
    "glTexBufferRange": "persistent_texbuffer(buffer, offset, size);",
    # The bindings, and the three group counts in GL_DISPATCH_INDIRECT_BUFFER.
    "glDispatchComputeIndirect": "persistent_dispatch(); persistent_read_target(0x90EEu, indirect, 12);",
}

PERSISTENT_UNKNOWN_READS = re.compile(
    r"^gl(MultiDraw\w*|Draw\w*Indirect\w*|DrawTransformFeedback\w*|Draw(Range)?ElementArray\w*|DrawMeshArraysSUN"
    r"|CopyNamedBufferSubData|NamedCopyBufferSubDataEXT|GetNamedBufferSubData\w*)$")
PERSISTENT_DISPATCH = re.compile(r"^gl(DispatchCompute\w*|DrawMeshTasks(NV|EXT))$")
PERSISTENT_VAO_STATE = re.compile(
    r"^gl(BindVertexBuffers?|VertexAttribBinding|VertexAttrib[IL]?Format(NV)?|VertexBindingDivisor"
    r"|VertexArray\w+|VertexAttribLPointer)$")
PERSISTENT_BINDINGS = re.compile(r"^gl(BindBuffers(Base|Range)|TextureBuffer\w*)$")
PERSISTENT_UNPACK = re.compile(r"^gl((Compressed)?Tex(ture)?(Sub)?Image[123]D(EXT)?|DrawPixels)$")


def mark_persistent_reader(name: str, function: str) -> str:
    """FUNCTION with its persistent-mapping call added as its first statement."""
    call = PERSISTENT_READS.get(name)
    if call is None:
        if PERSISTENT_UNKNOWN_READS.match(name):
            call = "persistent_read_unknown();"
        elif PERSISTENT_DISPATCH.match(name):
            call = "persistent_dispatch();"
        elif PERSISTENT_VAO_STATE.match(name):
            call = "persistent_vao_unknown();"
        elif PERSISTENT_BINDINGS.match(name):
            call = "persistent_indexed_unknown();"
        elif PERSISTENT_UNPACK.match(name):
            call = "if (bound_buffer(0x88ECu)) persistent_unpack(NULL, 0);"  # GL_PIXEL_UNPACK_BUFFER
        else:
            return function
    at = function.index("{\n") + 2
    return function[:at] + "    " + call + "\n" + function[at:]
