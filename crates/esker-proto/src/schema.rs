//! Asking one store for a table's columnar record, because the asker cannot read it itself.
//!
//! # The gap this closes
//!
//! A columnar learner decodes rows with a schema, and that schema is a **catalog record**:
//! `esker_keys::columnar::Published`, written by the `ALTER` that asks for a columnar copy, living
//! in the cluster's `'m'` key space. A store can read it only if it *hosts* the region covering
//! `'m'`. On a single-region cluster every store does, which is why this was invisible for a whole
//! phase; on any cluster that has split, a store holding a table's **rows** and not the catalog
//! has no way to learn how to decode them, so the columnar copy of every table outside the `'m'`
//! region silently does not exist (`docs/plans/phase-8-learner.md` §close, and
//! `crates/esker-store/src/columnar/region.rs`'s own module header, which names the fix).
//!
//! # Why a pull, and why store-to-store
//!
//! The record is already replicated, durable and versioned — it is an ordinary transactional value
//! in a region with its own Raft group. Nothing needs to be *pushed*: what the asking store lacks
//! is not the data but a way to reach it, and the placement driver already answers exactly that
//! question for any key (`PdReq::GetRegion`). So the store that needs a schema asks PD which
//! region covers the record's key, and asks a store hosting it for the bytes.
//!
//! Two consequences worth stating, because they are what make a pull the right shape here:
//!
//! * **the SQL layer changes not at all.** A push would have to originate where the `ALTER` runs,
//!   which means a new call in `esker-sql`, a new thing for it to know (which stores hold which
//!   rows), and a delivery it would have to retry. The record it already writes is enough;
//! * **nothing has to be remembered by a third party.** PD does not learn schemas, does not
//!   version them, and cannot be stale about them — it answers where a key lives, which is the one
//!   thing it is already the authority on.
//!
//! # What travels
//!
//! The record's **bytes**, exactly as they are stored, and not a decoded schema. `esker-proto`
//! does not depend on `esker-keys` and must not start: the record's format is
//! [ADR 0022](../../docs/adr/0022-columnar-learner-replica.md)'s and belongs to the layer that
//! writes it, so a wire message that decoded it would give the format a second owner and a second
//! place to drift. The asking store decodes it with the same `esker_keys::columnar::decode` it
//! would have used on its own engine, so a record fetched over the wire and a record read locally
//! go through one parser.

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};

/// Ask a store for the columnar record of one table.
///
/// Addressed to a **store** and not to a region, and carrying no [`RequestHeader`]: the asker does
/// not know which region covers the record — that is what it is asking about, transitively — and
/// an epoch it invented would be checked against a region it never routed to. The answering store
/// checks what it can, which is whether it holds the record at all.
///
/// [`RequestHeader`]: crate::RequestHeader
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaReq {
    /// Which tenant's catalog.
    pub tenant: u64,
    /// Which table in it.
    pub table_id: u64,
}

/// What a store answers with: the record's bytes, or nothing.
///
/// **`None` is a normal answer and never an error frame**, and the distinction is the same one
/// [`crate::fragment::FragmentResp::Refused`] draws. A store answers `None` when it does not host
/// the region covering the record, or when it does and the table has no columnar record — and the
/// asker must treat those the same way, by asking somewhere else or giving up on the copy. Making
/// either an error would turn "ask the next replica" into a fault, and would make a table that
/// simply does not want a columnar copy look like a broken cluster.
///
/// A record whose transaction has not committed is also `None`: a locked record is one whose
/// `ALTER` has not landed, and the commit that resolves it is itself a catalog write, so the next
/// ask sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaResp {
    /// The record as it is stored, for `esker_keys::columnar::decode`.
    pub record: Option<Bytes>,
}

impl SchemaReq {
    pub(crate) fn encode(&self, out: &mut Encoder) {
        out.put_varint(self.tenant);
        out.put_varint(self.table_id);
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            tenant: input.get_varint("schema.tenant")?,
            table_id: input.get_varint("schema.table_id")?,
        })
    }
}

impl SchemaResp {
    /// The answer a store with nothing to say gives.
    #[must_use]
    pub fn absent() -> Self {
        Self { record: None }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        // A presence byte rather than a zero-length record: an empty record is not the same thing
        // as no record, and a format that could not tell them apart would answer "this table has
        // no columnar copy" for a record that decoded to nothing.
        match &self.record {
            None => out.put_u8(0),
            Some(record) => {
                out.put_u8(1);
                out.put_bytes(record);
            }
        }
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match input.get_u8("schema.present")? {
            0 => Self { record: None },
            1 => Self {
                record: Some(Bytes::copy_from_slice(input.get_bytes("schema.record")?)),
            },
            other => {
                return Err(DecodeError::invalid(
                    "schema.present",
                    format!("presence byte {other} is not 0 or 1"),
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SchemaReq, SchemaResp};
    use crate::codec::{Decoder, Encoder};
    use bytes::Bytes;

    fn round_trip_resp(resp: &SchemaResp) -> SchemaResp {
        let mut out = Encoder::new();
        resp.encode(&mut out);
        let bytes = out.finish();
        let mut input = Decoder::new(&bytes);
        let back = SchemaResp::decode(&mut input).unwrap();
        input.finish().unwrap();
        back
    }

    #[test]
    fn a_request_round_trips() {
        let ask = SchemaReq {
            tenant: 7,
            table_id: 42,
        };
        let mut out = Encoder::new();
        ask.encode(&mut out);
        let bytes = out.finish();
        let mut input = Decoder::new(&bytes);
        let back = SchemaReq::decode(&mut input).unwrap();
        input.finish().unwrap();
        assert_eq!(back, ask);
    }

    /// The two answers, and the one that would be lost to a length-only encoding.
    #[test]
    fn an_absent_record_is_not_an_empty_one() {
        assert_eq!(round_trip_resp(&SchemaResp::absent()), SchemaResp::absent());
        let empty = SchemaResp {
            record: Some(Bytes::new()),
        };
        assert_eq!(round_trip_resp(&empty), empty);
        assert_ne!(round_trip_resp(&empty), SchemaResp::absent());
    }

    #[test]
    fn a_record_round_trips() {
        let resp = SchemaResp {
            record: Some(Bytes::from_static(b"a published record")),
        };
        assert_eq!(round_trip_resp(&resp), resp);
    }

    /// A presence byte this version does not define is corruption, not a default.
    #[test]
    fn an_unknown_presence_byte_is_refused() {
        let bytes = [2_u8];
        let mut input = Decoder::new(&bytes);
        assert!(SchemaResp::decode(&mut input).is_err());
    }
}
