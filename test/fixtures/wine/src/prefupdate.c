/* prefupdate.exe: a full prefix update (wineboot -u: setupapi's DefaultInstall,
 * which loads and registers every builtin in one rundll32) registers the
 * DLLs loaded last too. The registrations of windowscodecs, wmadmod and
 * wmvdecod (wine.inf's RegisterDlls, among the last) are deleted first, then
 * wineboot -u runs, then each is checked again. Prints "ok ..." per check that passes, "prefupdate FAIL ..." per one
 * that does not, then "N failures".
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>

static int failures;
static void check(const char *what, int ok)
{
    if (ok) printf("ok %s\n", what);
    else { printf("prefupdate FAIL %s\n", what); failures++; }
    fflush(stdout);
}

static const struct { const char *dll; HKEY root; const WCHAR *key; } regs[] =
{
    { "windowscodecs", HKEY_CLASSES_ROOT, L"CLSID\\{cacaf262-9370-4615-a13b-9f5539da4c0a}\\InprocServer32" },
    { "wmadmod", HKEY_CLASSES_ROOT, L"CLSID\\{2eeb4adf-4578-4d10-bca7-bb955f56320a}\\InprocServer32" },
    { "wmvdecod", HKEY_CLASSES_ROOT, L"CLSID\\{82d353df-90bd-4382-8bc2-3f6192b76e34}\\InprocServer32" },
};

static int present(HKEY root, const WCHAR *key)
{
    HKEY k;
    if (RegOpenKeyExW(root, key, 0, KEY_READ, &k)) return 0;
    RegCloseKey(k);
    return 1;
}

int main(void)
{
    WCHAR cmd[] = L"C:\\windows\\system32\\wineboot.exe -u";
    STARTUPINFOW si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    DWORD code = 1;
    char what[128];
    unsigned i;

    for (i = 0; i < ARRAYSIZE(regs); i++)
    {
        /* the CLSID's key, InprocServer32's parent */
        WCHAR parent[256], *s;
        lstrcpynW(parent, regs[i].key, ARRAYSIZE(parent));
        if ((s = wcsrchr(parent, '\\'))) *s = 0;
        RegDeleteTreeW(regs[i].root, parent);
        RegDeleteKeyW(regs[i].root, parent);
        snprintf(what, sizeof(what), "%s unregistered", regs[i].dll);
        check(what, !present(regs[i].root, regs[i].key));
    }
    if (!CreateProcessW(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
    {
        check("wineboot -u started", 0);
    }
    else
    {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &code);
        check("wineboot -u exit 0", code == 0);
    }
    for (i = 0; i < ARRAYSIZE(regs); i++)
    {
        snprintf(what, sizeof(what), "%s registered", regs[i].dll);
        check(what, present(regs[i].root, regs[i].key));
    }
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
