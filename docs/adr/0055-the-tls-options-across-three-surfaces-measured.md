# 0055 — The TLS options across three surfaces, measured

Status: **accepted 2026-09-04, and built for the PostgreSQL port.** The maintainer took option 4
and named `rustls-graviola` as the exception: refusal discipline everywhere now, `rustls` +
`rustls-graviola` behind `esker-sql`'s `tls` feature, off by default, PG port first. What that
turned into is at the end of this file, under "What was built, and what is still owed"; the option
list below is unchanged from the measurement that produced it.

[ADR 0025](0025-s3-transport-and-tls.md)
settled S3's transport for phase 6b (plain HTTP now, `https://` refused at parse time) and wrote down
four things that have to be true before TLS lands there. This ADR does the first of those four —
measure the dependency cost of a pure-Rust `rustls` provider against `deny.toml` — and widens the
question to the other two surfaces that need the same answer, because it turns out to be **one
dependency decision, not three**: whichever crate is added to reach S3 over TLS is already sitting in
the workspace graph for the PostgreSQL port and the RPC port to use too.

See [ADR 0003](0003-dependencies.md) (the allowlist and the budget), `docs/DESIGN.md` §9 (RPC framing)
and §13 (the TLS paragraph this ADR replaces), `deny.toml`, `crates/esker-cli/tests/dep_budget.rs`.

## Context: three surfaces, three current answers

**(a) The PostgreSQL wire protocol.** A connecting client sends `SSLRequest` before anything else if
it wants TLS; Esker answers a single `N` and the client either continues in the clear or gives up
(`crates/esker-sql/src/pgwire/server.rs:262-267`, `pgwire/message.rs:28-29,58-59`). `Auth::Trust`, the
default `Auth` variant, is documented as "the only sane behind a trusted network boundary"
(`pgwire/server.rs:29-30`) — the current security model is stated in those words, not implied.

**(b) The hand-rolled RPC** between client, store and PD (`DESIGN.md` §9, `esker-proto`). One TCP
connection, multiplexed frames, `tokio` on both ends. `TransportConfig` has seven knobs — frame size,
in-flight limit, write queue, keepalive, idle timeout, request timeout, shutdown grace — and none of
them is about encryption or identity, because there is no TLS or peer-authentication code anywhere in
this path today. This matters for `RaftTransport` and `Pd` traffic specifically: a store's
`StoreHeartbeat` and a peer's `RaftTransport::Batch` are accepted from whoever can open the socket.
Grepping `esker-pd`'s bootstrap and heartbeat handling for anything resembling a certificate or token
check turns up nothing — the only hits on "auth" and "cert" are the words "**auth**oritative" and
"**cert**ainly" in doc comments. This is not a gap this ADR closes (each option below says what it
does and does not buy against it); it is a reason to be precise about what each option actually buys.

**(c) The S3 tier's HTTPS.** Settled by ADR 0025: `esker_s3::Transport` is a trait with one blocking
`std::net` implementor, `TcpTransport`; `Endpoint::parse` refuses `https://` rather than accepting it
and speaking plaintext. Its decision 4 lists what has to be true before that changes; item 1 is "a
dependency count for `rustls` + a pure-Rust provider, measured with `cargo tree`."

All three converge on the same question: is there a `CryptoProvider` for `rustls` that is actually
pure Rust once measured, not just described that way, and what does it cost. That is what the rest of
this ADR answers, with a scratch Cargo project outside the repo (`cargo tree -e normal`, then
`cargo deny check` with this repo's own `deny.toml` copied beside it — the CI-equivalent check, per
the dep budget test's own method of walking `cargo metadata`'s resolve graph by name).

## The options

### Option 1 — terminate TLS outside the process

Zero crates, zero lines, zero change to `deny.toml`. But "terminate outside the process" is not the
same shape on all three surfaces:

- **S3** already works this way (ADR 0025): plain HTTP to MinIO, or a TLS-terminating sidecar on
  localhost for real S3.
- **RPC** works the same way cleanly: `esker-proto`'s framing has no protocol-level handshake that a
  terminator has to understand, so a plain TCP-level sidecar (a service-mesh proxy, `stunnel`, an
  envoy in TCP-passthrough mode) in front of every store and PD process is enough to get encryption on
  the wire between sidecars.
