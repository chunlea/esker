# Test fixtures for the PostgreSQL port's TLS

`localhost-test-cert.pem` and `localhost-test-key.pem` are a **throwaway self-signed certificate
and private key, generated for these tests and for nothing else.** The key is in the repository on
purpose, it has never protected anything, and it must never be used by any node that serves real
traffic. A secret scanner that flags it is working correctly; this file is the answer.

They exist so `tests/pgwire_tls.rs` can run a real `rustls` handshake against a real listener
without needing anything installed on the machine — the same reason the protocol tests use captured
bytes rather than a live PostgreSQL.

Regenerate with (OpenSSL 3):

```
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout localhost-test-key.pem -out localhost-test-cert.pem \
  -days 7300 -noenc -subj "/CN=esker-sql test node" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

Three details are load-bearing:

* **A subject alternative name.** `rustls` verifies the SAN and ignores the common name entirely, so
  a certificate with only a `CN` fails with a name-mismatch error that reads like a bug in the
  server.
* **P-256, PKCS#8.** `rustls-graviola` signs with ECDSA P-256/P-384 or Ed25519; the key label
  `PRIVATE KEY` is the PKCS#8 shape `pgwire::tls` maps to `PrivateKeyDer::Pkcs8`.
* **A long expiry.** It runs out in 2046. A checked-in certificate with a one-year life is a test
  that fails on a date nobody chose.
