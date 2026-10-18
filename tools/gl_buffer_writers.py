"""Commands that may change a buffer object's contents other than through
glBufferData/glBufferSubData: GPU writes (copies, clears, transform feedback,
compute, pixel and query readbacks into a bound buffer), the barriers that
publish shader writes, and the direct-state-access uploads and maps the guest
does not track. The guest calls buffer_contents_unknown() (guest/gl/gexgl.c)
before each, which drops its copies of mapped buffers' contents. Used by
tools/gen_gl44_transport.py and tools/gen_gl_compat.py."""

import re

BUFFER_WRITERS = re.compile(
    r"^gl(BufferStorage|NamedBufferStorage|CopyBufferSubData|CopyNamedBufferSubData"
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
    if PACK_READS.match(name):
        call = "if (bound_buffer(0x88EBu)) " + call  # GL_PIXEL_PACK_BUFFER
    elif name.startswith("glGetQueryObject"):
        call = "if (bound_buffer(0x9192u)) " + call  # GL_QUERY_BUFFER
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