- **The PostgreSQL port does not.** `SSLRequest` is an in-protocol upgrade: the client's first eight
  bytes ask a question, and only after `S` or `N` does either the plaintext or the TLS stream begin.
  A generic SNI-sniffing TCP proxy sees those eight bytes and does not know what to do with them —
  it needs to *speak enough PG wire protocol to answer the question itself*, which is exactly what
  connection poolers like PgBouncer do already. That is a real, well-worn pattern (it is how most
  managed Postgres-compatible services terminate client TLS today), but it is a PG-aware terminator,
  not an arbitrary load balancer, and the deployment note has to say so rather than imply any TCP LB
  will do.

**Where it leaks, for a serverless deployment specifically.** An edge terminator protects the
client-facing hop. It says nothing about store-to-store `RaftTransport` traffic or PD's own
`StoreHeartbeat`/`RegionHeartbeat` path, which are internal RPC calls on the same wire as everything
else in (b). Protecting those needs a sidecar *at every internal hop too* — which is the same idea
repeated at every node, and at that point the "zero crates" framing is doing less work than it looks:
the crates have not left the deployment, they have moved into the mesh's own process, outside Esker's
dependency budget but not outside the system's dependency graph or its audit surface. And even a fully
mesh-encrypted deployment gives you an authenticated *transport*, not an authorization *decision*:
nothing in `esker-pd` today maps a verified peer identity to "may register as a store" or "may vote in
region R"'s Raft group. That gap exists under every option in this ADR; mTLS makes the identity
available to check, it does not write the check.

**Work to adopt:** none in Esker. It is a deployment note (already half-written by ADR 0025 for S3),
extended to say: PD-and-store traffic needs a mesh or a private network trusted for the deployment's
threat model, and the PG port needs a PG-protocol-aware terminator, not a generic one.

### Option 2 — `rustls` with a pure-Rust `CryptoProvider` (measured)

`rustls` itself, `default-features = false, features = ["std"]`: **7 crates** — `rustls`, `once_cell`,
`rustls-pki-types`, `rustls-webpki`, `subtle`, `untrusted`, `zeroize`. No `*-sys`, no `cc`, no build
script. `rustls-webpki` (certificate path validation) ships as part of `rustls`'s own dependency, not
a separate opt-in, and it resolves cleanly here *without* `ring` or `aws-lc-rs` in the graph — the
signature-verification algorithms it needs come from whatever `CryptoProvider` is installed at
runtime, not from a hardcoded backend. For contrast, `rustls` with its own default `aws_lc_rs`
feature (the thing `deny.toml` already bans) is **14 crates**, and `aws-lc-sys`, `jobserver` and
`pkg-config` are among them — measured, not assumed, and exactly the shape the ban exists for.

**The pure-Rust providers that exist**, per `rustls`'s own third-party-provider list:

