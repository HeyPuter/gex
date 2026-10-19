/* Loads and unloads N DLLs one after another (plugNN.dll, distinct images at distinct bases), running each
 * one's code once, as a plugin scan's verify step does. Prints "dllcycle N x". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv)
{
	int n = argc > 1 ? atoi(argv[1]) : 40, rounds = argc > 2 ? atoi(argv[2]) : 1, linger = argc > 3 ? atoi(argv[3]) : 0;
	unsigned x = 1;
	setvbuf(stdout, NULL, _IONBF, 0);
	for (int r = 0; r < rounds; r++)
		for (int i = 0; i < n; i++) {
			char name[32];
			sprintf(name, "plug%02d.dll", i);
			HMODULE m = LoadLibraryA(name);
			if (!m) { printf("dllcycle no %s (%lu)\n", name, GetLastError()); return 1; }
			unsigned (*work)(unsigned) = (void *)GetProcAddress(m, "work");
			if (!work) { printf("dllcycle no work in %s\n", name); return 1; }
			x = work(x);
			if (!FreeLibrary(m)) { printf("dllcycle FreeLibrary %s failed\n", name); return 1; }
			if (GetModuleHandleA(name)) { printf("dllcycle %s still loaded\n", name); return 1; }
		}
	printf("dllcycle %d x %u\n", n * rounds, x);
	if (linger) { printf("dllcycle lingering\n"); Sleep(linger * 1000); }
	return 0;
}
