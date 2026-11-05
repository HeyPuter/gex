/* The root certificate store on 32-bit Wine (test/wow64/rootstore):
 * crypt32 fills ROOT from the host's bundle, or, on a machine with none (a
 * page, whose filesystem is the dist and the prefix), from the bundle the
 * dist ships at DIST/etc/ssl/certs. Prints "rootstore ok N" with N roots, or
 * "rootstore FAIL ..." when the store is empty. */
#include <windows.h>
#include <wincrypt.h>
#include <stdio.h>

int main(void)
{
    HCERTSTORE store = CertOpenSystemStoreA(0, "ROOT");
    PCCERT_CONTEXT c = NULL;
    int n = 0;

    if (!store) { printf("rootstore FAIL CertOpenSystemStore %lu\n", GetLastError()); return 1; }
    while ((c = CertEnumCertificatesInStore(store, c))) n++;
    CertCloseStore(store, 0);
    if (n < 10) { printf("rootstore FAIL %d roots\n", n); return 1; }
    printf("rootstore ok %d\n", n);
    return 0;
}
