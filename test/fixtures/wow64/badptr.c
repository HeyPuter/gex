/* A fault in native code that a system call makes on a bad pointer its caller
 * passed in: NtQueryInformationProcess writing its result to an address past
 * the guest's memory. The fault is the 64-bit side's in a WoW64 process (the
 * thunk's), the call's own in 32-bit Wine; either way the system call fails
 * with STATUS_ACCESS_VIOLATION and the caller goes on, as on Windows.
 * Prints "badptr ok" and exits 0.
 */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>

int main(void)
{
    void *bad = (void *)0xfffff000;
    ULONG len = 0;
    NTSTATUS s = NtQueryInformationProcess(GetCurrentProcess(), ProcessBasicInformation, bad,
                                           sizeof(PROCESS_BASIC_INFORMATION), &len);
    if (s != (NTSTATUS)0xc0000005) { printf("badptr FAIL: status %08lx\n", s); return 1; }
    printf("badptr ok\n");
    return 0;
}
