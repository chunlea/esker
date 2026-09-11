//! The reserved key-space layout (`docs/DESIGN.md` §3).
//!
//! Every key in a cluster begins with a one-byte namespace, so the four users of the key
//! space cannot collide and each can be scanned as a range:
//!
//! ```text
//! 'r' ++ user_key                                          RawKV, no MVCC
//! 'x' ++ user_key ++ enc_ts                                TxnKV, Percolator
//! 't' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ row_id       SQL rows      (phase 6)
//! 't' ++ tenant:u64 ++ table_id:u64 ++ 'i' ++ index_id …   SQL indexes   (phase 6)
//! 'm' ++ …                                                 cluster metadata
//! ```
//!
//! A versioned key ends with a fixed eight-byte timestamp, which is what lets the engine's
//! prefix extractor for those column families be "strip the last eight bytes".

use crate::codec::{self, CodecError, FIXED_INT_SIZE};

/// `RawKV`: byte-opaque keys with no MVCC.
pub const RAW: u8 = b'r';

/// `TxnKV`: transactional keys, each suffixed with an encoded timestamp.
pub const TXN: u8 = b'x';

/// SQL tables and indexes (phase 6).
pub const SQL: u8 = b't';

/// Cluster and catalog metadata.
pub const META: u8 = b'm';

/// Every namespace byte. They must stay distinct — that is the whole guarantee.
pub const ALL: [u8; 4] = [RAW, TXN, SQL, META];

/// Marks the row part of a SQL table's key space.
pub const SQL_ROW: u8 = b'r';

/// Marks the index part of a SQL table's key space.
pub const SQL_INDEX: u8 = b'i';

/// Width of the encoded timestamp suffix on a versioned key.
pub const TS_SUFFIX_LEN: usize = FIXED_INT_SIZE;

/// `'r' ++ user_key`.
#[must_use]
pub fn raw_key(user_key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + user_key.len());
    out.push(RAW);
    out.extend_from_slice(user_key);
    out
}

/// `'x' ++ user_key ++ enc_ts(ts)`.
///
/// The timestamp is complemented, so versions of one key sort newest first and a read at a
/// snapshot is a seek rather than a scan.
#[must_use]
pub fn txn_key(user_key: &[u8], ts: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + user_key.len() + TS_SUFFIX_LEN);
    out.push(TXN);
    out.extend_from_slice(user_key);
    out.extend_from_slice(&codec::enc_ts(ts));
    out
}

/// Splits a versioned key into the part before the timestamp and the timestamp itself.
///
/// This is the "strip the last eight bytes" prefix extractor from `docs/DESIGN.md` §3, and it
/// is where a truncated or non-versioned key is caught rather than silently mis-split.
pub fn split_ts(key: &[u8]) -> Result<(&[u8], u64), CodecError> {
    if key.len() < TS_SUFFIX_LEN {
        return Err(CodecError::Truncated {
            needed: TS_SUFFIX_LEN,
            found: key.len(),
        });
    }
    let (prefix, suffix) = key.split_at(key.len() - TS_SUFFIX_LEN);
    Ok((prefix, codec::dec_ts(suffix)?))
}

/// `'m' ++ suffix`.
#[must_use]
pub fn meta_key(suffix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + suffix.len());
    out.push(META);
    out.extend_from_slice(suffix);
    out
}

/// `'t' ++ tenant ++ table_id ++ 'r'` — the prefix every row of one table shares.
///
/// The ids are encoded memcomparably, so a scan of this prefix visits the table's rows in
/// row-id order. TODO(phase-6): the row id itself is appended by `esker-sql`.
#[must_use]
pub fn table_row_prefix(tenant: u64, table_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 + 8 + 1);
    out.push(SQL);
    codec::encode_u64(tenant, &mut out);
    codec::encode_u64(table_id, &mut out);
    out.push(SQL_ROW);
    out
}

/// The tenant and table a row key belongs to, or `None` when it is not a row key.
///
/// The inverse of [`table_row_prefix`], and it lives here because that is where the layout is
/// written: a reader that took the two `u64`s apart itself would be a second place to update when
/// the prefix changes (`CLAUDE.md` invariant 7 — key semantics live in `esker-keys`).
///
/// **Total, and refuses rather than guesses**: an index entry, a metadata key, a key too short to
/// hold the header, all answer `None`. `pg_locks` reads it over keys it did not build.
#[must_use]
pub fn row_key_table(key: &[u8]) -> Option<(u64, u64)> {
    const HEADER: usize = 1 + 8 + 8 + 1;
    if key.len() < HEADER || key[0] != SQL || key[HEADER - 1] != SQL_ROW {
        return None;
    }
    let tenant = u64::from_be_bytes(key[1..9].try_into().ok()?);
    let table_id = u64::from_be_bytes(key[9..17].try_into().ok()?);
    Some((tenant, table_id))
}

