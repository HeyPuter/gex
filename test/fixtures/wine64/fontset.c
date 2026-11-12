/* IDWriteFontSet::GetMatchingFonts(family, weight, stretch, style): the
 * system set's Tahoma asked for bold has the bold face first; and a set
 * built from a font file in memory (CreateInMemoryFontFileLoader,
 * AddFontFile), as programs that ship their own fonts make, finds it by
 * its family. */
#define COBJMACROS
#include <windows.h>
#include <initguid.h>
#include <dwrite_3.h>
#include <stdio.h>

/* The weight of `set`'s first font, -1 for none. */
static int first_weight(IDWriteFontSet *set)
{
    IDWriteFontFaceReference *ref;
    IDWriteFontFace3 *face;
    int w = -1;

    if (!IDWriteFontSet_GetFontCount(set) || FAILED(IDWriteFontSet_GetFontFaceReference(set, 0, &ref)))
        return -1;
    if (SUCCEEDED(IDWriteFontFaceReference_CreateFontFace(ref, &face)))
    {
        w = IDWriteFontFace3_GetWeight(face);
        IDWriteFontFace3_Release(face);
    }
    IDWriteFontFaceReference_Release(ref);
    return w;
}

/* The bytes of `set`'s first font's file. */
static void *first_file(IDWriteFontSet *set, UINT32 *size)
{
    IDWriteFontFaceReference *ref;
    IDWriteFontFileStream *stream;
    IDWriteFontFileLoader *loader;
    IDWriteFontFile *file;
    const void *key, *frag;
    void *ctx, *out = NULL;
    UINT32 keysize;
    UINT64 len;

    if (FAILED(IDWriteFontSet_GetFontFaceReference(set, 0, &ref)))
        return NULL;
    if (SUCCEEDED(IDWriteFontFaceReference_GetFontFile(ref, &file)))
    {
        if (SUCCEEDED(IDWriteFontFile_GetReferenceKey(file, &key, &keysize))
                && SUCCEEDED(IDWriteFontFile_GetLoader(file, &loader)))
        {
            if (SUCCEEDED(IDWriteFontFileLoader_CreateStreamFromKey(loader, key, keysize, &stream)))
            {
                if (SUCCEEDED(IDWriteFontFileStream_GetFileSize(stream, &len))
                        && SUCCEEDED(IDWriteFontFileStream_ReadFileFragment(stream, &frag, 0, len, &ctx)))
                {
                    if ((out = malloc(len))) memcpy(out, frag, len);
                    *size = len;
                    IDWriteFontFileStream_ReleaseFileFragment(stream, ctx);
                }
                IDWriteFontFileStream_Release(stream);
            }
            IDWriteFontFileLoader_Release(loader);
        }
        IDWriteFontFile_Release(file);
    }
    IDWriteFontFaceReference_Release(ref);
    return out;
}

int main(int argc, char **argv)
{
    IDWriteInMemoryFontFileLoader *mem;
    IDWriteFontSet *sys, *got, *own;
    IDWriteFontSetBuilder1 *builder;
    IDWriteFactory5 *factory;
    IDWriteFontFile *file;
    UINT32 size = 0;
    void *bytes;
    HRESULT hr;
    int bold, regular, inmem;

    if (FAILED(hr = DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED, &IID_IDWriteFactory5, (IUnknown **)&factory)))
    {
        printf("fontset: DWriteCreateFactory %#lx\n", hr);
        return 1;
    }
    IDWriteFactory5_GetSystemFontSet(factory, &sys);
    hr = IDWriteFontSet_GetMatchingFonts_(sys, L"Tahoma", DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_STRETCH_NORMAL,
            DWRITE_FONT_STYLE_NORMAL, &got);
    if (FAILED(hr))
    {
        printf("fontset: GetMatchingFonts %#lx\n", hr);
        return 1;
    }
    bold = first_weight(got);
    IDWriteFontSet_Release(got);
    IDWriteFontSet_GetMatchingFonts_(sys, L"tahoma", DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL,
            DWRITE_FONT_STYLE_NORMAL, &got);
    regular = first_weight(got);
    bytes = first_file(got, &size);
    IDWriteFontSet_Release(got);
    printf("fontset: system bold %d regular %d\n", bold, regular);

    /* Tahoma's file again, now from memory, in a set of its own */
    inmem = -1;
    if (bytes && SUCCEEDED(IDWriteFactory5_CreateInMemoryFontFileLoader(factory, &mem)))
    {
        IDWriteFactory5_RegisterFontFileLoader(factory, (IDWriteFontFileLoader *)mem);
        if (SUCCEEDED(IDWriteInMemoryFontFileLoader_CreateInMemoryFontFileReference(mem, (IDWriteFactory *)factory,
                bytes, size, NULL, &file)))
        {
            IDWriteFactory5_CreateFontSetBuilder(factory, &builder);
            IDWriteFontSetBuilder1_AddFontFile(builder, file);
            IDWriteFontSetBuilder1_CreateFontSet(builder, &own);
            if (SUCCEEDED(IDWriteFontSet_GetMatchingFonts_(own, L"Tahoma", DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, &got)))
            {
                inmem = IDWriteFontSet_GetFontCount(got) ? first_weight(got) : 0;
                IDWriteFontSet_Release(got);
            }
            IDWriteFontSet_Release(own);
            IDWriteFontSetBuilder1_Release(builder);
            IDWriteFontFile_Release(file);
        }
    }
    printf("fontset: in memory %d\n", inmem);
    if (bold == DWRITE_FONT_WEIGHT_BOLD && regular == DWRITE_FONT_WEIGHT_NORMAL && inmem == DWRITE_FONT_WEIGHT_NORMAL)
        printf("fontset ok\n");
    return 0;
}
