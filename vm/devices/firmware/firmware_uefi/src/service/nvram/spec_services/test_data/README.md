# PKCS#7 auth-var test fixtures

Fixtures consumed by unit tests in `auth_var_crypto.rs` to exercise the PKCS#7
signed-data verification path against both the OpenSSL (unix) and WinCrypt
(windows) backends.

`auth_var_cert.der` is a self-signed RSA-2048 X.509 certificate (DER).
`auth_var_pkcs7.der` is a detached PKCS#7 signature (DER) covering the byte
concatenation:

```
name("TestVar" as UCS-2 LE, no NUL) || vendor(16 zero bytes) || attr(0x27 LE u32) || timestamp(16 zero bytes) || var_data(b"hello uefi")
```

## Regeneration

```sh
# 1. Key + self-signed cert
openssl genrsa -out key.pem 2048
openssl req -new -x509 -key key.pem -out cert.pem -days 3650 \
  -subj "/C=US/ST=WA/L=Redmond/O=OpenVMM Test/CN=openvmm-test"
openssl x509 -in cert.pem -outform DER -out auth_var_cert.der

# 2. Build the verify_buf exactly as `authenticate_variable` does
python3 -c "
import struct
name = 'TestVar'.encode('utf-16-le')
vendor = b'\x00' * 16
attr = struct.pack('<I', 0x27)
timestamp = b'\x00' * 16
var_data = b'hello uefi'
open('verify_buf.bin', 'wb').write(name + vendor + attr + timestamp + var_data)
"

# 3. Sign (detached, signer cert embedded, no signed attributes)
openssl cms -sign -binary -in verify_buf.bin -signer cert.pem -inkey key.pem \
  -outform DER -noattr -out auth_var_pkcs7.der
```
