// The function JIT must forget what it learned of code that is unmapped: a
// dlclose and a later dlopen put another library at the same addresses. Code
// A has a function at +0x10; code B, mapped over it, has a min() whose cmp is
// before +0x10 and whose cmovle is at it. Kept, A's start split B's function
// there as at a tail call, with the flags dead (HK's libdbus and libxcb in the
// page, docs/unity-dotnet.md). Output must match native.
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

typedef int (*fn)(int, int);

static void *at;

// The first call picks the address; the second maps over the same one.
static void *place(const unsigned char *code, size_t n) {
	void *p = mmap(at, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | (at ? MAP_FIXED : 0), -1, 0);
	if (p == MAP_FAILED || (at && p != at)) return 0;
	at = p;
	memset(p, 0xcc, 4096);
	memcpy(p, code, n);
	if (mprotect(p, 4096, PROT_READ | PROT_EXEC)) return 0;
	return p;
}

int main(void) {
	// A: +0x00 ret; +0x10 lea (%rdi,%rsi),%eax; ret
	unsigned char a[0x14] = {0xc3};
	memset(a + 1, 0x90, 0xf);
	a[0x10] = 0x8d; a[0x11] = 0x04; a[0x12] = 0x37; a[0x13] = 0xc3;
	char *p = place(a, sizeof a);
	if (!p) { puts("map A failed"); return 1; }
	volatile fn f = (fn)(p + 0x10);
	long s = 0;
	for (int i = 0; i < 200000; i++) s += f(i, 1);
	printf("A %ld\n", s);
	if (munmap(p, 4096)) { puts("munmap failed"); return 1; }
	// B: mov %esi,%eax; cmp %esi,%edi; nop x12; +0x10 cmovle %edi,%eax; ret
	unsigned char b[0x14] = {0x89, 0xf0, 0x39, 0xf7};
	memset(b + 4, 0x90, 0xc);
	b[0x10] = 0x0f; b[0x11] = 0x4e; b[0x12] = 0xc7; b[0x13] = 0xc3;
	p = place(b, sizeof b);
	if (!p) { puts("map B failed"); return 1; }
	volatile fn g = (fn)p;
	long bad = 0, t = 0;
	for (int i = 0; i < 200000; i++) {
		int x = (i * 7919) % 1000 - 500, y = (i * 104729) % 1000 - 500;
		int m = g(x, y);
		t += m;
		if (m != (x < y ? x : y)) bad++;
	}
	printf("B %ld bad %ld\n", t, bad);
	return 0;
}
