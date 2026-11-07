/* The service manager in a session that starts no services
 * (WINEGEX_NO_WINEBOOT=1, the page's boot): the first OpenSCManager starts
 * services.exe and succeeds, as installers that register a service need
 * (it failed with RPC_S_SERVER_UNAVAILABLE). A second open finds it running,
 * and it serves: the services list holds the prefix's PlugPlay.
 * gex-nt-arch: x64
 * gex-nt-libs: -ladvapi32
 */
#include <windows.h>
#include <stdio.h>

static int failures;

static void ok(int cond, const char *what)
{
    printf("%s scmstart %s\n", cond ? "ok  " : "FAIL", what);
    if (!cond) failures++;
}

int main(void)
{
    SC_HANDLE scm, again, svc;
    DWORD err;

    scm = OpenSCManagerW(NULL, NULL, SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE);
    err = scm ? 0 : GetLastError();
    if (!scm) printf("OpenSCManagerW: error %lu\n", err);
    ok(scm != NULL, "the first OpenSCManager succeeds");
    again = OpenSCManagerW(NULL, NULL, SC_MANAGER_CONNECT);
    ok(again != NULL, "a second OpenSCManager succeeds");
    if (scm)
    {
        svc = OpenServiceW(scm, L"PlugPlay", SERVICE_QUERY_CONFIG);
        ok(svc != NULL, "the manager serves: PlugPlay opens");
        if (svc) CloseServiceHandle(svc);
        CloseServiceHandle(scm);
    }
    if (again) CloseServiceHandle(again);
    printf("%d failures\n", failures);
    return failures;
}
