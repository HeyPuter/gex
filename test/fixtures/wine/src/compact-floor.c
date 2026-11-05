/* Sleeps argv[1] ms (default 6000) between a start and an end line: a
 * program for the session programs to serve.
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>
int main(int c, char **v) { int ms = c > 1 ? atoi(v[1]) : 6000; printf("compact-floor start %d\n", ms); fflush(stdout); Sleep(ms); printf("compact-floor end\n"); return 0; }
