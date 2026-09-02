# 0039 — The S3 transport keeps its connection

Status: accepted. Extends [ADR 0025](0025-s3-transport-and-tls.md) (plain HTTP, transport behind a
trait) and leaves [ADR 0024](0024-tiering-failure-semantics.md) decision 2 (retrying belongs to the
client) exactly where it was.

## Context

`esker-s3` opened a fresh TCP connection for every request and sent `Connection: close`. That was
written for the uploader, which makes one `PutObject` per flush, and `http.rs` said so: *"one
connection per request means no state to get wrong between them, and an upload per flush does not
need pooling."*

The read path is not that shape. A cold tiered read is **one ranged `GET` per block**, and
`docs/bench/phase-6b.md` §2 measured 200,016 of them for 200,000 reads — one round trip per read,
by design. §3 measured what one costs: **692 µs single-threaded over loopback**, and named the
cause. 692 µs on loopback for a 4 KiB range is not the network and it is not `MinIO`; it is a TCP
handshake plus a socket setup, paid per block.

That section also said what to do about it and deliberately did not do it: *"connection reuse is the
single change with the most headroom behind it, and it is local to `esker_s3::transport` — the trait
exists exactly so this is one implementor's problem."* This is that change, with the measurement it
asked for.

## Options

1. **Leave it.** Correct, and the read path pays a handshake per block for ever. Against real S3 —
   a wide-area round trip, 10–100× loopback — the handshake is a smaller *fraction*, but it is
   also a full extra round trip on a link where round trips are the whole cost.
2. **One connection, no pool.** A single kept connection behind a mutex. Simple, and it serialises
   every reader thread in the process behind one socket, which is a worse bug than the one being
   fixed.
3. **A pool per endpoint, bounded.** What this does.
4. **Pipelining.** Several requests in flight on one connection. It would remove more round trips
   and it makes response parsing genuinely dangerous: a reader that over-reads past one response's
   body is reading the next one's, and the parser is the fuzzed half of this crate. Refused.

## Decision

**Option 3.** `TcpTransport` keeps idle connections, at most 16 per endpoint, and hands one back
out for the next request to the same endpoint. Requests say `Connection: keep-alive` — redundant on
HTTP/1.1 and written anyway, because a proxy or an HTTP/1.0 endpoint reads it.

Three rules bound what reuse can do wrong:

1. **A connection is pooled only after a framed response was read whole.** Never mid-body, never
   after any error, and never when the answer said `Connection: close` or carried no framing at all
   (`Response::may_reuse_connection`). What is in the pool is therefore always a socket at a message
   boundary. An error drops the connection instead, because a socket whose position in the message
   stream is unknown is the one thing that must never come back out of a pool.
2. **No pipelining.** One request is in flight at a time, so the only bytes on the wire when a
   response is read are that response's — which is what makes the existing parser safe to reuse
   unchanged. This is not an accident to rely on quietly; it is the reason option 4 is refused.
3. **The transport still does not retry.** ADR 0024 decision 2 puts retrying in `S3Client`, because
   only it knows whether an operation is idempotent, and that matters more now than before:
   `PutObject` with `If-None-Match: *` ([ADR 0029](0029-the-sst-store-claim.md)) is a request whose
   silent replay would answer `412` and hand a prefix to the wrong owner.

   What replaces a retry is a **check before reuse**: an idle connection is peeked at
   non-blockingly, and one the peer has closed is dropped and replaced rather than written into. A
   server's idle timeout — the common case, and the one that would otherwise turn a saving into a
   stream of failed requests — never becomes a failed request at all. The residual race, a peer that
   closes between the peek and the write, surfaces as `Error::Io`, which `is_retryable` already
   answers `true` for, and the layer that owns idempotency decides.

`TcpTransport::connections_opened` counts what has been opened, because reuse is invisible in a
response: the bytes are identical either way, so a test and an operator need something else to look
at.

## Consequences

* `TcpTransport` is no longer `Copy` or `Clone` — it owns sockets. Its one constructor site wraps it
  in an `Arc` already.
* File descriptors: at most 16 idle per endpoint, per transport. `close_idle` empties the pool for a
  caller that wants them back.
* The numbers are in `docs/bench/debt-c4.md`, taken the way `docs/bench/phase-6b.md` §3 asked for
  them: the same command, the same container, before and after.
* When TLS lands it is a second implementor of `Transport`, and it inherits none of this — a TLS
  session is more expensive to establish than a TCP connection, so the case for keeping one is
  strictly stronger there.