| Provider | Backend | Pure Rust? |
|---|---|---|
| `rustls-rustcrypto` | RustCrypto primitives | Yes, measured below |
| `rustls-graviola` | `graviola` (new, by `rustls`'s original author) | Yes, measured below |
| `oxiquic-crypto` | "OxiCrypto," framed around QUIC | Unmeasured — 418 downloads total, no track record |
| `rustls-symcrypt` | Microsoft SymCrypt | **No** — depends on `symcrypt` → `symcrypt-sys`, a C binding |
| `rustls-mbedtls-provider` / `rustls-mbedcrypto-provider` | Mbed TLS | No — C library |
| `rustls-openssl` | OpenSSL | No — already named in `deny.toml` |
| `boring-rustls-provider` | BoringSSL | No — C, and work-in-progress |
| `rustls-wolfcrypt-provider` | wolfCrypt | No — C, and work-in-progress |

Four of the eight named candidates are eliminated without writing a line of Rust, by reading what they
wrap — `rustls-symcrypt`'s own tagline calls itself "pure Rust," which is the traps section's warning
made concrete: a crate one level removed from the C library can still describe itself that way.

**`rustls-rustcrypto`, measured.** `rustls` + `rustls-rustcrypto` (`default-features = false,
features = ["std", "tls12"]`, matching its own defaults) resolves to **72 crates**
(`cargo tree -e normal`, counted the way `dep_budget.rs` counts — unique names, workspace members
excluded). `cargo deny check` against this repo's own `deny.toml`, copied beside the scratch project,
**fails**, today, on six independent grounds:

- `error[banned]`: `rand v0.8.8` (via `num-bigint-dig` via `rsa`) — on the ban list by name.
- `error[duplicate]`: `rustls-webpki` resolves to **two versions** — `0.102.8`, pinned by
  `rustls-rustcrypto`'s own (stale) requirement, and `0.103.15`, wanted by `rustls` itself. Trips
  `[bans] multiple-versions = "deny"`.
- `error[unmaintained]`: `paste` — RUSTSEC-2024-0436, archived by its author, "no safe upgrade
  available."
- `error[vulnerability]` × 4, all against the stuck old `rustls-webpki 0.102.8`: RUSTSEC-2026-0049,
  -0098, -0099, -0104 — name-constraint and CRL-parsing bugs, every one of them already fixed in the
  `0.103.x` line that `rustls` itself already wants, but unreachable here because
  `rustls-rustcrypto`'s manifest cannot be bumped without a release it has not had since
  **2024-04-24** (version `0.0.2-alpha`, no stable release ever cut).
- `error[vulnerability]`: `rsa v0.9.10` — RUSTSEC-2023-0071, the Marvin Attack timing side-channel,
  "no safe upgrade available."

That is not a maturity label copied from a README. It is what `cargo deny check` says today, against
this repo's own policy, about the best-known pure-Rust provider.

**`rustls-graviola`, measured.** `rustls` + `rustls-graviola` resolves to **12 crates**: `rustls`,
`once_cell`, `rustls-pki-types`, `rustls-webpki`, `subtle`, `untrusted`, `zeroize`, `rustls-graviola`,
`graviola`, `cfg-if`, `getrandom`, `libc`. No `*-sys`, no `cc`, no duplicate versions.
`cargo deny check` against this repo's `deny.toml`: **advisories ok, bans ok, licenses ok, sources
ok** (license clean once the crate under test carries a `license` field, which every real Esker crate
already does — the scratch project needed one added to reach that state). `graviola` covers the
requested minimal profile and then some: X25519/P-256/P-384 key exchange, AES-GCM and
ChaCha20-Poly1305/XChaCha20-Poly1305 AEADs, ECDSA P-256/P-384 and Ed25519 signatures, SHA-256/384/512
and HMAC. It builds only for `x86_64` and `aarch64` — exactly `deny.toml`'s two `[graph] targets`,
nothing else to worry about — with a portable-Rust fallback for at least SHA-256 where the CPU
intrinsic isn't available. Its own README states its maturity plainly: *"This project is very new, so
exercise due caution."* Not independently audited as a whole; its big-integer arithmetic reuses
assembly from AWS's **formally-verified** `s2n-bignum` rather than being freehand, which is a real
provenance signal and not a substitute for one. `graviola` last published 2026-06-24,
`rustls-graviola` 2026-06-17 — both roughly two years newer than `rustls-rustcrypto`'s last release,
and both from `ctz`, the original author of `rustls` and `webpki`. One incidental wrinkle: adding
`rustls-graviola` pulls in `rustls`'s own `tls12` feature by unification (its manifest asks for it);
that is a compiled-in code path, not an extra crate, and disabling TLS 1.2 at the protocol level would
still need to happen in the `ServerConfig`/`ClientConfig` construction, not at `Cargo.toml`.

**Certificate parsing and the root store.** `rustls-webpki` is already counted above. The root store
is the other half: `webpki-roots` (the vendored Mozilla list ADR 0025 flagged as "the likely answer")
adds **one crate** — but its license is `CDLA-Permissive-2.0`, which is not on `deny.toml`'s allow
list (`Apache-2.0`, `MIT`, `BSD-2/3-Clause`, `ISC`, `Unicode-3.0`, `Zlib`). `cargo deny check` fails on
`licenses` alone until that line is added. This closes the open paragraph ADR 0025 decision 4 left:
vendored roots are cheap in crate count and not free in policy — one line in `deny.toml`'s
`[licenses] allow` list, decided the same way every other line there was.

**What this option does NOT cover.** Transport encryption is not peer authorization — see the PD gap
above. It also doesn't decide the root-of-trust question for internal (store↔store, store↔PD) mTLS,
which needs its own certificate-issuance story (a private CA, most likely) that nothing here builds.

**Work to adopt, once a provider is picked.** Smaller than it looks, because the seams already exist:
`esker_s3::Transport` is already a trait with one implementor (ADR 0025 decision 1) — a `rustls`
implementor is a second one, not a refactor. `pgwire::server::Connection<S>` is **already generic over
its stream** rather than tied to `TcpStream`, specifically so "the whole startup handshake ... can be
driven over an in-memory pipe in a unit test" (`pgwire/server.rs:1-10`) — the same shape, already
built, for a reason that turns out to also be TLS's reason. Answering `S` instead of `N` and handing
the accepted socket through a TLS wrap before the loop re-reads the startup packet is the actual
change; nothing about `Connection`'s own logic moves. `esker-proto`'s `TransportConfig` needs new
knobs (a certificate+key for a server, a root store or explicit CA for a client) and its connection
setup needs the same "wrap the stream" step tokio's ecosystem convention already has a shape for
(`tokio_rustls`-style, though `tokio_rustls` itself is not on the allowlist and would need its own
line — wrapping `rustls::ServerConnection`/`ClientConnection` by hand over the existing
`AsyncRead + AsyncWrite` split is a few hundred lines, not a new dependency).

### Option 3 — in-house TLS 1.3, minimal profile

X25519 + ChaCha20-Poly1305 + SHA-256/HKDF, Ed25519 or ECDSA P-256 certificates, TLS 1.3 only, no
renegotiation, no session resumption at first — as specified. Taking each piece on its own honesty:

- **Record layer and handshake state machine.** This is the same shape of work as `esker-proto`'s own
  framing (`DESIGN.md` §9) — fixed headers, explicit lengths, no ambiguous cases — and the PG wire
  protocol's frontend decoder is already a precedent for "parse untrusted bytes, never panic." This
  part is squarely inside what this project already does well.
- **SHA-256, HKDF, ChaCha20-Poly1305.** Fixed-function constructions (ARX rounds, a stream cipher, a
  polynomial MAC) with no secret-dependent branches or table lookups by their nature. Realistically
  achievable at the same quality bar as `crc32c` — call it 600-900 lines with RFC test vectors.
- **X25519.** Compact — a few hundred lines — but every existing implementation earns that size by
  being written with real care around scalar clamping and constant-time conditional swaps. A
  functionally-correct-but-variable-time implementation passes every test that is not specifically a
  timing test, which is exactly the danger.
- **Certificate signature verification (Ed25519 or ECDSA P-256) plus the DER/X.509 parsing underneath
  it.** The DER parser is familiar shape (untrusted bytes, no panics). The signature math is not: a
  constant-time elliptic-curve implementation is a different skill and a different failure mode from
  everything else on this project's "build it ourselves" list. A wrong answer here does not turn the
  test suite red — the suite stays green and the private key leaks over the network to anyone who can
  measure response timing. This is not hypothetical: Minerva, Raccoon and Lucky13 are real, remotely
  exploited timing attacks against mature, widely-reviewed TLS implementations, not academic curiosities.
- **Total estimate: 4,000-8,000 lines of production code** for a genuinely minimal profile, plus a
  vector/fuzz/differential test suite that should be at least as large given the stakes. That is larger
  than any single item on CLAUDE.md's "written in-house instead" list, and every item on that list
  shares a property this does not: a bug in a CRC, a skiplist, or the RPC framing produces a wrong
  answer or a crash, and a test finds it. A bug in constant-time crypto can produce a right answer,
  every time, in every test, while leaking the key.

**What an independent review would have to cover**, concretely: (1) constant-time verification with a
dedicated tool — statistical timing tests (dudect-style) or secret-data-flow tracking
(Valgrind/ctgrind-style, the same category of tool `graviola`'s own optional `crabgrind` feature uses,
which is evidence that even a professional cryptography author treats this as a separate discipline
from functional correctness); (2) test-vector coverage against a standard corpus (Wycheproof, RFC
7748/8032/5869) — a discipline this project already has precedent for (ADR 0025's SigV4 vectors,
`crc32c`'s polynomial tests) that alone still does not catch (1); (3) interop and fuzz testing against
a mature TLS stack as both client and server, malformed-certificate and truncated-record cases
included; (4) a reviewer with cryptographic-implementation experience specifically — CLAUDE.md
invariant 8's "no `unsafe` without a `SAFETY` comment and a test" is aimed at memory safety, a
different risk from constant-time correctness, and *safe* Rust can still leak a key through a
data-dependent branch or a variable-latency instruction.

**What it does NOT cover:** post-quantum key exchange, session resumption, 0-RTT, or a TLS 1.2
fallback for any client that cannot speak 1.3 — all explicitly out of scope per the brief, and real
limitations an operator would hit, not just unfinished features.

**The ongoing cost, illustrated by this ADR's own measurement:** the four RUSTSEC advisories found
above against a stale `rustls-webpki` pin were each found and fixed by the `rustls` project's own
security process, for free, without Esker doing anything. An in-house stack gets that scrutiny only if
Esker commits to running an equivalent process indefinitely.

### Option 4 — hybrid: option 1 now, option 2 behind a default-off feature

Ship option 1's discipline on all three surfaces now (S3 already has it; extend "refuse a
configuration that looks encrypted and is not" to the PG and RPC surfaces too, rather than silently
accepting a `--tls` flag nothing backs). Add `rustls` + `rustls-graviola` behind a workspace feature —
call it `tls` — off by default, so `cargo build` without it never touches the new crates and
`crates/esker-cli/tests/dep_budget.rs`'s default run stays exactly where it is today. The feature is
what the maintainer turns on once the provider (or a successor) has more track record: a stable
release, or independent scrutiny, or enough production hours elsewhere to be a reasonable bet.

**A wrinkle this ADR's own reading of `deny.toml` surfaces:** `[graph] all-features = true` is already
set there, on purpose (dev-only tools like `proptest` are meant to be visible to the check). That
means `cargo deny check` evaluates the graph with **every** feature on, including a default-off `tls`
feature — so "off by default" protects `dep_budget.rs`'s crate count (which does respect Cargo's
actual default-feature resolution) but does **not**, by itself, keep the new crates invisible to
`cargo deny check`. Choosing this option means either accepting that `cargo deny check` sees the `tls`
feature's crates unconditionally (i.e., they must pass the same policy as everything else, gated or
not — which `rustls` + `rustls-graviola` already do, per the measurement above), or narrowing
`deny.toml`'s `[graph]` section so gated dependencies are excluded from the default check and verified
separately with `cargo deny check --features tls`. Either is a real edit to `deny.toml`, not a free
"the feature hides it" outcome — worth deciding explicitly rather than discovering in CI.

