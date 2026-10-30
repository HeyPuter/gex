/* DirectWrite renders glyphs: a glyph run of the system font has a non-empty
 * alpha texture with ink in it, in the aliased and the ClearType texture
 * types, and its outline has segments. Both come from dwrite's Unix side
 * (FreeType); without it every run's bounds are empty and Direct2D, which
 * draws text through these calls, draws none.
 * gex-nt-arch: x64
 * gex-nt-libs: -ldwrite
 */
#define COBJMACROS
#include <windows.h>
#include <initguid.h>
#include <d2d1.h>
#include <dwrite.h>
#include <stdio.h>
#include <stdlib.h>

static int failures;
static void ok(int cond, const char *what, HRESULT hr)
{
    if (cond) printf("ok %s\n", what);
    else { printf("FAIL %s (hr %08lx)\n", what, (unsigned long)hr); failures++; }
}

/* a geometry sink that counts what it is given */
typedef struct { ID2D1SimplifiedGeometrySink iface; int figures, segments; } sink_t;
static HRESULT WINAPI s_qi(IUnknown *s, REFIID r, void **o) { *o = s; return S_OK; }
static ULONG WINAPI s_ref(IUnknown *s) { return 1; }
static void WINAPI s_fill(ID2D1SimplifiedGeometrySink *s, D2D1_FILL_MODE m) {}
static void WINAPI s_flags(ID2D1SimplifiedGeometrySink *s, D2D1_PATH_SEGMENT f) {}
static void WINAPI s_begin(ID2D1SimplifiedGeometrySink *s, D2D1_POINT_2F p, D2D1_FIGURE_BEGIN b) { ((sink_t *)s)->figures++; }
static void WINAPI s_lines(ID2D1SimplifiedGeometrySink *s, const D2D1_POINT_2F *p, UINT32 n) { ((sink_t *)s)->segments += n; }
static void WINAPI s_beziers(ID2D1SimplifiedGeometrySink *s, const D2D1_BEZIER_SEGMENT *b, UINT32 n) { ((sink_t *)s)->segments += n; }
static void WINAPI s_end(ID2D1SimplifiedGeometrySink *s, D2D1_FIGURE_END e) {}
static HRESULT WINAPI s_close(ID2D1SimplifiedGeometrySink *s) { return S_OK; }
static ID2D1SimplifiedGeometrySinkVtbl sink_vtbl = { { s_qi, s_ref, s_ref }, s_fill, s_flags, s_begin, s_lines, s_beziers, s_end, s_close };

int main(void)
{
    IDWriteFactory *f; IDWriteFontCollection *fc; IDWriteFontFamily *fam; IDWriteFont *font; IDWriteFontFace *face;
    UINT32 index; BOOL exists; HRESULT hr;
    hr = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED, &IID_IDWriteFactory, (IUnknown **)&f);
    ok(hr == S_OK, "factory", hr); if (hr != S_OK) return 1;
    hr = IDWriteFactory_GetSystemFontCollection(f, &fc, FALSE);
    ok(hr == S_OK, "system font collection", hr); if (hr != S_OK) return 1;
    hr = IDWriteFontCollection_FindFamilyName(fc, L"Tahoma", &index, &exists);
    if (hr != S_OK || !exists) index = 0;
    hr = IDWriteFontCollection_GetFontFamily(fc, index, &fam);
    ok(hr == S_OK, "a font family", hr); if (hr != S_OK) return 1;
    hr = IDWriteFontFamily_GetFirstMatchingFont(fam, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, &font);
    ok(hr == S_OK, "a font", hr); if (hr != S_OK) return 1;
    hr = IDWriteFont_CreateFontFace(font, &face);
    ok(hr == S_OK, "its face", hr); if (hr != S_OK) return 1;

    UINT32 cps[2] = { 'A', 'g' }; UINT16 glyphs[2] = { 0, 0 };
    hr = IDWriteFontFace_GetGlyphIndices(face, cps, 2, glyphs);
    ok(hr == S_OK && glyphs[0] && glyphs[1], "glyph indices", hr);
    FLOAT adv[2] = { 20, 20 }; DWRITE_GLYPH_OFFSET offs[2] = { { 0, 0 }, { 0, 0 } };
    DWRITE_GLYPH_RUN run = { face, 32, 2, glyphs, adv, offs, FALSE, 0 };

    static const struct { DWRITE_RENDERING_MODE mode; DWRITE_TEXTURE_TYPE type; const char *name; } kinds[] = {
        { DWRITE_RENDERING_MODE_ALIASED, DWRITE_TEXTURE_ALIASED_1x1, "aliased" },
        { DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC, DWRITE_TEXTURE_CLEARTYPE_3x1, "cleartype" },
    };
    for (int k = 0; k < 2; k++) {
        IDWriteGlyphRunAnalysis *a; RECT b = { 0 }; char what[64];
        hr = IDWriteFactory_CreateGlyphRunAnalysis(f, &run, 1.0f, NULL, kinds[k].mode, DWRITE_MEASURING_MODE_NATURAL, 0, 0, &a);
        snprintf(what, sizeof what, "%s analysis", kinds[k].name); ok(hr == S_OK, what, hr); if (hr != S_OK) continue;
        hr = IDWriteGlyphRunAnalysis_GetAlphaTextureBounds(a, kinds[k].type, &b);
        snprintf(what, sizeof what, "%s bounds %ldx%ld", kinds[k].name, b.right - b.left, b.bottom - b.top);
        ok(hr == S_OK && b.right > b.left && b.bottom > b.top, what, hr);
        if (b.right > b.left && b.bottom > b.top) {
            UINT32 size = (b.right - b.left) * (b.bottom - b.top) * (kinds[k].type == DWRITE_TEXTURE_CLEARTYPE_3x1 ? 3 : 1), ink = 0;
            BYTE *px = calloc(size, 1);
            hr = IDWriteGlyphRunAnalysis_CreateAlphaTexture(a, kinds[k].type, &b, px, size);
            for (UINT32 i = 0; i < size; i++) ink += px[i] != 0;
            snprintf(what, sizeof what, "%s texture has ink", kinds[k].name);
            ok(hr == S_OK && ink > 0, what, hr); free(px);
        }
        IDWriteGlyphRunAnalysis_Release(a);
    }
    sink_t sink = { { &sink_vtbl }, 0, 0 };
    hr = IDWriteFontFace_GetGlyphRunOutline(face, 32, glyphs, adv, offs, 2, FALSE, FALSE, (IDWriteGeometrySink *)&sink.iface);
    ok(hr == S_OK && sink.figures > 0 && sink.segments > 0, "outline has segments", hr);
    printf("%d failures\n", failures);
    return failures != 0;
}
