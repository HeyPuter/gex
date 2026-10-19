#!/usr/bin/env python3
# Makes authattrs.h for authattrs.c: a PKCS#7 signed message whose authenticated
# attributes are not in DER order (messageDigest before contentType), signed by a
# throwaway self-signed key, as some real Authenticode signers emit them.
#   openssl req -x509 -newkey rsa:2048 -nodes -keyout k.pem -out c.pem -subj "/CN=gex authattrs test" -sha256
#   openssl x509 -in c.pem -outform DER -out c.der
#   authattrs-gen.py k.pem c.der > authattrs.h
import sys, hashlib, subprocess, tempfile, os
key, certf = sys.argv[1:3]
cert = open(certf, 'rb').read()
def L(n): return bytes([n]) if n < 0x80 else (lambda b: bytes([0x80 | len(b)]) + b)(n.to_bytes((n.bit_length() + 7) // 8, 'big'))
def T(tag, body): return bytes([tag]) + L(len(body)) + body
def tlv(b, p):
    l = b[p + 1]; h = 2
    if l & 0x80: n = l & 0x7f; l = int.from_bytes(b[p + 2:p + 2 + n], 'big'); h += n
    return p + h, p + h + l
def kids(b, s, e):
    out = []
    while s < e:
        cs, ce = tlv(b, s); out.append((s, cs, ce)); s = ce
    return out
SEQ, SET, OCT, OID, INT = 0x30, 0x31, 0x04, 0x06, 0x02
oid = lambda hexs: T(OID, bytes.fromhex(hexs))
data_oid, sd_oid = oid('2a864886f70d010701'), oid('2a864886f70d010702')
sha256 = T(SEQ, oid('608648016503040201') + b'\x05\x00')
rsa = T(SEQ, oid('2a864886f70d010101') + b'\x05\x00')
# issuer and serial from the certificate's tbsCertificate
cs, ce = tlv(cert, 0); tbs = kids(cert, cs, ce)[0]; f = kids(cert, tbs[1], tbs[2])
if cert[f[0][0]] == 0xa0: f = f[1:]
serial, issuer = cert[f[0][0]:f[0][2]], cert[f[2][0]:f[2][2]]
content = b'gex: authenticated attributes are signed as encoded'
md = T(SEQ, oid('2a864886f70d010904') + T(SET, T(OCT, hashlib.sha256(content).digest())))
ct = T(SEQ, oid('2a864886f70d010903') + T(SET, data_oid))
attrs = md + ct                      # DER would sort ct (30 18 ...) before md (30 2f ...)
with tempfile.TemporaryDirectory() as d:
    open(d + '/a', 'wb').write(T(SET, attrs))
    sig = subprocess.run(['openssl', 'dgst', '-sha256', '-sign', key, d + '/a'], capture_output=True, check=True).stdout
si = T(SEQ, T(INT, b'\x01') + T(SEQ, issuer + serial) + sha256 + T(0xa0, attrs) + rsa + T(OCT, sig))
sd = T(SEQ, T(INT, b'\x01') + T(SET, sha256) + T(SEQ, data_oid + T(0xa0, T(OCT, content))) + T(0xa0, cert) + T(SET, si))
msg = T(SEQ, sd_oid + T(0xa0, sd))
arr = lambda n, b: 'static const BYTE %s[%d] = {\n%s\n};\n' % (n, len(b), '\n'.join(
    '    ' + ','.join('0x%02x' % x for x in b[i:i + 16]) + ',' for i in range(0, len(b), 16)))
print('/* authattrs-gen.py output: a signed message, its authenticated attributes as encoded. */')
print(arr('signed_msg', msg) + arr('raw_attrs', T(SET, attrs)))
