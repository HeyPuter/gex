#ifndef GEXGL_PIXEL_H
#define GEXGL_PIXEL_H

#include <stddef.h>
#include <stdint.h>

/* Pixel-store state needed to determine the client-memory span touched by an
 * uncompressed pixel transfer. Values and defaults are from OpenGL 4.4
 * section 8.4.  Keeping this calculation identical at both ends of the wire
 * is a memory-safety requirement, not merely an optimization. */
struct gexgl_pixel_store {
    size_t alignment;
    size_t row_length;
    size_t image_height;
    size_t skip_pixels;
    size_t skip_rows;
    size_t skip_images;
};

struct gexgl_pixel_layout {
    size_t offset;
    size_t row_stride;
    size_t image_stride;
    size_t end;
};

static inline struct gexgl_pixel_store gexgl_default_pixel_store(void) {
    struct gexgl_pixel_store value = {4, 0, 0, 0, 0, 0};
    return value;
}

static inline int gexgl_size_add(size_t a, size_t b, size_t *out) {
    if (b > SIZE_MAX - a) return 0;
    *out = a + b;
    return 1;
}

static inline int gexgl_size_mul(size_t a, size_t b, size_t *out) {
    if (a && b > SIZE_MAX / a) return 0;
    *out = a * b;
    return 1;
}

static inline int gexgl_size_align(size_t value, size_t alignment, size_t *out) {
    size_t added;
    if (!alignment || (alignment & (alignment - 1)) ||
        !gexgl_size_add(value, alignment - 1, &added)) return 0;
    *out = added & ~(alignment - 1);
    return 1;
}

static inline size_t gexgl_format_components(uint32_t format) {
    switch (format) {
    case 0x1900: /* COLOR_INDEX */
    case 0x1901: /* STENCIL_INDEX */
    case 0x1902: /* DEPTH_COMPONENT */
    case 0x1903: /* RED */
    case 0x1904: /* GREEN */
    case 0x1905: /* BLUE */
    case 0x1906: /* ALPHA */
    case 0x1909: /* LUMINANCE */
    case 0x8D94: /* RED_INTEGER */
    case 0x8D95: /* GREEN_INTEGER */
    case 0x8D96: /* BLUE_INTEGER */
        return 1;
    case 0x190A: /* LUMINANCE_ALPHA */
    case 0x8227: /* RG */
    case 0x8228: /* RG_INTEGER */
        return 2;
    case 0x1907: /* RGB */
    case 0x80E0: /* BGR */
    case 0x8D98: /* RGB_INTEGER */
    case 0x8D9A: /* BGR_INTEGER */
        return 3;
    case 0x1908: /* RGBA */
    case 0x80E1: /* BGRA */
    case 0x8D99: /* RGBA_INTEGER */
    case 0x8D9B: /* BGRA_INTEGER */
        return 4;
    case 0x84F9: /* DEPTH_STENCIL: its legal types are packed. */
        return 1;
    default:
        return 0;
    }
}

static inline size_t gexgl_pixel_group_bytes(uint32_t format, uint32_t type) {
    size_t scalar = 0;
    switch (type) {
    case 0x1400: /* BYTE */
    case 0x1401: /* UNSIGNED_BYTE */
        scalar = 1; break;
    case 0x1402: /* SHORT */
    case 0x1403: /* UNSIGNED_SHORT */
    case 0x140B: /* HALF_FLOAT */
        scalar = 2; break;
    case 0x1404: /* INT */
    case 0x1405: /* UNSIGNED_INT */
    case 0x1406: /* FLOAT */
    case 0x140C: /* FIXED */
        scalar = 4; break;
    case 0x140A: /* DOUBLE */
        scalar = 8; break;
    case 0x8032: /* UNSIGNED_BYTE_3_3_2 */
    case 0x8362: /* UNSIGNED_BYTE_2_3_3_REV */
        return 1;
    case 0x8033: /* UNSIGNED_SHORT_4_4_4_4 */
    case 0x8034: /* UNSIGNED_SHORT_5_5_5_1 */
    case 0x8363: /* UNSIGNED_SHORT_5_6_5 */
    case 0x8364: /* UNSIGNED_SHORT_5_6_5_REV */
    case 0x8365: /* UNSIGNED_SHORT_4_4_4_4_REV */
    case 0x8366: /* UNSIGNED_SHORT_1_5_5_5_REV */
        return 2;
    case 0x8035: /* UNSIGNED_INT_8_8_8_8 */
    case 0x8036: /* UNSIGNED_INT_10_10_10_2 */
    case 0x8367: /* UNSIGNED_INT_8_8_8_8_REV */
    case 0x8368: /* UNSIGNED_INT_2_10_10_10_REV */
    case 0x84FA: /* UNSIGNED_INT_24_8 */
    case 0x8C3B: /* UNSIGNED_INT_10F_11F_11F_REV */
    case 0x8C3E: /* UNSIGNED_INT_5_9_9_9_REV */
        return 4;
    case 0x8DAD: /* FLOAT_32_UNSIGNED_INT_24_8_REV */
        return 8;
    default:
        return 0;
    }
    size_t components = gexgl_format_components(format), bytes;
    return components && gexgl_size_mul(components, scalar, &bytes) ? bytes : 0;
}

