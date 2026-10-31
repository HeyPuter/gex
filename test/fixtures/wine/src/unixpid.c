/* unixpid: prints "unix pid N", N the Unix pid of the process it runs in
 * (the first field of Z:\proc\self\stat), and exits 42.
 * gex-nt-arch: x64 */
#include <windows.h>
#include <stdio.h>

int main(void)
{
	char buf[64] = { 0 };
	DWORD got = 0;
	HANDLE f = CreateFileA("Z:\\proc\\self\\stat", GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, 0, NULL);
	if (f == INVALID_HANDLE_VALUE) { printf("no Z:\\proc\\self\\stat (%lu)\n", GetLastError()); return 1; }
	if (!ReadFile(f, buf, sizeof(buf) - 1, &got, NULL) || !got) { printf("cannot read it (%lu)\n", GetLastError()); return 1; }
	CloseHandle(f);
	printf("unix pid %d\n", atoi(buf));
	return 42;
}
