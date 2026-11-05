/* test/wow64/provfill: a new thread's provider stack when the guest's address
 * space is full (with ?winesplit=1 in the page).
 *
 * The address space is filled, then given back a block at a time only until
 * Wine can make a thread (its stack and TEB), and what the thread left is
 * taken again at once, before the thread's Worker has started. The Worker
 * then needs a 1 MiB stack for the stateful providers its Wine DLLs
 * bring (wasmdll data, export providers), which nothing in the guest's space
 * has room for any more. In a split 32-bit Wine process that stack comes from the
 * shim's heap above 2 GiB; from the guest's, the process would end with
 * "no memory for a provider stack". Prints "provfill ok". */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static HANDLE done;

static DWORD WINAPI worker(void *arg) {
    (void)arg;
    SetEvent(done);
    return 0;
}

#define SMALL (64u << 10)
#define MAX_SMALL 65536

static void *small[MAX_SMALL]; /* every block taken */

int main(void) {
    done = CreateEventA(NULL, TRUE, FALSE, NULL);
    /* Every block, in the order taken: given back last first, the smallest. */
    SIZE_T chunks[] = {64u << 20, 4u << 20, 1u << 20, 256u << 10, SMALL};
    SIZE_T taken = 0;
    unsigned n = 0;
    for (unsigned i = 0; i < sizeof(chunks) / sizeof(chunks[0]); i++) {
        while (n < MAX_SMALL && (small[n] = VirtualAlloc(NULL, chunks[i], MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE))) {
            taken += chunks[i];
            n++;
        }
    }
    printf("%s filled %lu MiB of the address space\n", taken > (512u << 20) ? "ok  " : "FAIL",
           (unsigned long)(taken >> 20));
    fflush(stdout);

    /* Back only as much as a thread needs. */
    HANDLE t = NULL;
    SIZE_T freed = 0;
    while (!t && n > 0) {
        t = CreateThread(NULL, SMALL, worker, NULL, STACK_SIZE_PARAM_IS_A_RESERVATION, NULL);
        if (!t) {
            MEMORY_BASIC_INFORMATION mi;
            VirtualQuery(small[--n], &mi, sizeof(mi));
            freed += mi.RegionSize;
            VirtualFree(small[n], 0, MEM_RELEASE);
        }
    }
    /* Take back what the thread left, before its Worker is up (a Worker
     * takes milliseconds to start; this takes microseconds): its provider
     * stack then finds no 1 MiB in the guest's space. */
    SIZE_T again = 0;
    for (unsigned i = 2; t && i < sizeof(chunks) / sizeof(chunks[0]); i++) {
        while (n < MAX_SMALL && (small[n] = VirtualAlloc(NULL, chunks[i], MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE))) {
            again += chunks[i];
            n++;
        }
    }
    printf("%s a thread made after giving back %lu KiB (%lu KiB taken again)\n", t ? "ok  " : "FAIL",
           (unsigned long)(freed >> 10), (unsigned long)(again >> 10));
    fflush(stdout);
    if (!t) return 1;
    DWORD w = WaitForSingleObject(done, 60000);
    printf("%s the thread ran\n", w == WAIT_OBJECT_0 ? "ok  " : "FAIL");
    fflush(stdout);
    if (w != WAIT_OBJECT_0) return 1;
    printf("provfill ok\n");
    fflush(stdout);
    return 0;
}
