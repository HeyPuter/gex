/* Text measured as it is drawn: for Tahoma at -11, -12 and -13 (Wine's
 * tahoma.ttf has embedded bitmap strikes there, and its gasp table draws
 * them without antialiasing), every glyph's advance from GGO_METRICS equals
 * the GGO_BITMAP glyph's that the DIB driver draws with, GetCharWidth32's
 * and GetCharABCWidths' A+B+C, in each quality. A string drawn with
 * ExtTextOutW's own advances and at the positions GetTextExtentExPointW
 * measured is the same pixels, for Tahoma and Courier New (Tahoma in Wine's
 * prefix), regular and bold, upright and italic, -11 to -16, in each quality
 * including ClearType. Tahoma Bold's glyphs are bold at every size from -9
 * to -16, its bitmap strikes' too.
 * gex-nt-arch: x64
 * gex-nt-libs: -lgdi32
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
static const MAT2 identity = {{0, 1}, {0, 0}, {0, 0}, {0, 1}};

static void probe(int height, int quality)
{
    static const WCHAR chars[] = L"iWLoa mjl.,1";
    HDC dc = CreateCompatibleDC(NULL);
    HFONT font = CreateFontW(height, 0, 0, 0, FW_NORMAL, 0, 0, 0, DEFAULT_CHARSET, 0, 0, quality, 0, L"Tahoma");
    int bad = 0, k;

    SelectObject(dc, font);
    for (k = 0; chars[k]; k++)
    {
        GLYPHMETRICS met, bit;
        INT width;
        ABC abc;
        int sum;

        if (GetGlyphOutlineW(dc, chars[k], GGO_METRICS, &met, 0, NULL, &identity) == GDI_ERROR ||
            GetGlyphOutlineW(dc, chars[k], GGO_BITMAP, &bit, 0, NULL, &identity) == GDI_ERROR ||
            !GetCharWidth32W(dc, chars[k], chars[k], &width) ||
            !GetCharABCWidthsW(dc, chars[k], chars[k], &abc))
        {
            printf("FAIL Tahoma %d quality %d '%lc': a call failed\n", height, quality, chars[k]);
            bad++;
            continue;
        }
        sum = abc.abcA + (int)abc.abcB + abc.abcC;
        if (met.gmCellIncX != bit.gmCellIncX || width != bit.gmCellIncX || sum != bit.gmCellIncX)
        {
            printf("FAIL Tahoma %d quality %d '%lc': metrics %d, bitmap %d, char width %d, abc %d\n",
                   height, quality, chars[k], met.gmCellIncX, bit.gmCellIncX, width, sum);
            bad++;
        }
    }
    if (!bad) printf("ok Tahoma %d quality %d: measured as drawn\n", height, quality);
    failures += bad;
    DeleteDC(dc);
    DeleteObject(font);
}

/* The string drawn with its own advances and with the measured ones. */
static int drawn_as_measured(const WCHAR *face, int height, int weight, int italic, int quality)
{
    static const WCHAR text[] = L"encoding=\"UTF-8\" ABC XYZ Wilma ij.,1";
    int len = lstrlenW(text), fit, k, diff = 0;
    INT ext[64], dx[64];
    BITMAPINFO bi = {{sizeof(BITMAPINFOHEADER), 400, -40, 1, 32, BI_RGB}};
    void *bits[2];
    HBITMAP bmp[2];
    HDC dc[2];
    SIZE sz;
    HFONT font = CreateFontW(height, 0, 0, 0, weight, italic, 0, 0, DEFAULT_CHARSET, 0, 0, quality, 0, face);

    for (k = 0; k < 2; k++)
    {
        dc[k] = CreateCompatibleDC(NULL);
        bmp[k] = CreateDIBSection(dc[k], &bi, DIB_RGB_COLORS, &bits[k], NULL, 0);
        SelectObject(dc[k], bmp[k]);
        SelectObject(dc[k], font);
        PatBlt(dc[k], 0, 0, 400, 40, WHITENESS);
    }
    GetTextExtentExPointW(dc[1], text, len, 0, &fit, ext, &sz);
    for (k = 0; k < len; k++) dx[k] = ext[k] - (k ? ext[k - 1] : 0);
    ExtTextOutW(dc[0], 2, 2, 0, NULL, text, len, NULL);
    ExtTextOutW(dc[1], 2, 2, 0, NULL, text, len, dx);
    GdiFlush();
    for (k = 0; k < 400 * 40; k++) diff += ((DWORD *)bits[0])[k] != ((DWORD *)bits[1])[k];
    for (k = 0; k < 2; k++)
    {
        DeleteDC(dc[k]);
        DeleteObject(bmp[k]);
    }
    DeleteObject(font);
    return diff;
}

