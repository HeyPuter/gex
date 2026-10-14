/*
 * taskapp.exe: a plain application window for the taskbar leg. Its title,
 * colour and place come from its own file name (taskone.exe, tasktwo.exe:
 * two programs, so Windows 7's taskbar, which groups a program's windows on
 * one button, shows a button for each). It appends what happens to it to
 * C:\taskapp.log ("NAME shown", "NAME activated MS" with GetTickCount at
 * the time) for the leg to read.
 *
 *   i686-w64-mingw32-gcc -O2 -o taskone.exe taskapp.c -mwindows
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static char name[MAX_PATH];

static void note( const char *what )
{
    FILE *f = fopen( "C:\\taskapp.log", "a" );
    if (!f) return;
    fprintf( f, "%s %s %lu\n", name, what, GetTickCount() );
    fclose( f );
}

static LRESULT CALLBACK proc( HWND hwnd, UINT msg, WPARAM wp, LPARAM lp )
{
    switch (msg)
    {
    case WM_SHOWWINDOW:
        if (wp) note( "shown" );
        break;
    case WM_ACTIVATE:
        if (LOWORD(wp) != WA_INACTIVE) note( "activated" );
        break;
    case WM_DESTROY:
        PostQuitMessage( 0 );
        return 0;
    }
    return DefWindowProcA( hwnd, msg, wp, lp );
}

int WINAPI WinMain( HINSTANCE inst, HINSTANCE prev, LPSTR cmd, int show )
{
    char path[MAX_PATH], *p;
    int two;
    WNDCLASSA wc = {0};
    HWND hwnd;
    MSG msg;

    GetModuleFileNameA( NULL, path, sizeof(path) );
    p = strrchr( path, '\\' );
    strcpy( name, p ? p + 1 : path );
    if ((p = strrchr( name, '.' ))) *p = 0;
    two = !_stricmp( name, "tasktwo" );

    wc.lpfnWndProc = proc;
    wc.hInstance = inst;
    wc.hIcon = LoadIconA( NULL, two ? (LPCSTR)IDI_INFORMATION : (LPCSTR)IDI_WARNING );
    wc.hCursor = LoadCursorA( NULL, (LPCSTR)IDC_ARROW );
    wc.hbrBackground = CreateSolidBrush( two ? RGB(40, 160, 60) : RGB(200, 60, 40) );
    wc.lpszClassName = "taskapp";
    RegisterClassA( &wc );
    hwnd = CreateWindowA( "taskapp", two ? "Task Two" : "Task One", WS_OVERLAPPEDWINDOW,
                          two ? 420 : 120, two ? 300 : 120, 320, 220, NULL, NULL, inst, NULL );
    ShowWindow( hwnd, SW_SHOWNORMAL );
    UpdateWindow( hwnd );
    while (GetMessageA( &msg, NULL, 0, 0 ))
    {
        TranslateMessage( &msg );
        DispatchMessageA( &msg );
    }
    return 0;
}
