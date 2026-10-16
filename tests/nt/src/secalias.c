/* A section mapped more than once in one process: the views share their
 * pages, and each stays mapped until its own unmap. A second view at the
 * same offset (the named section opened again), one at an offset inside
 * the first, and a read-only one; unmapped in an order that leaves the
 * later views after the first is gone. A copy view (FILE_MAP_COPY) keeps
 * its writes to itself. */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#define GRAN (64 * 1024)

int main(void) {
    HANDLE s1 = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 4 * GRAN, "gex-secalias");
    HANDLE s2 = OpenFileMappingA(FILE_MAP_ALL_ACCESS, FALSE, "gex-secalias");
    CHECK("a named section, opened twice", s1 && s2);
    volatile unsigned *v1 = MapViewOfFile(s1, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    volatile unsigned *v2 = MapViewOfFile(s2, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    /* at the third granule, two of them long */
    volatile unsigned *v3 = MapViewOfFile(s1, FILE_MAP_ALL_ACCESS, 0, 2 * GRAN, 2 * GRAN);
    volatile unsigned *v4 = MapViewOfFile(s2, FILE_MAP_READ, 0, GRAN, GRAN);
    CHECK("four views", v1 && v2 && v3 && v4);
    if (!(v1 && v2 && v3 && v4)) {
        printf("%d failures\n", failures + 1);
        return 1;
    }

    v1[0] = 0x1234;
    v1[2 * GRAN / 4 + 5] = 0x5678;
    v1[GRAN / 4 + 7] = 0x9abc;
    CHECK("the same offset: a write through one is read through the other", v2[0] == 0x1234);
    CHECK("an offset inside: the write at its offset", v3[5] == 0x5678);
    CHECK("a read-only view inside: the write at its offset", v4[7] == 0x9abc);
    v3[1] = 0xdef0;
    CHECK("and back: a write through the inner view", v1[2 * GRAN / 4 + 1] == 0xdef0);

    /* a copy view's pages are its own: its writes reach no other view */
    volatile unsigned *vc = MapViewOfFile(s1, FILE_MAP_COPY, 0, 0, 0);
    CHECK("a copy view reads the section", vc && vc[0] == 0x1234);
    if (vc) {
        vc[0] = 0x7777;
        CHECK("a write through the copy stays in the copy", vc[0] == 0x7777 && v1[0] == 0x1234 && v2[0] == 0x1234);
        volatile unsigned *v6 = MapViewOfFile(s2, FILE_MAP_ALL_ACCESS, 0, 0, 0);
        CHECK("a plain view mapped after it does not see it", v6 && v6[0] == 0x1234);
        if (v6) CHECK("unmap that plain view", UnmapViewOfFile((void *)v6));
        CHECK("unmap the copy", UnmapViewOfFile((void *)vc));
    }

    CHECK("unmap the first", UnmapViewOfFile((void *)v1));
    CHECK("the second still reads", v2[0] == 0x1234);
    CHECK("the inner one still reads", v3[5] == 0x5678);
    v2[3] = 0x4242;
    CHECK("unmap the inner one", UnmapViewOfFile((void *)v3));
    CHECK("the second still writes and reads", v2[3] == 0x4242 && v2[2 * GRAN / 4 + 1] == 0xdef0);
    CHECK("the read-only one still reads", v4[7] == 0x9abc);
    CHECK("unmap the second", UnmapViewOfFile((void *)v2));
    CHECK("unmap the read-only one", UnmapViewOfFile((void *)v4));
    CHECK("a view unmapped is not unmapped again", !UnmapViewOfFile((void *)v4));

    /* the section still holds its pages: a new view reads them */
    volatile unsigned *v5 = MapViewOfFile(s1, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    CHECK("a new view after all are gone reads the section", v5 && v5[0] == 0x1234 && v5[3] == 0x4242);
    if (v5) UnmapViewOfFile((void *)v5);
    CloseHandle(s1);
    CloseHandle(s2);
    printf("%d failures\n", failures);
    return failures != 0;
}
