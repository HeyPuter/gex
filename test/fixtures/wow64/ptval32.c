/* ptval32.exe: a native wasm32 program (a Wine builtin, linked into
 * winepe.wasm) calling i386 DLLs' exports that take a struct by value,
 * which i386 passes on the stack as its bytes, wasm32 by a pointer to it:
 * oleaut32's CY functions (8 bytes, two of them in VarCyAdd), shlwapi's
 * AssocCreate (a CLSID, 16 bytes) and comctl32's LBItemFromPt (a POINT);
 * directly, through the guest-import stubs, and through pointers
 * GetProcAddress gave. Prints "ok ..." per check that passes, "ptval32
 * FAIL ..." per one that does not, then "N failures". */
#include <windows.h>
#include <oleauto.h>
#include <shlwapi.h>
#include <commctrl.h>
#include <stdio.h>

static int failures;
static void check(const char *what, int ok)
{
    if (ok) printf("ok %s\n", what);
    else { printf("ptval32 FAIL %s\n", what); failures++; }
    fflush(stdout);
}

static const GUID clsid_qa = { 0xa07034fd, 0x6caa, 0x4954, { 0xac, 0x3f, 0x97, 0xa2, 0x72, 0x16, 0xf9, 0x8a } };
static const GUID iid_iqa = { 0xc46ca590, 0x3c3f, 0x11d2, { 0xbe, 0xe6, 0x00, 0x00, 0xf8, 0x05, 0xca, 0x57 } };

int main(int argc, char **argv)
{
    CY a, b, out;
    double d = 0;
    IUnknown *qa = NULL;
    HWND lb;
    POINT in = { 10, 5 }, away = { 5000, 5000 };
    HMODULE oa = LoadLibraryA("oleaut32.dll"), sh = LoadLibraryA("shlwapi.dll"), cc = LoadLibraryA("comctl32.dll");
    HRESULT (WINAPI *cyadd)(CY, CY, CY *) = (void *)GetProcAddress(oa, "VarCyAdd");
    HRESULT (WINAPI *assoc)(CLSID, REFIID, void **) = (void *)GetProcAddress(sh, "AssocCreate");
    INT (WINAPI *lbpt)(HWND, POINT, BOOL) = (void *)GetProcAddress(cc, (const char *)14);

    a.int64 = 150000;  /* 15.0000 */
    b.int64 = -25000;  /* -2.5000 */
    out.int64 = 0;
    check("VarCyAdd", VarCyAdd(a, b, &out) == S_OK && out.int64 == 125000);
    out.int64 = 0;
    check("VarCyMulI4", VarCyMulI4(a, 3, &out) == S_OK && out.int64 == 450000);
    check("VarR8FromCy", VarR8FromCy(b, &d) == S_OK && d == -2.5);
    check("VarCyCmp", VarCyCmp(b, a) == VARCMP_LT && VarCyCmp(a, b) == VARCMP_GT);
    check("AssocCreate", AssocCreate(clsid_qa, &iid_iqa, (void **)&qa) == S_OK && qa);
    if (qa) qa->lpVtbl->Release(qa), qa = NULL;
    check("AssocCreate other class", AssocCreate(iid_iqa, &iid_iqa, (void **)&qa) != S_OK);

    lb = CreateWindowA("LISTBOX", "", WS_POPUP | WS_VISIBLE | LBS_NOINTEGRALHEIGHT, 0, 0, 200, 200, NULL, NULL, NULL, NULL);
    SendMessageA(lb, LB_ADDSTRING, 0, (LPARAM)"one");
    SendMessageA(lb, LB_ADDSTRING, 0, (LPARAM)"two");
    ClientToScreen(lb, &in);
    check("LBItemFromPt item", LBItemFromPt(lb, in, FALSE) == 0);
    check("LBItemFromPt outside", LBItemFromPt(lb, away, FALSE) == -1);

    out.int64 = 0;
    check("VarCyAdd pointer", cyadd && cyadd(a, b, &out) == S_OK && out.int64 == 125000);
    check("AssocCreate pointer", assoc && assoc(clsid_qa, &iid_iqa, (void **)&qa) == S_OK && qa);
    if (qa) qa->lpVtbl->Release(qa);
    check("LBItemFromPt pointer", lbpt && lbpt(lb, in, FALSE) == 0 && lbpt(lb, away, FALSE) == -1);
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
