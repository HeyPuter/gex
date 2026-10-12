/* A call through a null function pointer, caught: Windows 7's comctl32 does
 * this whenever an Edit control is made without lpk.dll (it calls
 * LpkEditControl's table) and relies on the access violation being caught.
 * The page must deliver the fault to Wine as natively (W3: a branch to
 * non-executable memory goes to ntdll.so's fault hook). */
#include <windows.h>
#include <stdio.h>

static LONG caught;

static LONG CALLBACK veh(EXCEPTION_POINTERS *e)
{
    if (e->ExceptionRecord->ExceptionCode != EXCEPTION_ACCESS_VIOLATION) return EXCEPTION_CONTINUE_SEARCH;
    if (e->ExceptionRecord->ExceptionInformation[0] != 8) return EXCEPTION_CONTINUE_SEARCH;
    /* return to the caller, as if the null function had returned */
#ifdef _WIN64
    e->ContextRecord->Rip = *(DWORD64 *)e->ContextRecord->Rsp;
    e->ContextRecord->Rsp += 8;
#else
    e->ContextRecord->Eip = *(DWORD *)e->ContextRecord->Esp;
    e->ContextRecord->Esp += 4;
#endif
    caught++;
    return EXCEPTION_CONTINUE_EXECUTION;
}

int main(void)
{
    void (*volatile f)(void) = 0;
    AddVectoredExceptionHandler(1, veh);
    for (int i = 0; i < 3; i++) f();
    printf("nullcall caught %ld\n", caught);
    fflush(stdout);
    return caught == 3 ? 0 : 1;
}
