/* xcload2 OP...: each OP is a DLL name, loaded and its `work` run once ("NAME x=X base=B size=S"),
 * or "-", which unloads every DLL loaded so far, newest first. Loading DLLs in one order, unloading
 * them and loading them in another puts each image where the other was.
 * gex-nt-arch: x64 */
#include <windows.h>
#include <stdio.h>

int main(int argc, char **argv)
{
	HMODULE held[16];
	int n = 0;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "-")) {
			while (n) FreeLibrary(held[--n]);
			printf("unloaded\n");
			continue;
		}
		if (n == 16) return 2;
		HMODULE m = LoadLibraryA(argv[i]);
		if (!m) { printf("LoadLibrary %s: %lu\n", argv[i], GetLastError()); return 3; }
		held[n++] = m;
		unsigned (*work)(unsigned) = (void *)GetProcAddress(m, "work");
		if (!work) return 4;
		IMAGE_NT_HEADERS64 *nt = (void *)((char *)m + ((IMAGE_DOS_HEADER *)m)->e_lfanew);
		printf("%s x=%u base=%p size=%lx\n", argv[i], work(1), (void *)m,
		       (unsigned long)nt->OptionalHeader.SizeOfImage);
		fflush(stdout);
	}
	return 0;
}
