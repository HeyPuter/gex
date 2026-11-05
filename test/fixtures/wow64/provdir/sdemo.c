/* test/wow64/provdir: the library a stateful provider replaces. */
__declspec(dllexport) int __cdecl sd_add(int a, int b) { return a + b; }
__declspec(dllexport) int __cdecl sd_other(void) { return 7; }
