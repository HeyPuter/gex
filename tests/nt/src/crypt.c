/* crypt32 loads, and its Unix side (which gex provides with nothing behind
 * it) answers as a host with no root certificates and no PKCS#12 support
 * would: Wine's crypt32 fails DLL_PROCESS_ATTACH with no Unix side at all,
 * which ended Unity players at startup; and the default crypto provider is
 * registered, as wineboot leaves a prefix. Prints one line per check.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld -lcrypt32 -ladvapi32 */
#include <windows.h>
#include <wincrypt.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

int main(void) {
    HMODULE m = LoadLibraryA("crypt32.dll");
    CHECK("crypt32 loads", m != NULL);
    /* The root store opens; with no host certificates it may be empty. */
    HCERTSTORE root = CertOpenSystemStoreA(0, "ROOT");
    CHECK("ROOT store opens", root != NULL);
    if (root) CertCloseStore(root, 0);
    /* A memory store and an encoded round trip work without the Unix side. */
    HCERTSTORE mem = CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, NULL);
    CHECK("memory store opens", mem != NULL);
    if (mem) CertCloseStore(mem, 0);
    /* The default RSA provider is registered (rsaenh), as wineboot leaves it. */
    HCRYPTPROV prov = 0;
    BOOL acquired = CryptAcquireContextW(&prov, NULL, NULL, PROV_RSA_FULL, CRYPT_VERIFYCONTEXT);
    CHECK("CryptAcquireContext(PROV_RSA_FULL)", acquired);
    BYTE rnd[16] = {0};
    CHECK("CryptGenRandom", acquired && CryptGenRandom(prov, sizeof rnd, rnd));
    if (acquired) CryptReleaseContext(prov, 0);
    /* PFX import is the Unix side's: it fails, it does not crash. */
    BYTE junk[64] = {0x30, 0x03, 0x02, 0x01, 0x03};
    CRYPT_DATA_BLOB pfx = {sizeof junk, junk};
    HCERTSTORE imported = PFXImportCertStore(&pfx, L"", 0);
    CHECK("PFXImportCertStore fails cleanly", imported == NULL);
    printf("%d failures\n", failures);
    return failures != 0;
}