## Recommendation

**Option 4, with `rustls-graviola` named as the specific provider, not `rustls-rustcrypto`.**
`rustls-rustcrypto` fails this repo's own `cargo deny check` today, measured above, on grounds that
cannot be fixed by Esker (a stalled upstream release). `rustls-graviola` passes clean at 12 crates,
covers the requested profile, matches the CI targets exactly, and carries a real provenance signal in
its low-level arithmetic — but it is four months past a 0.4.0 release with no audit, and its own
author calls it "very new." That is a real security boundary, not a budget line, and a "vetted
exception" (ADR 0025's own phrase for option (c)) is a heavier thing to accept unconditionally than to
accept behind a flag with a stated bar for turning it on. Shipping option 1's refusal discipline on
all three surfaces costs nothing and is strictly better than today's silent `N`. Option 3 is not
recommended at this time: nothing here is blocked waiting for it, and the honest LOC estimate and
constant-time risk profile above are a different kind of project than the rest of this codebase.

**The decision this needs from the human:** whether to accept `rustls-graviola` (v0.4.x, unaudited,
but the only pure-Rust `CryptoProvider` that measures clean against `deny.toml` today) as the
dependency-policy exception now, behind a default-off `tls` feature, and if so, which surface — S3,
the PostgreSQL port, or the RPC layer — gets it first.

