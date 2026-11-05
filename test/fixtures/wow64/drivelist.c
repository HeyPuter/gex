/* The prefix's drives in a session wineboot started: its MountMgr makes the
 * \??\X: links GetLogicalDrives reads. With MountMgr demand-start the mask
 * was 0 in every process, and an installer that picks a drive to extract to
 * (the .NET Framework 4.8 one) gave up at once. Built for i386 and x86_64. */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    DWORD drives = GetLogicalDrives();
    char strings[256], *s;
    int has_c = 0;

    if (GetLogicalDriveStringsA(sizeof(strings), strings))
        for (s = strings; *s; s += lstrlenA(s) + 1)
            if (!lstrcmpiA(s, "C:\\")) has_c = 1;
    printf("drives %08lx, C:\\ %s in the drive strings\n", drives, has_c ? "is" : "is not");
    if ((drives & (1 << 2)) && has_c) printf("drivelist ok\n");
    return !((drives & (1 << 2)) && has_c);
}