static void drawn(const WCHAR *face, int quality)
{
    int h, w, i, bad = 0, diff;

    for (h = -11; h >= -16; h--)
        for (w = FW_NORMAL; w <= FW_BOLD; w += FW_BOLD - FW_NORMAL)
            for (i = 0; i < 2; i++)
                if ((diff = drawn_as_measured(face, h, w, i, quality)))
                {
                    printf("FAIL %ls %d weight %d%s quality %d: drawn %d pixels off the measured positions\n",
                           face, h, w, i ? " italic" : "", quality, diff);
                    bad++;
                }
    if (!bad) printf("ok %ls quality %d: drawn as measured, -11 to -16, regular and bold, upright and italic\n", face, quality);
    failures += bad;
}

/* One font selected into a 1-bpp and a 32-bpp DC at the same time draws mono in
 * the first and smoothed in the second.  Each DC measures as it draws, also
 * right after the font was selected into the other one. */
struct depth_dcs
{
    HDC dc[2];
    HBITMAP bmp[2];
    void *bits[2];
    int size;
};

static void depth_dcs_init(struct depth_dcs *d, int bpp, HFONT font)
{
    struct { BITMAPINFOHEADER h; RGBQUAD c[2]; } bi = {{sizeof(BITMAPINFOHEADER), 400, -40, 1, bpp, BI_RGB},
                                                       {{0, 0, 0, 0}, {255, 255, 255, 0}}};
    int k;

    d->size = ((400 * bpp + 31) / 32) * 4 * 40;
    for (k = 0; k < 2; k++)
    {
        d->dc[k] = CreateCompatibleDC(NULL);
        d->bmp[k] = CreateDIBSection(d->dc[k], (BITMAPINFO *)&bi, DIB_RGB_COLORS, &d->bits[k], NULL, 0);
        SelectObject(d->dc[k], d->bmp[k]);
        SelectObject(d->dc[k], font);
    }
}

static void depth_dcs_free(struct depth_dcs *d)
{
    int k;

    for (k = 0; k < 2; k++)
    {
        SelectObject(d->dc[k], GetStockObject(SYSTEM_FONT));
        DeleteDC(d->dc[k]);
        DeleteObject(d->bmp[k]);
    }
}

static void reselect(struct depth_dcs *d, HFONT font)
{
    int k;

    for (k = 0; k < 2; k++)
    {
        SelectObject(d->dc[k], GetStockObject(SYSTEM_FONT));
        SelectObject(d->dc[k], font);
    }
}

static int depth_drawn_as_measured(struct depth_dcs *d)
{
    static const WCHAR text[] = L"encoding=\"UTF-8\" ABC XYZ Wilma ij.,1";
    int len = lstrlenW(text), fit, k, diff = 0;
    INT ext[64], dx[64];
    SIZE sz;

    GetTextExtentExPointW(d->dc[1], text, len, 0, &fit, ext, &sz);
    for (k = 0; k < len; k++) dx[k] = ext[k] - (k ? ext[k - 1] : 0);
    for (k = 0; k < 2; k++) PatBlt(d->dc[k], 0, 0, 400, 40, WHITENESS);
    ExtTextOutW(d->dc[0], 2, 2, 0, NULL, text, len, NULL);
    ExtTextOutW(d->dc[1], 2, 2, 0, NULL, text, len, dx);
    GdiFlush();
    for (k = 0; k < d->size; k++) diff += ((BYTE *)d->bits[0])[k] != ((BYTE *)d->bits[1])[k];
    return diff;
}

