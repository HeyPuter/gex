/* The guest's low memory belongs to the guest. A Worker starting a thread
 * once ran its first shim call on wasm-ld's default stack -- the top of the
 * first 8 MiB, where a Windows program's image lies (noita.exe's .text) --
 * and overwrote a kilobyte or two below 0x800000 per thread.
 *
 * So this image is linked at 0x7f0000 with 72 KiB of code-section ballast,
 * putting its .text across 0x800000; it copies its .text, starts and ends
 * threads, and compares. Prints one line per check.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -Wl,--image-base=0x7f0000 -Wl,--disable-dynamicbase -Wl,--disable-reloc-section */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

/* Ballast in .text, so the section reaches past 0x800000. */
__attribute__((section(".text"), used)) static const unsigned char ballast[72 * 1024] = { 1 };

static DWORD WINAPI quick(void *p) { return (DWORD)(INT_PTR)p; }

int main(void) {
    unsigned char *base = (unsigned char *)GetModuleHandleA(0);
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)(base + ((IMAGE_DOS_HEADER *)base)->e_lfanew);
    IMAGE_SECTION_HEADER *sec = IMAGE_FIRST_SECTION(nt);
    unsigned char *text = base + sec->VirtualAddress;
    size_t len = sec->Misc.VirtualSize;
    char name[128];
    snprintf(name, sizeof name, ".text at %p..%p covers 0x7ff000..0x800000", text, text + len);
    CHECK(name, (UINT_PTR)text <= 0x7ff000 && (UINT_PTR)(text + len) >= 0x800000);
    unsigned char *copy = malloc(len);
    memcpy(copy, text, len);
    for (int round = 0; round < 4; round++) {
        HANDLE t[8];
        for (int i = 0; i < 8; i++) t[i] = CreateThread(0, 0, quick, (void *)(INT_PTR)i, 0, 0);
        WaitForMultipleObjects(8, t, TRUE, INFINITE);
        for (int i = 0; i < 8; i++) CloseHandle(t[i]);
    }
    size_t bad = 0, first = 0;
    for (size_t i = 0; i < len; i++) {
        if (text[i] != copy[i]) { if (!bad) first = (size_t)(text + i); bad++; }
    }
    snprintf(name, sizeof name, "32 threads later, %zu bytes of .text changed (first at %#zx)", bad, first);
    CHECK(name, bad == 0);
    printf("%d failures\n", failures);
    return failures;
}
