/* crypt32: a signed message whose authenticated attributes are not in DER
 * order verifies (the signature covers the attributes as encoded, which
 * Authenticode signers need not sort). Wine's re-encoding of the
 * attributes failed such a check with TRUST_E_CERT_SIGNATURE. Vectors:
 * authattrs-gen.py. */
#include <windows.h>
#include <wincrypt.h>
#include <stdio.h>
#include <string.h>
#include "authattrs.h"

static int failures;
static void ok(int cond, const char *what)
{
    if (cond) printf("ok %s\n", what);
    else { printf("FAIL %s (error %08lx)\n", what, GetLastError()); failures++; }
}

static BOOL verify(const BYTE *data, DWORD size)
{
    HCRYPTMSG msg = CryptMsgOpenToDecode(X509_ASN_ENCODING | PKCS_7_ASN_ENCODING, 0, 0, 0, NULL, NULL);
    HCERTSTORE store;
    PCCERT_CONTEXT cert;
    CERT_INFO *info;
    DWORD n = 0;
    BOOL ret = FALSE;

    if (!msg || !CryptMsgUpdate(msg, data, size, TRUE)) goto done;
    if (!CryptMsgGetParam(msg, CMSG_SIGNER_CERT_INFO_PARAM, 0, NULL, &n)) goto done;
    info = malloc(n);
    CryptMsgGetParam(msg, CMSG_SIGNER_CERT_INFO_PARAM, 0, info, &n);
    store = CertOpenStore(CERT_STORE_PROV_MSG, X509_ASN_ENCODING | PKCS_7_ASN_ENCODING, 0, 0, msg);
    cert = CertGetSubjectCertificateFromStore(store, X509_ASN_ENCODING, info);
    if (cert)
    {
        ret = CryptMsgControl(msg, 0, CMSG_CTRL_VERIFY_SIGNATURE, cert->pCertInfo);
        CertFreeCertificateContext(cert);
    }
    CertCloseStore(store, 0);
    free(info);
done:
    if (msg) CryptMsgClose(msg);
    return ret;
}

int main(void)
{
    HCRYPTMSG msg = CryptMsgOpenToDecode(X509_ASN_ENCODING | PKCS_7_ASN_ENCODING, 0, 0, 0, NULL, NULL);
    CRYPT_ATTRIBUTES *attrs;
    BYTE *enc, copy[sizeof(signed_msg)];
    DWORD n = 0, encsize;

    /* The vector exercises the case: a DER re-encoding of its attributes is
     * not what was signed. */
    CryptMsgUpdate(msg, signed_msg, sizeof(signed_msg), TRUE);
    CryptMsgGetParam(msg, CMSG_SIGNER_AUTH_ATTR_PARAM, 0, NULL, &n);
    attrs = malloc(n);
    CryptMsgGetParam(msg, CMSG_SIGNER_AUTH_ATTR_PARAM, 0, attrs, &n);
    ok(CryptEncodeObjectEx(X509_ASN_ENCODING, PKCS_ATTRIBUTES, attrs, CRYPT_ENCODE_ALLOC_FLAG, NULL, &enc, &encsize) &&
       encsize == sizeof(raw_attrs) && memcmp(enc, raw_attrs, encsize), "attributes re-encode in another order");
    LocalFree(enc);
    free(attrs);
    CryptMsgClose(msg);

    ok(verify(signed_msg, sizeof(signed_msg)), "signature over the attributes as encoded verifies");
    /* and a changed content still fails (its messageDigest no longer matches
     * after the hash, or the signature check does): flip a byte of the
     * signature itself, the last bytes of the message */
    memcpy(copy, signed_msg, sizeof(copy));
    copy[sizeof(copy) - 1] ^= 1;
    ok(!verify(copy, sizeof(copy)), "a changed signature fails");
    printf("%d failures\n", failures);
    return failures != 0;
}
