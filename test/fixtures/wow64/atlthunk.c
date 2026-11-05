/* An ATL window thunk (atlthunk.dll): code that puts the thunk's argument
 * in place of the window handle and jumps to the window procedure, as ATL
 * windows (Process Explorer's) subclass with. The thunk is guest code the
 * DLL writes, the guest's own: with none, AtlThunk_AllocateData failed and
 * the program ended.
 *
 * Prints "atlthunk ok" and exits 0. */
#include <windows.h>
#include <stdio.h>

typedef struct AtlThunkData_t AtlThunkData_t;

static LRESULT CALLBACK proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    return (LRESULT)hwnd + msg + wp + lp;
}

int main(void)
{
    HMODULE h = LoadLibraryA("atlthunk.dll");
    AtlThunkData_t *(WINAPI *alloc_data)(void);
    WNDPROC (WINAPI *data_to_code)(AtlThunkData_t *);
    void (WINAPI *init_data)(AtlThunkData_t *, void *, SIZE_T);
    void (WINAPI *free_data)(AtlThunkData_t *);
    AtlThunkData_t *d;
    WNDPROC code;
    LRESULT r;

    if (!h)
    {
        printf("no atlthunk: %lu\n", GetLastError());
        return 1;
    }
    alloc_data = (void *)GetProcAddress(h, "AtlThunk_AllocateData");
    data_to_code = (void *)GetProcAddress(h, "AtlThunk_DataToCode");
    init_data = (void *)GetProcAddress(h, "AtlThunk_InitData");
    free_data = (void *)GetProcAddress(h, "AtlThunk_FreeData");
    if (!(d = alloc_data()))
    {
        printf("AtlThunk_AllocateData failed\n");
        return 1;
    }
    init_data(d, proc, 0x1000);
    code = data_to_code(d);
    /* through user32, as a window procedure is called, and directly */
    r = CallWindowProcA(code, (HWND)0x10, 2, 3, 4);
    if (r != 0x1000 + 2 + 3 + 4 || code((HWND)0x10, 5, 0, 0) != 0x1005)
    {
        printf("thunk returned %Ix\n", (SIZE_T)r);
        return 1;
    }
    free_data(d);
    printf("atlthunk ok\n");
    return 0;
}
