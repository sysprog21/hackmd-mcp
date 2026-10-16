These DER files are disposable TLS fixture material, never production credentials.
The server key is intentionally public and must never secure a real service.
`root.der` is the test CA; `cert.der` is its server certificate for `image.test`
and `other.test`; `key.der` is the server's PKCS#8 key. Both certificates expire
in September 2126. The CA private key is not needed or stored here.

The fixture connects those two names to an ephemeral loopback port only in
`cfg(test)` code. It trusts only `root.der`, without changing the system trust
store, and keeps certificate/hostname verification enabled. Other redirect
hosts still pass through the normal public-address check.

To regenerate them, for a new name or after expiry, run this from this
directory; it mints a new CA, since the old CA key is gone, stops at the first
failure, and deletes the CA key and other temporaries however it exits
(checked against `cargo test --lib tls` and `image`):

```sh
(
set -eu
trap 'rm -f ca.key server.key server.csr server.ext' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -days 36500 \
    -subj '/CN=HackMD fixture CA' \
    -addext 'basicConstraints=critical,CA:TRUE' \
    -addext 'keyUsage=critical,keyCertSign,cRLSign' -outform der -out root.der
openssl req -newkey rsa:2048 -nodes -keyout server.key -subj '/CN=image.test' -out server.csr
printf '%s\n' 'basicConstraints=CA:FALSE' \
    'keyUsage=critical,digitalSignature,keyEncipherment' \
    'extendedKeyUsage=serverAuth' 'subjectAltName=DNS:image.test,DNS:other.test' > server.ext
openssl x509 -req -in server.csr -CA root.der -CAform der -CAkey ca.key -days 36500 \
    -extfile server.ext -outform der -out cert.der
openssl pkcs8 -topk8 -nocrypt -in server.key -outform der -out key.der
)
```
