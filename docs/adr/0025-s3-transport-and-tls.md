# 0025 — The S3 transport, and what we are doing about TLS

Status: accepted (phase 6b), with the TLS half deliberately deferred and its exit criteria
written down. Settles the option list in `docs/DESIGN.md` §13 ("TLS is the known hard case for
the pure-Rust rule"). See `crates/esker-s3/`, `docs/plans/phase-6b.md`,
[ADR 0024](0024-tiering-failure-semantics.md), [ADR 0003](0003-dependencies.md).

## Context

Tiering needs an S3 client. `CLAUDE.md` forbids the AWS SDK, `hyper`, `reqwest`, and anything
whose graph reaches a `*-sys` crate, and the parts we actually need are four calls, one signing
algorithm and one hash — all of which are the kind of thing this project exists to write. So the
client is in-house and that part is not in question.

TLS is. `DESIGN.md` §13 named it the known hard case and listed three options without choosing:

> (a) plain HTTP to a local MinIO or a TLS-terminating sidecar, (b) `rustls` with a pure-Rust
> crypto provider, (c) accepting one vetted exception.

Real S3 is HTTPS-only. So the choice is not "do we want TLS" — it is "when, and at what cost to
the dependency policy". A phase that has no working tiering at all is in no position to spend its
budget on the transport underneath it.

## Decision 1: the transport is a trait, and the blocking `std::net` implementation is the default

`esker_s3::Transport` is one method: given a host, a port and a request's bytes, return a stream
of response bytes. `TcpTransport` implements it over `std::net::TcpStream`. That is the whole
seam, and it is deliberately small enough that a TLS implementation is a second implementor
rather than a refactor.

This is what `DESIGN.md` §13 asked for in so many words — "design so the transport is a trait and
the choice is local to one module" — and it has a second benefit the design note did not
anticipate: `esker-s3` ends up with **no dependency beyond `esker-base` and `thiserror`**, so
adding an S3 client to the workspace costs zero against the 40-crate budget in `deny.toml`.

## Decision 2: blocking, not tokio

The lane brief suggested tokio TCP. We are not doing that, and the reason is a rule that outranks
the brief.

`CLAUDE.md`, "Toolchain and conventions": *async only at the network edge (`tokio`); the engine
and Raft cores are synchronous and `std`-only*. The uploader is driven from the engine's flush and
compaction threads (ADR 0024 decision 1 puts it after the manifest edit, but still on those
threads' timeline), and a tiered `FileSystem`'s `open` must be able to fetch an object from inside
a synchronous read path. Making either async would mean either a runtime handle threaded through
`esker-engine` or a `block_on` inside it — the first breaks the rule, the second is a runtime
inside a library that already has threads, which is the shape that deadlocks.

Blocking sockets on a dedicated uploader thread are the boring correct answer. The engine already
owns compaction threads; one more is not a new category of thing.

A tokio transport remains available to anyone who wants it: implement `Transport` in a crate that
already depends on tokio (`esker-store` does). Nothing in `esker-s3` has to change.

## Decision 3: option (a) — plain HTTP, and the milestone is MinIO

Phase 6b ships **plain HTTP to a MinIO endpoint**, exactly as `prompts/06-sql-serverless.md` §6b
allows ("plain HTTP to MinIO is acceptable for the first milestone").

The endpoint is configuration: `--sst-store s3://bucket/prefix` names the bucket, and
`ESKER_S3_ENDPOINT` (or the endpoint field on the config struct) names the host and scheme.
`https://` is **rejected at parse time with an error that says why and points at this ADR**,
rather than accepted and silently downgraded. A configuration that looks encrypted and is not is
worse than one that refuses to start.

For an operator who needs to reach real S3 before we ship TLS, option (a) has a second half that
costs us nothing: a TLS-terminating sidecar on localhost. That is a deployment note, not a
feature, and it is written down in the plan.

Why not the other two, yet:

- **(b) `rustls` with a pure-Rust provider.** This is where we intend to end up. It is not a
  small piece of work: `rustls` itself is fine, but the default provider is `aws-lc-rs`, which is
  banned and would have to be replaced with `rustls-rustcrypto` or an equivalent, and the whole
  `RustCrypto` graph then has to be audited crate by crate for `cc` and for `links` keys, against
  a budget of 40 that currently stands in the teens. Doing that *and* writing SigV4, an HTTP
  codec, a tiered filesystem and a disk-cache governor in one phase is how a phase misses.
- **(c) a vetted exception.** Still on the table, but it should be taken with a measurement in
  hand — the crate count and the build-script audit from actually attempting (b) — not on the
  suspicion that (b) will be unpleasant.

## Decision 4: what has to be true before TLS lands

Written down now so that "later" has a definition:

1. A dependency count for `rustls` + a pure-Rust provider, measured with
   `cargo tree -e no-dev` against `deny.toml`'s budget, with every build script named.
2. Certificate verification that actually verifies. A `TransportConfig` with a
   `danger_accept_invalid_certs` escape hatch is not TLS; if it appears, it is because a test
   needs it and it is `#[cfg(test)]`.
3. A decision about the root store. `webpki-roots` is a vendored copy of Mozilla's list and is
   pure Rust; reading the platform store is not, on macOS. Vendored roots plus an explicit
   `--ca-file` is the likely answer and needs its own paragraph in that ADR.
4. The budget in `deny.toml` raised by ADR if it must be, per `CLAUDE.md` — not quietly.

Until all four exist, `https://` stays refused.

## Decision 5: SigV4 is implemented even though the first milestone is unencrypted

Signing is not authentication-over-TLS; it is a keyed hash over the canonical request, and MinIO
checks it. Implementing it now means the only thing standing between this client and real S3 is
the transport, which is the point of putting the transport behind a trait.

The signing key derivation, the canonical request and the string-to-sign are golden-tested against
the published `aws-sig-v4-test-suite` vectors, and each test names where its vector came from. A
signing implementation that only agrees with itself is worth nothing, which is the same argument
`esker-base::crc32c` makes about polynomials.

## Consequences

- Phase 6b's tiering works against MinIO and against any S3-compatible endpoint reachable over
  HTTP, including real S3 behind a local terminator. It does not work against real S3 directly,
  and says so at parse time.
- `esker-s3` costs nothing against the dependency budget, so the budget stays available for the
  TLS work when it happens.
- The engine stays synchronous and `std`-only. No `tokio` appears below the network edge.
- We carry an in-house HTTP/1.1 response parser, which is a parser exposed to bytes we did not
  write. It is fuzzed, it never panics, and it handles exactly the subset the four calls need —
  status line, headers, `Content-Length` and chunked bodies — rejecting everything else rather
  than guessing (`CLAUDE.md` invariant 9, and the same discipline as `esker-proto`'s framing).
- When TLS lands it is one new implementor of one trait plus a dependency ADR, not a rewrite.

## Closing note (2026-09-04): decision 4 is satisfied, and `https://` is accepted

Decision 3 shipped plain HTTP and decision 4 listed the four things that had to be true before TLS
landed here. All four now are, and the transport exists —
[ADR 0055](0055-the-tls-options-across-three-surfaces-measured.md) is the decision and
`esker_s3::tls` is the code. Against decision 4's own list:

1. **A dependency count, measured.** Nine crates for the whole TLS exception, shared with the
   PostgreSQL port, and this surface added none of its own: 34 crates by `cargo tree` with the
   feature off, 43 with it on. `cargo deny check` is green with them in the graph.
2. **Certificate verification that verifies.** There is no `danger_accept_invalid_certs`, in any
   form, in any build. `tests/https.rs` proves the refusals rather than asserting the happy path
   alone: a chain to an untrusted root, a certificate for another name, and a plaintext server
   behind an `https://` URL are each refused, and each is checked to be **non-retryable** so the
   uploader cannot wait out a misconfiguration for ever.
3. **The root store decision, with its own paragraph.** It is not `webpki-roots`, which this ADR
   expected. The host's CA bundle is a file, `SSL_CERT_FILE` is the conventional override, and
   `ESKER_S3_CA_CERT` names one for a self-signed endpoint — so the vendored-roots crate and the
   `CDLA-Permissive-2.0` licence line it would have cost are both avoided. ADR 0055 has the
   reasoning and the trade.
4. **The budget, raised only if it must be.** It did not have to be: with the feature off the graph
   is unchanged, and nothing measures the feature-on graph against the budget today (ADR 0055 says
   why, and what would have to be fixed in `dep_budget.rs` first).

**What changes here:** `Endpoint::parse` accepts `https://` in a build with the `tls` feature and
still refuses it without one — now naming the feature as well as this ADR, because "rebuild with
`--features tls`" is the actionable half. Decision 3's sidecar deployment note stays true and stays
the answer for a build that does not carry TLS.

The transport trait of decision 1 needed no change of any kind, which is the part worth keeping:
the seam was designed for exactly this and it held.