## Consequences, per option

- **Option 1:** no change to `deny.toml`, ADR 0003, or the budget. `DESIGN.md` §13 already points here
  (updated alongside this ADR); it still needs a deployment note spelling out that RPC and PG each need
  their own kind of terminator, and neither is "any TCP load balancer will do."
- **Option 2:** `ADR 0003`'s runtime dependency table gains rows for `rustls` and `rustls-graviola`
  (and `webpki-roots` if the root store is adopted the same way); `deny.toml`'s `[licenses] allow`
  gains `CDLA-Permissive-2.0` if so. The budget absorbs **+12** (or **+13** with `webpki-roots`)
  against the ceiling of 40 — comfortably inside it on this measurement, though the actual current
  count should be re-read with `just check` at merge time rather than trusted from ADR 0025's now
  two-phases-old "the teens" (`sqlparser`, ADR 0014, has landed since that was written).
- **Option 3:** no `deny.toml` or budget change — nothing new enters the graph. `ADR 0003` gains a
  "written in-house instead" row, and this ADR is the record of the LOC estimate and the review bar
  for whoever eventually attempts it.
- **Option 4:** the same `ADR 0003`/`deny.toml` additions as option 2, latent behind the feature, plus
  the `[graph] all-features = true` decision named above — a real edit either way, decided once rather
  than discovered when `cargo deny check` turns red on a branch that thought the feature was invisible.

