/* test/wow64/certupd: I_CertUpdateStore over two memory stores holding the
 * same self-signed certificate: the context it finds in both it frees
 * through the certificate interface's freeContext. Prints "certupd ok". */
#include <windows.h>
#include <wincrypt.h>
#include <stdio.h>

typedef BOOL (WINAPI *update_fn)(HCERTSTORE, HCERTSTORE, DWORD, DWORD);

int main(void)
{
    BYTE name[256];
    CERT_NAME_BLOB subject = { sizeof(name), name };
    HCERTSTORE s1, s2;
    PCCERT_CONTEXT cert;
    update_fn update = (update_fn)GetProcAddress(LoadLibraryA("crypt32.dll"), "I_CertUpdateStore");

    if (!CertStrToNameA(X509_ASN_ENCODING, "CN=gex", CERT_X500_NAME_STR, NULL, name, &subject.cbData, NULL)) {
        printf("certupd: CertStrToName failed (%lu)\n", GetLastError());
        return 1;
    }
    if (!(cert = CertCreateSelfSignCertificate(0, &subject, 0, NULL, NULL, NULL, NULL, NULL))) {
        printf("certupd: CertCreateSelfSignCertificate failed (%#lx)\n", GetLastError());
        return 1;
    }
    s1 = CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, NULL);
    s2 = CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, NULL);
    if (!update || !s1 || !s2 || !CertAddCertificateContextToStore(s1, cert, CERT_STORE_ADD_ALWAYS, NULL)
        || !CertAddCertificateContextToStore(s2, cert, CERT_STORE_ADD_ALWAYS, NULL)) {
        printf("certupd: setup failed (%lu)\n", GetLastError());
        return 1;
    }
    if (!update(s1, s2, 0, 0)) {
        printf("certupd: I_CertUpdateStore failed (%lu)\n", GetLastError());
        return 1;
    }
    printf("certupd ok\n");
    return 0;
}