/// `'t' ++ tenant ++ table_id ++ 'i' ++ index_id` — the prefix one index shares.
///
/// TODO(phase-6): the indexed columns and row id are appended by `esker-sql`.
#[must_use]
pub fn table_index_prefix(tenant: u64, table_id: u64, index_id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 + 8 + 1 + 8);
    out.push(SQL);
    codec::encode_u64(tenant, &mut out);
    codec::encode_u64(table_id, &mut out);
    out.push(SQL_INDEX);
    codec::encode_u64(index_id, &mut out);
    out
}

/// Which part of a table's key space a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablePart {
    /// A row: `'t' ++ tenant ++ table_id ++ 'r' ++ row_id`.
    Row,
    /// An index entry: `'t' ++ tenant ++ table_id ++ 'i' ++ index_id ++ …`.
    Index,
}

/// The table a SQL key belongs to: `(tenant, table_id, which part)`.
///
/// The inverse of [`table_row_prefix`] and [`table_index_prefix`], and it lives here for the
/// reason `CLAUDE.md` invariant 7 gives: key semantics belong in `esker-keys` and nowhere
/// below it. The garbage collector needs it — a per-table retention window is looked up by the
/// table a version's key names ([ADR 0021](../../../docs/adr/0021-time-machine.md)) — and the
/// collector sits in `esker-store`, which must not learn to parse a key layout that is
/// described here.
///
/// A **decode, not a parse**: the ids are memcomparable fixed-width, so this reads them rather
/// than searching for delimiters.
///
/// `None` for a key that is not in the SQL namespace at all — a `RawKV` key, a transactional
/// one, a metadata one. That is not an error: a collector sees every key in a column family and
/// most of them are nobody's table.
pub fn split_table(key: &[u8]) -> Result<Option<(u64, u64, TablePart)>, CodecError> {
    let Some(rest) = key.strip_prefix(&[SQL]) else {
        return Ok(None);
    };
    // Too short to be a table key. Not an error for the same reason as the namespace check: a
    // key that is not one of these is simply not one of these.
    if rest.len() < FIXED_INT_SIZE * 2 + 1 {
        return Ok(None);
    }
    let (tenant, rest) = codec::decode_u64(rest)?;
    let (table_id, rest) = codec::decode_u64(rest)?;
    let part = match rest.first() {
        Some(&SQL_ROW) => TablePart::Row,
        Some(&SQL_INDEX) => TablePart::Index,
        // A `'t'` key whose third field is neither. Nothing this crate writes produces one, so
        // it is a key from somewhere else rather than damage — and answering `None` leaves it
        // to whatever owns it.
        _ => return Ok(None),
    };
    Ok(Some((tenant, table_id, part)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// If two namespaces shared a byte, a scan of one would return the other's keys.
    #[test]
    fn namespaces_are_distinct() {
        let unique: std::collections::BTreeSet<u8> = ALL.into_iter().collect();
        assert_eq!(
            unique.len(),
            ALL.len(),
            "two namespaces share a prefix byte"
        );
    }

    /// Rows and indexes of one table must not overlap, and both must stay inside the SQL
    /// namespace of their own tenant and table.
    #[test]
    fn rows_and_indexes_of_a_table_do_not_overlap() {
        let rows = table_row_prefix(1, 7);
        let indexes = table_index_prefix(1, 7, 0);
        assert!(!indexes.starts_with(&rows));
        assert!(!rows.starts_with(&indexes));
        assert_eq!(rows[0], SQL);
        assert_eq!(indexes[0], SQL);
        // 'i' sorts before 'r', so a table's indexes precede its rows.
        assert!(indexes < rows);

        // Different tenants and tables never share a prefix.
        assert!(!table_row_prefix(2, 7).starts_with(&rows));
        assert!(!table_row_prefix(1, 8).starts_with(&rows));
    }

    #[test]
    fn txn_keys_sort_newest_version_first() {
        let old = txn_key(b"account/1", 100);
        let new = txn_key(b"account/1", 200);
        assert!(new < old, "the newer version must come first");

        let (prefix, ts) = split_ts(&new).unwrap();
        assert_eq!(prefix, txn_key(b"account/1", 0).get(..10).unwrap());
        assert_eq!(ts, 200);
    }

    /// Every version of one key must sort before any version of the next key, or a scan of
    /// one key's versions would run into its neighbour.
    #[test]
    fn versions_stay_grouped_by_key() {
        let a_newest = txn_key(b"a", u64::MAX);
        let a_oldest = txn_key(b"a", 0);
        let b_newest = txn_key(b"b", u64::MAX);
        assert!(a_newest < a_oldest);
        assert!(a_oldest < b_newest);
    }

    #[test]
    fn namespaces_do_not_bleed_into_each_other() {
        let raw = raw_key(b"x-marks-the-spot");
        let txn = txn_key(b"anything", 1);
        let meta = meta_key(b"cluster-id");
        assert_ne!(raw[0], txn[0]);
        assert_ne!(raw[0], meta[0]);
        assert_ne!(txn[0], meta[0]);
    }

    /// The decoder is the inverse of the two encoders, for a row and for an index alike.
    #[test]
    fn a_table_key_decodes_back_to_the_table_that_built_it() {
        let mut row = table_row_prefix(7, 42);
        row.extend_from_slice(b"row-id");
        assert_eq!(split_table(&row).unwrap(), Some((7, 42, TablePart::Row)));

        let mut index = table_index_prefix(7, 42, 3);
        index.extend_from_slice(b"cols");
        assert_eq!(
            split_table(&index).unwrap(),
            Some((7, 42, TablePart::Index))
        );

        // The bare prefixes, with nothing appended, decode too: a collector may meet one.
        assert_eq!(
            split_table(&table_row_prefix(0, 0)).unwrap(),
            Some((0, 0, TablePart::Row))
        );
        assert_eq!(
            split_table(&table_index_prefix(u64::MAX, u64::MAX, 1)).unwrap(),
            Some((u64::MAX, u64::MAX, TablePart::Index))
        );
    }

    /// A key that is not a table's is `None` and not an error. A collector sees every key in a
    /// column family and most of them are nobody's table; failing on one would stop the sweep.
    #[test]
    fn a_key_that_is_not_a_table_s_is_not_an_error() {
        assert_eq!(split_table(b"").unwrap(), None, "empty");
        assert_eq!(split_table(&raw_key(b"k")).unwrap(), None, "RawKV");
        assert_eq!(split_table(&txn_key(b"k", 1)).unwrap(), None, "TxnKV");
        assert_eq!(
            split_table(&meta_key(b"cluster")).unwrap(),
            None,
            "metadata"
        );
        // In the SQL namespace but too short to carry two ids.
        assert_eq!(split_table(b"t").unwrap(), None);
        assert_eq!(split_table(&[SQL; 16]).unwrap(), None);
        // Two ids, and a third field that is neither a row nor an index.
        let mut odd = vec![SQL];
        codec::encode_u64(1, &mut odd);
        codec::encode_u64(2, &mut odd);
        odd.push(b'?');
        assert_eq!(split_table(&odd).unwrap(), None);
    }

    #[test]
    fn split_ts_rejects_a_key_that_is_too_short() {
        assert!(split_ts(b"short").is_err());
        assert!(split_ts(b"").is_err());
        assert!(split_ts(&[0u8; 8]).is_ok());
    }

    /// The inverse really is the inverse, and it refuses what it did not build.
    #[test]
    fn a_row_key_names_its_tenant_and_table() {
        let key = table_row_prefix(7, 42);
        assert_eq!(row_key_table(&key), Some((7, 42)));
        let mut longer = key.clone();
        longer.extend_from_slice(b"whatever the primary key encodes to");
        assert_eq!(row_key_table(&longer), Some((7, 42)));

        assert_eq!(
            row_key_table(&table_index_prefix(7, 42, 1)),
            None,
            "an index entry is not a row"
        );
        assert_eq!(row_key_table(&[SQL]), None, "too short to hold the header");
        assert_eq!(row_key_table(b""), None);
        let mut wrong = key.clone();
        wrong[0] = META;
        assert_eq!(
            row_key_table(&wrong),
            None,
            "another namespace is not a row key"
        );
    }
}
