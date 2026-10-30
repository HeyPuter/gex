/* xcload DLL [move]: load DLL, run its `work` once, and print what it returns, where the DLL was
 * loaded and its image size. With `move`, the DLL is loaded and unloaded first and the range it
 * had is reserved, so the second load places it elsewhere and relocates it for there.
 * gex-nt-arch: x64 */
#include <windows.h>
#include <stdio.h>

int main(int argc, char **argv)
{
	if (argc < 2) return 2;
	if (argc > 2 && !strcmp(argv[2], "move")) {
		HMODULE first = LoadLibraryA(argv[1]);
		if (!first) { printf("LoadLibrary %s: %lu\n", argv[1], GetLastError()); return 3; }
		IMAGE_NT_HEADERS64 *nt = (void *)((char *)first + ((IMAGE_DOS_HEADER *)first)->e_lfanew);
		SIZE_T size = nt->OptionalHeader.SizeOfImage;
		FreeLibrary(first);
		if (!VirtualAlloc(first, size, MEM_RESERVE, PAGE_NOACCESS)) { printf("cannot reserve %p\n", (void *)first); return 4; }
		printf("first base=%p\n", (void *)first);
	}
	HMODULE m = LoadLibraryA(argv[1]);
	if (!m) { printf("LoadLibrary %s: %lu\n", argv[1], GetLastError()); return 5; }
	unsigned (*work)(unsigned) = (void *)GetProcAddress(m, "work");
	if (!work) return 6;
	IMAGE_NT_HEADERS64 *nt = (void *)((char *)m + ((IMAGE_DOS_HEADER *)m)->e_lfanew);
	printf("x=%u base=%p size=%lx\n", work(1), (void *)m, (unsigned long)nt->OptionalHeader.SizeOfImage);
	return 0;
}
