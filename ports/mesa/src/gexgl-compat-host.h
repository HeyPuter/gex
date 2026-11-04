/* The host side of the generated compatibility-profile commands
 * (scripts/gl/gen_gl_compat.py, gl-compat-generated-cases.inc): reading a plan's
 * input arrays off the wire and checking them against the size the plan
 * gives, which is computed here exactly as the guest computed it
 * (guest/gl/gexgl-plan.h). Shared by the browser's Mesa decoder
 * (gexgl-mesa.cpp) and the native host's (crates/gex-gl/native).
 *
 * The includer provides, before this header: a `Reader` with u32/u64/blob
 * and `ok`; `unpack_store`, the unpack pixel store its pixel commands
 * track; and, before the cases, put_u32/put_u64/put_blob, GEXGEN_NAME(kind,
 * name), `void* gexgen_proc(const char* name, const char* alias)` and
 * `void gexgen_missing(const char* name)`. */
#ifndef GEXGL_COMPAT_HOST_H
#define GEXGL_COMPAT_HOST_H

#include <cstdint>
#include <cstring>
#include <limits>
#include <vector>

#include "../../../guest/gl/gexgl-plan.h"

/* 64-bit words enough for `count` elements of `size` bytes. */
static inline size_t gexgen_words(size_t count, size_t size) {
    if (count > GEXGEN_MAX_BYTES / (size ? size : 1)) count = GEXGEN_MAX_BYTES / (size ? size : 1);
    return (count * size + 7) / 8 + 1;
}

/* An input array as it came off the wire: read in parameter order, checked
 * once every parameter (its count among them) has been read. */
struct gexgen_raw {
    uint32_t tag = 0;     /* 0 NULL, 1 bytes, 2 an unpack buffer offset */
    const uint8_t* p = nullptr;
    uint32_t n = 0;
    uint64_t offset = 0;
};

template <typename R>
static bool gexgen_take(R& a, gexgen_raw& r, bool unpack) {
    r.tag = a.u32();
    if (!a.ok) return false;
    if (r.tag == 1) r.p = a.blob(r.n);
    else if (r.tag == 2 && unpack) r.offset = a.u64();
    else if (r.tag != 0) a.ok = false;
    return a.ok;
}

/* Bytes, which must be exactly `bytes` long: copied to 8-byte-aligned
 * storage, zero-padded to at least 64 bytes (a pname-sized vector GL may read
 * more of than the shared table says). An unpack offset is passed as the
 * pointer GL takes for one. */
static inline bool gexgen_check(const gexgen_raw& r, std::vector<uint64_t>& store, const void** out, size_t bytes) {
    *out = nullptr;
    if (r.tag == 0) return true;
    if (r.tag == 2) { *out = reinterpret_cast<const void*>(static_cast<uintptr_t>(r.offset)); return true; }
    if (r.n != bytes) return false;
    store.assign((r.n + 7) / 8 < 8 ? 8 : (r.n + 7) / 8, 0);
    if (r.n) memcpy(store.data(), r.p, r.n);
    *out = store.data();
    return true;
}

/* An unpack pixel transfer, sized by this end's unpack store. */
static inline bool gexgen_check_pixels(const gexgen_raw& r, std::vector<uint64_t>& store, const void** out,
                                       uint32_t format, uint32_t type, int w, int h, int d) {
    struct gexgl_pixel_layout layout;
    const size_t bytes = gexgl_compute_pixel_layout(format, type, w, h, d, &unpack_store, &layout) ? layout.end : 0;
    return gexgen_check(r, store, out, bytes);
}

static inline size_t gexgen_bitmap_bytes(long long w, long long h) {
    return gexgen_bitmap_bytes_store(w, h, &unpack_store);
}

/* A string: any length, NUL-terminated here. */
static inline bool gexgen_check_string(const gexgen_raw& r, std::vector<uint64_t>& store, const char** out) {
    *out = nullptr;
    if (r.tag == 0) return true;
    if (r.tag != 1) return false;
    store.assign(r.n / 8 + 1, 0);
    if (r.n) memcpy(store.data(), r.p, r.n);
    *out = reinterpret_cast<const char*>(store.data());
    return true;
}

#endif
