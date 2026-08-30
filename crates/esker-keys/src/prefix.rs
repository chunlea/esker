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

    #[test]
    fn split_ts_rejects_a_key_that_is_too_short() {
        assert!(split_ts(b"short").is_err());
        assert!(split_ts(b"").is_err());
        assert!(split_ts(&[0u8; 8]).is_ok());
    }
}