## What was built, and what is still owed

Written after the fact, because two of the numbers above moved once the crates were in a real
workspace rather than a probe, and because building it found things the measurement could not.

### The PostgreSQL port, built

`SSLRequest` is answered `S` when the node holds a certificate, and the whole session — the
client's real startup packet included — runs inside TLS records. `--tls-cert` and `--tls-key` take
PEM. The provider is passed explicitly rather than installed with `install_default`, which is
process-global and would decide for anything else linking rustls in the same binary.

`esker-sql`'s `tls` feature is off by default, and the default build's runtime graph is unchanged —
36 crates by `dep_budget.rs`'s method, name for name, before and after. **With the feature on it is
nine crates, not the twelve measured standalone**, because `once_cell`, `cfg-if` and `libc` are
already in this workspace. `cargo deny check`, which always evaluates with `all-features = true`,
is green with them in the graph.

Three things the measurement did not predict, all now written down where someone will hit them:

* **`ring` and `cc` are in `Cargo.lock` and nothing builds them.** Cargo locks a version for every
  optional dependency edge, and `rustls-webpki` declares an unused optional `ring`. `deny.toml` and
  ADR 0003 carry the three checks that prove it is not in the graph.
* **`dep_budget.rs` would report that unused `ring` as a banned crate** the moment anything measured
  the feature-on graph, because it walks `resolve.nodes[].deps` without consulting the activated
  feature list. That is a false positive in the one test that must not have them, and it is owed to
  whoever owns `crates/esker-cli/`. The same flaw inflates the default count by two.
* **The budget has four crates of headroom, not twenty.** ADR 0025 said "the teens"; the real number
  today is 36 of 40, `sqlparser` and tokio's chain having landed since. Nothing here needs the
  budget raised, and the next thing that wants a crate should re-measure rather than trust either
  number.

### What S3 and RPC still need

Neither surface is touched by this work. Both terminate outside the process today, which is
option 1 and remains correct until someone does for them what this did for the PG port.

* **S3** (`esker-s3`, ADR 0025): the transport is already a trait with one blocking `std::net`
  implementor, so this is a second implementor and a config field, not a refactor. It needs the one
  thing the PG port did not: **a root store**, because a client verifies a certificate where a
  server only presents one. `webpki-roots` is one crate and one `CDLA-Permissive-2.0` line in
  `deny.toml`'s `[licenses] allow` — that licence is not on the list today and the check fails on
  it, measured. An explicit `--ca-file` is the other half, per ADR 0025 decision 4 item 3.
  `Endpoint::parse` should stop refusing `https://` in the same change that makes it work, and not
  before.
* **RPC** (`esker-proto`, DESIGN.md §9): the harder one, and not because of TLS. It is the surface
  where both ends are ours, so it is the one that wants **mutual** authentication — and mTLS gives
  a verified peer identity, not an authorization decision. Nothing in `esker-pd` today maps an
  identity to "may register as this store id" or "may vote in this region": a store's
  `StoreHeartbeat` and a peer's `RaftTransport::Batch` are accepted from whoever can open the
  socket. Encrypting that link without also deciding who is allowed on it moves the problem rather
  than solving it, and the deciding is application logic no option in this ADR provides. Whoever
  takes this surface should expect the certificate-issuance story (a private CA, most likely) and
  the identity check to be the bulk of the work, with the TLS itself the small part — `TlsConfig`
  and the session driver in `pgwire::tls` are the shape it can reuse.
