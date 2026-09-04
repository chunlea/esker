# Test fixtures for the S3 client's HTTPS

Five files, all **throwaway**, all generated for `tests/https.rs` and for nothing else. The two
private keys are in the repository on purpose, have never protected anything, and must never be
used by anything that serves real traffic. A secret scanner that flags them is working correctly;
this file is the answer.

| File | What it is |
|---|---|
| `ca-cert.pem` | The test CA. The client trusts this and only this. |
| `localhost-cert.pem` / `localhost-key.pem` | The good server leaf: `CA:FALSE`, `serverAuth`, SANs `DNS:localhost` and `IP:127.0.0.1`. |
| `wrong-name-cert.pem` | A leaf signed by the same CA whose only SAN is `DNS:not-the-endpoint.invalid`. The chain is valid and the *name* is wrong, which is the case a client that skipped name verification sails through. |
| `wrong-name-key.pem` | Its key. |
| `other-ca-cert.pem` | An unrelated CA that signed none of the above, so a client told to trust it must refuse the server. |

## Why these are not `esker-sql`'s

`crates/esker-sql/tests/fixtures/` holds an equivalent pair for the PostgreSQL port. Two sets
rather than one because a test reaching into another crate's fixture directory by relative path
breaks the day that crate reorganises, and because these tests need two more certificates that the
other surface has no use for. A shared home for both — with the PEM reader that also exists twice —
belongs in `esker-base`, and is written down as owed in ADR 0055 rather than done from a lane that
does not own that crate.

## Regenerating

OpenSSL 3, from this directory:

```
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > /tmp/good.ext
printf 'subjectAltName=DNS:not-the-endpoint.invalid\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > /tmp/wrong.ext

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout /tmp/ca-key.pem \
  -out ca-cert.pem -days 7300 -noenc -subj "/CN=esker-s3 test CA"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout localhost-key.pem \
  -out /tmp/good.csr -noenc -subj "/CN=localhost"
openssl x509 -req -in /tmp/good.csr -CA ca-cert.pem -CAkey /tmp/ca-key.pem -CAcreateserial \
  -out localhost-cert.pem -days 7300 -extfile /tmp/good.ext
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout wrong-name-key.pem \
  -out /tmp/wrong.csr -noenc -subj "/CN=not-the-endpoint.invalid"
openssl x509 -req -in /tmp/wrong.csr -CA ca-cert.pem -CAkey /tmp/ca-key.pem -CAcreateserial \
  -out wrong-name-cert.pem -days 7300 -extfile /tmp/wrong.ext
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout /tmp/other-key.pem \
  -out other-ca-cert.pem -days 7300 -noenc -subj "/CN=esker-s3 unrelated CA"
```

Three details are load-bearing, and each one cost a red test the first time it was got wrong on the
PostgreSQL surface:

* **A subject alternative name.** `rustls` verifies the SAN and ignores the common name entirely.
* **A leaf, not a CA.** `openssl req -x509` writes `basicConstraints=CA:TRUE`, and webpki rejects a
  CA used as an end entity (`CaUsedAsEndEntity`) — correctly. The leaves above are signed by the CA
  with `CA:FALSE`.
* **A long expiry.** These run out in 2046. A checked-in certificate with a one-year life is a test
  that fails on a date nobody chose.
