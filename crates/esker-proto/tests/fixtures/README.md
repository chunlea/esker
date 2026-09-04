# Test fixtures for the RPC's TLS

Throwaway, generated for `tests/rpc_tls.rs` and nothing else. The three private keys are in the
repository on purpose, have never protected anything, and must never be used by a node that serves
real traffic. A secret scanner that flags them is working correctly; this file is the answer.

| File | What it is |
|---|---|
| `ca-cert.pem` | The cluster's CA. Both nodes trust this and only this. |
| `node-a-cert.pem` / `node-a-key.pem` | One node's identity. `serverAuth` **and** `clientAuth`, SANs `DNS:localhost` and `IP:127.0.0.1`. |
| `node-b-cert.pem` / `node-b-key.pem` | The other node's, same shape. Two of them because mTLS is about two ends that each present one. |
| `other-ca-cert.pem` | An unrelated CA. |
| `stranger-cert.pem` / `stranger-key.pem` | A node whose chain is perfectly valid and signed by that other CA — the peer mTLS exists to turn away. |

Two details are load-bearing and each cost a red test on an earlier surface:

* **`clientAuth` in the extended key usage.** A certificate with only `serverAuth` is refused as a
  *client* certificate, which is what mTLS makes every node present.
* **A leaf, not a CA.** `openssl req -x509` writes `basicConstraints=CA:TRUE`, and webpki rejects a
  CA used as an end entity (`CaUsedAsEndEntity`). The node certificates are signed by the CA with
  `CA:FALSE`.

Regenerate with OpenSSL 3, from this directory:

```
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\n' > /tmp/node.ext

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout /tmp/ca-key.pem \
  -out ca-cert.pem -days 7300 -noenc -subj "/CN=esker rpc test CA"
for who in node-a node-b; do
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout $who-key.pem \
    -out /tmp/$who.csr -noenc -subj "/CN=$who"
  openssl x509 -req -in /tmp/$who.csr -CA ca-cert.pem -CAkey /tmp/ca-key.pem \
    -CAcreateserial -out $who-cert.pem -days 7300 -extfile /tmp/node.ext
done

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout /tmp/other-key.pem \
  -out other-ca-cert.pem -days 7300 -noenc -subj "/CN=esker rpc unrelated CA"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -keyout stranger-key.pem \
  -out /tmp/stranger.csr -noenc -subj "/CN=stranger"
openssl x509 -req -in /tmp/stranger.csr -CA other-ca-cert.pem -CAkey /tmp/other-key.pem \
  -CAcreateserial -out stranger-cert.pem -days 7300 -extfile /tmp/node.ext
```

They run out in 2046. A checked-in certificate with a one-year life is a test that fails on a date
nobody chose.
