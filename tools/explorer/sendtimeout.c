/* A message sent to another process with SendMessageTimeout reaches its
 * receiver even when the sender's timeout ran out first: the timeout only
 * stops the sender waiting (Windows; ReactOS's co_MsqSendMessage keeps the
 * message queued and only detaches its result). This is how a busy program
 * still hears of a WM_THEMECHANGED or WM_SETTINGCHANGE broadcast.
 *
 * sendtimeout.exe           make a window, start "sendtimeout.exe send HWND",
 *                           and do not pump until it has exited; then pump
 *                           and say whether the message arrived.
 * sendtimeout.exe send HWND send WM_APP+1 with a 300 ms timeout.
 * Prints "sendtimeout: PASS" or "sendtimeout: FAIL ..." on stdout. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static int got;
static WPARAM got_wp;

static LRESULT CALLBACK proc( HWND hwnd, UINT msg, WPARAM wp, LPARAM lp )
{
    if (msg == WM_APP + 1) { got++; got_wp = wp; return 42; }
    return DefWindowProcW( hwnd, msg, wp, lp );
}

int main( int argc, char **argv )
{
    WNDCLASSW cls = { 0 };
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    char cmd[MAX_PATH + 64], self[MAX_PATH];
    DWORD start, code = 0;
    DWORD_PTR res = 0;
    HWND hwnd;
    MSG msg;

    if (argc == 3 && !strcmp( argv[1], "send" ))
    {
        LRESULT r = SendMessageTimeoutW( (HWND)(ULONG_PTR)strtoul( argv[2], NULL, 0 ), WM_APP + 1, 7, 9,
                                         SMTO_NORMAL, 300, &res );
        return r ? 2 : (GetLastError() == ERROR_TIMEOUT ? 0 : 3);
    }
    cls.lpfnWndProc = proc;
    cls.lpszClassName = L"sendtimeout";
    RegisterClassW( &cls );
    hwnd = CreateWindowW( L"sendtimeout", L"", 0, 0, 0, 0, 0, HWND_MESSAGE, 0, 0, 0 );
    GetModuleFileNameA( 0, self, sizeof(self) );
    snprintf( cmd, sizeof(cmd), "\"%s\" send %#lx", self, (unsigned long)(ULONG_PTR)hwnd );
    if (!CreateProcessA( NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi ))
    {
        printf( "sendtimeout: FAIL could not start the sender (%lu)\n", GetLastError() );
        return 1;
    }
    /* busy: no messages until the sender has given up */
    WaitForSingleObject( pi.hProcess, INFINITE );
    GetExitCodeProcess( pi.hProcess, &code );
    if (code)
    {
        printf( "sendtimeout: FAIL the sender did not time out (exit %lu)\n", code );
        return 1;
    }
    start = GetTickCount();
    while (GetTickCount() - start < 2000 && !got)
    {
        while (PeekMessageW( &msg, 0, 0, 0, PM_REMOVE )) DispatchMessageW( &msg );
        Sleep( 20 );
    }
    if (got == 1 && got_wp == 7) printf( "sendtimeout: PASS\n" );
    else printf( "sendtimeout: FAIL the receiver got the message %d times after the sender's timeout\n", got );
    fflush( stdout );
    return got == 1 ? 0 : 1;
}
