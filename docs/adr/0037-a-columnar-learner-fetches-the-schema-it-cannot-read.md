# 0037 — a columnar learner fetches the schema it cannot read

Status: accepted. Debt wave c3, unit 7, from `docs/plans/debt-c3.md` §7 — recorded as "no schema
push, so a table outside the `'m'` region gets no columnar copy at all".

## Context

A columnar learner decodes rows with a schema, and that schema is a **catalog record**:
`esker_keys::columnar::Published`, written by the `ALTER` that asks for a columnar copy, living in
the cluster's `'m'` key space. `columnar::region::published_schema` reads it out of the store's own
engine — which works exactly when the store hosts the region covering `'m'`.

Every cluster this feature was built and measured on was single-region, where every store does. So
the copy worked everywhere anyone looked and existed nowhere else: on a cluster that has split
once, a store holding a table's **rows** and not the catalog answers `RefusalReason::NotColumnar`
for that table, for ever. `crates/esker-store/src/columnar/region.rs`'s module header names the gap
and names the fix as a schema *push*; `docs/plans/phase-8-learner.md` §close lists it as still
absent.

That refusal is the worst shape a missing feature can take. It is not an error and not a wrong
answer — it is the sentence that tells the planner *this replica has no copy, read the rows
instead*, which is a correct and useful thing to say and which nothing ever stops saying. A
columnar replica that is never used looks exactly like one that is not helping.

## Decision

**1. A pull, not a push.** The store that needs the schema fetches it, rather than the SQL node
sending it. The record is already replicated, durable and versioned; what the asking store lacks is
not the data but a way to reach it, and PD already answers exactly that question for any key.

Two consequences are the argument:

* **`esker-sql` changes not at all.** A push originates where the `ALTER` runs, which means a new
  call there, a new thing for it to know — which stores hold which rows — and a delivery it would
  have to retry against stores coming and going. The record it already writes is enough.
* **Nothing has to be remembered by a third party.** PD does not learn schemas, does not version
  them, and cannot be stale about them. It answers where a key lives, which it is already the
  authority on and which a client's routing already rests on.

**2. A new service and a new message, in their own file.** `esker_proto::schema`:
`Method::SchemaFetch = 0x0701` under `SERVICE_SCHEMA = 0x07`, carrying `SchemaReq { tenant,
table_id }` and answering `SchemaResp { record: Option<Bytes> }`. Its own service because it is the
only store-to-store request addressed to a **store** rather than a region, so anything routing or
metering on the service byte can see that it carries no `RequestHeader` without decoding a body.

**The record travels as bytes.** `esker-proto` does not depend on `esker-keys` and must not start:
the record's format belongs to the layer that writes it, and a wire message that decoded it would
give the format a second owner and a second place to drift. The asking store parses it with the
same `esker_keys::columnar::decode` it would have used on its own engine.

`None` is a normal answer and never an error frame — a store that does not host the range and a
table that never asked for a copy answer alike, and the asker treats them alike. The response
carries a **presence byte** rather than relying on length, because an empty record and an absent
one are different facts.

**3. The fetch is on the request path and never on the apply path.**
`crate::columnar::decode`'s module header already states the rule and the reason: *"the apply path
may not fetch — a schema lookup on the log's critical path makes apply latency depend on another
region's availability, and a lookup that fails stalls the log rather than failing a request"*. A
learner whose schema has not arrived stops advancing its applied index, which the heartbeat reports
and which `RefusalReason::TooFarBehind` turns into a row-scan fallback. `Store::ensure_schema` runs
in `serve_fragment`, which is the moment the schema is actually needed and a place that may wait.

**4. Every failure in the fetch is silent and leaves the schema unknown.** No PD, no answer, no
store reachable, a record that does not decode — each leaves the table refused as `NotColumnar`,
which is a refusal the planner already falls back from. A fetch that could fail a *fragment* would
make a columnar read less available than the row read it is an optimisation of.

**5. A fetched record is kept per table, and an older one never replaces a newer.** Nothing orders
two fetches — a slow answer from one store can land after a fast one from another — and installing
an older schema over a newer would make the copy refuse rows it had already decoded.
`schema_version` is monotonic per table and is the comparison `Published` documents itself for.
Installing one closes any open copy of that table, so the next read rebuilds under the new schema
rather than extending a copy built under the old one.

## Consequences

A table whose rows live outside the `'m'` region gets a columnar copy. That is the feature ADR 0022
describes, working on the cluster shape it was always meant for.

**A store holding this cache sees no catalog writes for these tables**, so nothing pushes a schema
change at it. That is safe rather than merely tolerable, and the reason is already in the design:
`decode_row` **refuses** a row wider than the schema it is read against — `DecodeOutcome::
SchemaBehind` — so a stale schema makes the copy stop, loudly, rather than answer wrongly. A copy
that stops is a learner that is behind, which the system already reports and already falls back
from. What is *not* yet built is the re-fetch that would clear it automatically; until it is, a
`SchemaBehind` on a fetched table is resolved by the copy being rebuilt. Recorded in
`docs/plans/debt-c3.md` §7 rather than hidden here.

One `SchemaFetch` round trip per table per store, on the first fragment for that table. A store
that can read the record locally never makes one, which on a single-region cluster is every store —
so nothing about the existing shape changes cost.

## Alternatives rejected

**A push from `esker-sql`.** The shape the original note named. It puts the retry, the routing
knowledge and the failure handling in the layer that has least of each, and it is forbidden to this
lane besides — the brief says an `esker-sql` change goes in the report as a NEEDS. It turned out
not to need one.

**A push through PD.** PD would have to learn schemas, version them, and be asked about them, which
widens the one component whose staleness is hardest to reason about. It already knows where a key
lives; that is enough.

**Carry the schema in the region's Raft log.** The store that owns the row region would learn it
with the rows. But the schema is not that region's data, so something has to propose it there —
which is a push with an extra hop, and it puts a catalog fact into a log that has no other reason
to hold one.
