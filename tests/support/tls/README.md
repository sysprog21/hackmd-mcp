These DER files are disposable TLS fixture material, never production credentials.
The server key is intentionally public and must never secure a real service.
`root.der` is the test CA; `cert.der` is its server certificate for `image.test`
and `other.test`; `key.der` is the server's PKCS#8 key. Both certificates expire
in September 2126. The CA private key is not needed or stored here.

The fixture connects those two names to an ephemeral loopback port only in
`cfg(test)` code. It trusts only `root.der`, without changing the system trust
store, and keeps certificate/hostname verification enabled. Other redirect
hosts still pass through the normal public-address check.