static void shared_across_depths(const WCHAR *face, int quality)
{
    int h, w, round, bad = 0, diff;

    for (h = -11; h >= -16; h--)
        for (w = FW_NORMAL; w <= FW_BOLD; w += FW_BOLD - FW_NORMAL)
        {
            HFONT font = CreateFontW(h, 0, 0, 0, w, 0, 0, 0, DEFAULT_CHARSET, 0, 0, quality, 0, face);
            struct depth_dcs mono, rgb;

            depth_dcs_init(&mono, 1, font);
            depth_dcs_init(&rgb, 32, font);
            for (round = 0; round < 2; round++)
            {
                reselect(&rgb, font);
                if ((diff = depth_drawn_as_measured(&mono)))
                {
                    printf("FAIL %ls %d weight %d quality %d, 1 bpp after a 32 bpp select: drawn %d bytes off the measured positions\n",
                           face, h, w, quality, diff);
                    bad++;
                }
                reselect(&mono, font);
                if ((diff = depth_drawn_as_measured(&rgb)))
                {
                    printf("FAIL %ls %d weight %d quality %d, 32 bpp after a 1 bpp select: drawn %d bytes off the measured positions\n",
                           face, h, w, quality, diff);
                    bad++;
                }
            }
            depth_dcs_free(&mono);
            depth_dcs_free(&rgb);
            DeleteObject(font);
        }
    if (!bad) printf("ok %ls quality %d: one font in a 1 bpp and a 32 bpp DC, each drawn as measured\n", face, quality);
    failures += bad;
}

/* A glyph's GGO_BITMAP image (rows of bytes, DWORD-aligned). */
static DWORD glyph_bits(HDC dc, WCHAR c, GLYPHMETRICS *gm, BYTE *buf, DWORD size)
{
    return GetGlyphOutlineW(dc, c, GGO_BITMAP, gm, size, buf, &identity);
}

static void bold_is_bold(int height)
{
    static const WCHAR chars[] = L"UBHn";
    HDC dc = CreateCompatibleDC(NULL);
    HFONT regular = CreateFontW(height, 0, 0, 0, FW_NORMAL, 0, 0, 0, DEFAULT_CHARSET, 0, 0, NONANTIALIASED_QUALITY, 0, L"Tahoma");
    HFONT bold = CreateFontW(height, 0, 0, 0, FW_BOLD, 0, 0, 0, DEFAULT_CHARSET, 0, 0, NONANTIALIASED_QUALITY, 0, L"Tahoma");
    static BYTE a[4096], b[4096];
    int k, bad = 0;

    for (k = 0; chars[k]; k++)
    {
        GLYPHMETRICS ga, gb;
        DWORD na, nb;

        SelectObject(dc, regular);
        na = glyph_bits(dc, chars[k], &ga, a, sizeof(a));
        SelectObject(dc, bold);
        nb = glyph_bits(dc, chars[k], &gb, b, sizeof(b));
        if (na == GDI_ERROR || nb == GDI_ERROR ||
            (na == nb && ga.gmBlackBoxX == gb.gmBlackBoxX && ga.gmBlackBoxY == gb.gmBlackBoxY && !memcmp(a, b, na)))
        {
            printf("FAIL Tahoma %d '%lc': the bold glyph is the regular one (advance %d, regular %d)\n",
                   height, chars[k], gb.gmCellIncX, ga.gmCellIncX);
            bad++;
        }
    }
    if (!bad) printf("ok Tahoma Bold %d: bold glyphs\n", height);
    failures += bad;
    SelectObject(dc, GetStockObject(SYSTEM_FONT));
    DeleteDC(dc);
    DeleteObject(regular);
    DeleteObject(bold);
}

int main(void)
{
    static const int heights[] = {-11, -12, -13};
    static const int qualities[] = {DEFAULT_QUALITY, NONANTIALIASED_QUALITY, ANTIALIASED_QUALITY};
    int h, q;

    for (h = 0; h < 3; h++)
        for (q = 0; q < 3; q++)
            probe(heights[h], qualities[q]);
    for (q = 0; q < 3; q++)
    {
        drawn(L"Tahoma", qualities[q]);
        drawn(L"Courier New", qualities[q]);
    }
    drawn(L"Tahoma", CLEARTYPE_QUALITY);
    drawn(L"Courier New", CLEARTYPE_QUALITY);
    shared_across_depths(L"Tahoma", ANTIALIASED_QUALITY);
    shared_across_depths(L"Tahoma", CLEARTYPE_QUALITY);
    shared_across_depths(L"Courier New", ANTIALIASED_QUALITY);
    for (h = -9; h >= -16; h--)
        bold_is_bold(h);
    printf("%d failures\n", failures);
    return failures != 0;
}