/* depth == -1 denotes a 2D transfer, for which IMAGE_HEIGHT and SKIP_IMAGES do
 * not participate. Non-negative depth denotes a 3D transfer. GL_BITMAP is kept
 * out of this byte-group helper; callers reject it safely until its bit-level
 * packing is transported explicitly. */
static inline int gexgl_compute_pixel_layout(
    uint32_t format, uint32_t type, int width, int height, int depth,
    const struct gexgl_pixel_store *store, struct gexgl_pixel_layout *out) {
    size_t group, row_pixels, image_rows, row_bytes, row_stride;
    size_t image_stride, offset_rows, offset_images, offset_pixels, offset;
    size_t last_image, last_row, active_row, end;
    const int is_3d = depth >= 0;
    const size_t layers = is_3d ? (size_t)depth : 1;

    if (!out || !store || width < 0 || height < 0 || depth < -1) return 0;
    *out = (struct gexgl_pixel_layout){0, 0, 0, 0};
    if (width == 0 || height == 0 || (is_3d && depth == 0)) return 1;
    if (type == 0x1A00) return 0; /* GL_BITMAP */
    group = gexgl_pixel_group_bytes(format, type);
    if (!group) return 0;
    row_pixels = store->row_length ? store->row_length : (size_t)width;
    image_rows = is_3d && store->image_height
        ? store->image_height : (size_t)height;
    if (!gexgl_size_mul(row_pixels, group, &row_bytes) ||
        !gexgl_size_align(row_bytes, store->alignment, &row_stride) ||
        !gexgl_size_mul(row_stride, image_rows, &image_stride) ||
        !gexgl_size_mul(store->skip_rows, row_stride, &offset_rows) ||
        !gexgl_size_mul(is_3d ? store->skip_images : 0,
                        image_stride, &offset_images) ||
        !gexgl_size_mul(store->skip_pixels, group, &offset_pixels) ||
        !gexgl_size_add(offset_images, offset_rows, &offset) ||
        !gexgl_size_add(offset, offset_pixels, &offset) ||
        !gexgl_size_mul(layers - 1, image_stride, &last_image) ||
        !gexgl_size_mul((size_t)height - 1, row_stride, &last_row) ||
        !gexgl_size_mul((size_t)width, group, &active_row) ||
        !gexgl_size_add(offset, last_image, &end) ||
        !gexgl_size_add(end, last_row, &end) ||
        !gexgl_size_add(end, active_row, &end)) return 0;
    out->offset = offset;
    out->row_stride = row_stride;
    out->image_stride = image_stride;
    out->end = end;
    return 1;
}

static inline int gexgl_set_pixel_store(struct gexgl_pixel_store *pack,
                                        struct gexgl_pixel_store *unpack,
                                        uint32_t pname, int param) {
    struct gexgl_pixel_store *state = NULL;
    size_t *field = NULL;
    if (!pack || !unpack || param < 0) return 0;
    switch (pname) {
    case 0x0D05: state = pack; field = &state->alignment; break;
    case 0x0D02: state = pack; field = &state->row_length; break;
    case 0x806C: state = pack; field = &state->image_height; break;
    case 0x0D04: state = pack; field = &state->skip_pixels; break;
    case 0x0D03: state = pack; field = &state->skip_rows; break;
    case 0x806B: state = pack; field = &state->skip_images; break;
    case 0x0CF5: state = unpack; field = &state->alignment; break;
    case 0x0CF2: state = unpack; field = &state->row_length; break;
    case 0x806E: state = unpack; field = &state->image_height; break;
    case 0x0CF4: state = unpack; field = &state->skip_pixels; break;
    case 0x0CF3: state = unpack; field = &state->skip_rows; break;
    case 0x806D: state = unpack; field = &state->skip_images; break;
    default: return 0;
    }
    if ((pname == 0x0D05 || pname == 0x0CF5) &&
        param != 1 && param != 2 && param != 4 && param != 8) return 0;
    *field = (size_t)param;
    return 1;
}

#endif
