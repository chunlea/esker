# 0015 — The Percolator key and record encodings

Status: accepted (phase 5). Supersedes nothing. See `crates/esker-txn/src/key.rs`,
`crates/esker-txn/src/codec.rs`, `docs/txn-spec.md` §1–§4, `docs/DESIGN.md` §3 and §8.

## Context

`docs/DESIGN.md` §8 gives the shape of the three column families in one table — `lock` holds
`{primary, start_ts, ttl, kind, short_value?}` at `user_key`, `write` holds
`{kind, start_ts, short_value?}` at `user_key ++ enc(commit_ts)`, `default` holds the value at
`user_key ++ enc(start_ts)`. Everything below that is a decision, and three of them are the kind a
future reader would otherwise reverse without knowing what they were paying for.

These bytes are an on-disk format with a golden file (ADR 0002), so this is the record of *why*,
which a re-blessed golden file would otherwise erase.

## Decision 1: the user key is group-encoded before the version is appended

`'x' ++ encode_bytes(user_key) ++ enc_ts(ts)`, not `'x' ++ user_key ++ enc_ts(ts)`.

The layout as §3 writes it is correct only when the user key is prefix-free, which the SQL layouts
are and arbitrary `TxnKV` keys are not. With the raw form the versions of `"a"` interleave with
those of `"ab"`:

```
'x' "a"  !0  =  78 61 ff ff ff ff ff ff ff ff      <- "a" at ts 0
'x' "ab" !0  =  78 61 62 ff ff ff ff ff ff ff ff   <- "ab" at ts 0, and it sorts FIRST
```

A seek for "the newest version of `a` at or below `ts`" then lands inside `ab`'s versions, and the
prefix check that is supposed to catch it — "does this key start with `'x' ++ "a"`?" — says yes,
because `"a"` *is* a prefix of `"ab"`. The read returns another key's value with nothing anywhere
reporting an error. `esker-keys`' own property test for this ordering excludes the case with an
explicit `prop_assume!`, which is how we know the shape was already understood there.

**Options.** (a) Check the *decoded* length as well as the prefix on every seek — one more
comparison per read, and a rule every future call site has to remember. (b) Append a length to the
key — a second encoding to specify, and it breaks range scans, because a length prefix is not
order-preserving. (c) Use the memcomparable group encoding that `esker-keys` already has and that
`docs/DESIGN.md` §3 already calls prefix-free.

**(c).** It removes the case rather than defending against it: no encoded key is a byte prefix of
another, so each key's versions are one contiguous run and "seek, then check the prefix" is
correct by construction. It is also what TiKV does, for this reason.

**Consequences.** One byte per eight of key, plus a whole padding group when the length is a
multiple of eight — about 12.5%, on keys only. Scan bounds have to be encoded the same way, which
they are (`key::version_range`). And `esker_keys::prefix::txn_key` — the raw form — stays as it is
and is not used by `esker-txn`: it is correct for prefix-free callers, and changing it would be a
format change to a golden file for the benefit of a caller that no longer wants it.

## Decision 2: the kind byte is the format's version field

Both records begin with a one-byte kind, and a decoder that meets a tag it does not know refuses
the record. There is no separate version byte.

**Options.** (a) A version byte on every record — two bytes of fixed overhead on records that are
otherwise eight, and a version that in practice never changes because every real change is "a new
kind of record". (b) Length-prefix the record and ignore trailing fields — forward compatible, and
forbidden: `docs/DESIGN.md` §9 says unknown fields are errors, not ignored, and a lock silently
missing a field it needed is exactly the failure that rule exists to stop. (c) The kind byte.

**(c).** The four tags are `Put`, `Delete`, `Rollback` and `Lock` and only three of them are legal
in the `lock` CF, so the discriminator is doing real work already; a fifth kind is how a future
version adds a field, and old code refuses it loudly rather than reading it wrong. The SST that
holds these values carries its own format version and magic, so the bytes are not unframed.

**Consequences.** A change that is *not* expressible as a new kind — widening `ttl_ms`, say —
needs a new tag or a new column family, not a version bump. `Kind::Lock` is written by nothing
today and exists so that `SELECT … FOR UPDATE` is not a format change later.

## Decision 3: an inline value is a presence byte, then a length byte

`0x00` for absent; `0x01 ++ len:u8 ++ len bytes` for present.

**Options.** (a) A single varint where `0` means absent and `n+1` a value of `n` bytes — one byte
for everything up to 126, and clever in the way that costs a reader ten seconds every time.
(b) "The rest of the record is the value" — free, and it stops being unambiguous the moment a field
is added after it, which is exactly what decision 2 says will happen. (c) A presence byte and a
length byte.

**(c).** Two bytes, and the presence byte is separate from the length because `put(k, b"")` is a
legal write: "absent" and "present, zero bytes" are different records, and a format that cannot
tell them apart turns an empty value into a missing one. The single length byte is what fixes the
inline cutoff at 255 (`docs/DESIGN.md` §8), and the phase-0 crate already asserted that
relationship before there was a format to hold it.

**Consequences.** Two bytes on every record, one of them almost always `0x00`. A value over 255
bytes is not representable inline, which is the intent: it belongs in the `default` CF, and the
encoder will not produce one.

## What this makes true

- Each key's versions are contiguous, so a read at `ts` is one forward seek and a prefix check.
- Every malformed byte string is a typed error rather than a wrong record: unknown kind, a
  `Rollback` in the `lock` CF, a value on a kind that has none, a presence byte that is neither, a
  length past the end, an empty `primary`, and any trailing byte (`CLAUDE.md` invariant 9).
- The encoding is canonical — one byte string per record — which is what lets
  `crates/esker-txn/tests/golden/txn.txt` mean anything.
