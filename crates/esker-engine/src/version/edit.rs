//! `VersionEdit`: the delta the manifest is a log of.
//!
//! The manifest never stores the state of the database. It stores the *changes* — this file
//! was added at that level, that one was deleted, the log number moved forward — and the
//! current state is what you get by replaying them in order (`docs/DESIGN.md` §4.6). A flush
//! or a compaction appends one edit and is done; nothing rewrites a snapshot of the whole
//! file list, which is what would make a large database slow to modify and dangerous to
//! interrupt.
//!
//! ```text
//! edit  = field*
//! field = tag:varint ++ payload
//! ```
//!
//! Every field is optional and repeated fields are allowed, so an edit that touches one thing
//! costs a handful of bytes. An **unknown tag is an error**, not a field to skip: an edit
//! written by a newer format cannot be applied half-way, and pretending otherwise would leave
//! the version silently missing whatever the new tag described.

use esker_base::varint;

use crate::dbformat::SeqNo;
use crate::error::{Error, Result};

/// Field tags. On disk, and therefore frozen.
mod tag {
    /// The user comparator's name; recorded once, checked on every reopen.
    pub(super) const COMPARATOR: u32 = 1;
    /// The oldest write-ahead-log segment that still has to be replayed.
    pub(super) const LOG_NUMBER: u32 = 2;
    /// The next file number to hand out.
    pub(super) const NEXT_FILE_NUMBER: u32 = 3;
    /// The highest sequence number durably assigned.
    pub(super) const LAST_SEQNO: u32 = 4;
    /// A column family was created: id, then name.
    pub(super) const CF_ADD: u32 = 5;
    /// A column family was dropped: id.
    pub(super) const CF_DROP: u32 = 6;
    /// A file left a level: cf, level, file number.
    pub(super) const DELETE_FILE: u32 = 7;
    /// A file joined a level: cf, level, then the metadata below.
    pub(super) const ADD_FILE: u32 = 8;
}

/// What the version set knows about one SST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// The file number, which names the file and identifies it in the block cache.
    pub number: u64,
    /// Size on disk, used to score compactions.
    pub size: u64,
    /// The smallest internal key in the file.
    pub smallest: Vec<u8>,
    /// The largest internal key in the file.
    pub largest: Vec<u8>,
    /// The smallest sequence number in the file.
    pub smallest_seqno: SeqNo,
    /// The largest sequence number in the file.
    pub largest_seqno: SeqNo,
}

/// One atomic change to the set of live files and the numbers that describe the database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionEdit {
    /// Set once, when the database is created.
    pub comparator: Option<String>,
    /// Segments below this number have been flushed and need no replay.
    pub log_number: Option<u64>,
    /// The next unused file number.
    pub next_file_number: Option<u64>,
    /// The highest sequence number this edit makes durable.
    pub last_seqno: Option<SeqNo>,
    /// Column families created by this edit: `(id, name)`.
    pub cf_added: Vec<(u32, String)>,
    /// Column families dropped by this edit.
    pub cf_dropped: Vec<u32>,
    /// Files removed: `(cf, level, file number)`.
    pub deleted_files: Vec<(u32, u32, u64)>,
    /// Files added: `(cf, level, metadata)`.
    pub added_files: Vec<(u32, u32, FileMeta)>,
}

impl VersionEdit {
    /// An edit that changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this edit would change anything at all.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Records a file entering a level.
    pub fn add_file(&mut self, cf: u32, level: u32, meta: FileMeta) {
        self.added_files.push((cf, level, meta));
    }

    /// Records a file leaving a level.
    pub fn delete_file(&mut self, cf: u32, level: u32, number: u64) {
        self.deleted_files.push((cf, level, number));
    }

    /// Serialises the edit. One of these is the payload of one manifest record.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(name) = &self.comparator {
            put_tag(tag::COMPARATOR, &mut out);
            put_bytes(name.as_bytes(), &mut out);
        }
        for (field, value) in [
            (tag::LOG_NUMBER, self.log_number),
            (tag::NEXT_FILE_NUMBER, self.next_file_number),
            (tag::LAST_SEQNO, self.last_seqno),
        ] {
            if let Some(value) = value {
                put_tag(field, &mut out);
                varint::put_u64(value, &mut out);
            }
        }
        for (cf, name) in &self.cf_added {
            put_tag(tag::CF_ADD, &mut out);
            varint::put_u32(*cf, &mut out);
            put_bytes(name.as_bytes(), &mut out);
        }
        for cf in &self.cf_dropped {
            put_tag(tag::CF_DROP, &mut out);
            varint::put_u32(*cf, &mut out);
        }
        for (cf, level, number) in &self.deleted_files {
            put_tag(tag::DELETE_FILE, &mut out);
            varint::put_u32(*cf, &mut out);
            varint::put_u32(*level, &mut out);
            varint::put_u64(*number, &mut out);
        }
        for (cf, level, meta) in &self.added_files {
            put_tag(tag::ADD_FILE, &mut out);
            varint::put_u32(*cf, &mut out);
            varint::put_u32(*level, &mut out);
            varint::put_u64(meta.number, &mut out);
            varint::put_u64(meta.size, &mut out);
            put_bytes(&meta.smallest, &mut out);
            put_bytes(&meta.largest, &mut out);
            varint::put_u64(meta.smallest_seqno, &mut out);
            varint::put_u64(meta.largest_seqno, &mut out);
        }
        out
    }

    /// Parses one edit, rejecting anything it cannot fully understand.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let mut edit = Self::new();
        while !cursor.is_empty() {
            let field = cursor.u32("tag")?;
            match field {
                tag::COMPARATOR => edit.comparator = Some(cursor.string("comparator name")?),
                tag::LOG_NUMBER => edit.log_number = Some(cursor.u64("log number")?),
                tag::NEXT_FILE_NUMBER => {
                    edit.next_file_number = Some(cursor.u64("next file number")?);
                }
                tag::LAST_SEQNO => edit.last_seqno = Some(cursor.u64("last seqno")?),
                tag::CF_ADD => {
                    let cf = cursor.u32("column family id")?;
                    edit.cf_added
                        .push((cf, cursor.string("column family name")?));
                }
                tag::CF_DROP => edit.cf_dropped.push(cursor.u32("column family id")?),
                tag::DELETE_FILE => {
                    let cf = cursor.u32("column family id")?;
                    let level = cursor.u32("level")?;
                    edit.deleted_files
                        .push((cf, level, cursor.u64("file number")?));
                }
                tag::ADD_FILE => {
                    let cf = cursor.u32("column family id")?;
                    let level = cursor.u32("level")?;
                    let meta = FileMeta {
                        number: cursor.u64("file number")?,
                        size: cursor.u64("file size")?,
                        smallest: cursor.bytes("smallest key")?.to_vec(),
                        largest: cursor.bytes("largest key")?.to_vec(),
                        smallest_seqno: cursor.u64("smallest seqno")?,
                        largest_seqno: cursor.u64("largest seqno")?,
                    };
                    edit.added_files.push((cf, level, meta));
                }
                unknown => {
                    return Err(Error::corruption(
                        "version edit",
                        format!("unknown field tag {unknown} at byte {}", cursor.consumed()),
                    ));
                }
            }
        }
        Ok(edit)
    }
}

fn put_tag(tag: u32, out: &mut Vec<u8>) {
    varint::put_u32(tag, out);
}

fn put_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    varint::put_u64(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

/// A reading position in an encoded edit, with the field name for every error message.
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn consumed(&self) -> usize {
        self.offset
    }

    fn u64(&mut self, what: &str) -> Result<u64> {
        let (value, used) = varint::get_u64(&self.bytes[self.offset..]).map_err(|err| {
            Error::corruption(
                "version edit",
                format!("{what} at byte {}: {err}", self.offset),
            )
        })?;
        self.offset += used;
        Ok(value)
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        let (value, used) = varint::get_u32(&self.bytes[self.offset..]).map_err(|err| {
            Error::corruption(
                "version edit",
                format!("{what} at byte {}: {err}", self.offset),
            )
        })?;
        self.offset += used;
        Ok(value)
    }

    fn bytes(&mut self, what: &str) -> Result<&'a [u8]> {
        let start = self.offset;
        let len = usize::try_from(self.u64(what)?).map_err(|_| {
            Error::corruption(
                "version edit",
                format!("{what} at byte {start} is impossibly long"),
            )
        })?;
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len());
        let Some(end) = end else {
            return Err(Error::corruption(
                "version edit",
                format!("{what} at byte {start} claims {len} bytes, past the end of the edit"),
            ));
        };
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn string(&mut self, what: &str) -> Result<String> {
        let start = self.offset;
        let bytes = self.bytes(what)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            Error::corruption(
                "version edit",
                format!("{what} at byte {start} is not UTF-8"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{FileMeta, VersionEdit};

    fn meta(number: u64) -> FileMeta {
        FileMeta {
            number,
            size: 4096 * number,
            smallest: vec![1, 2, 3, 0, 0, 0, 0, 0, 0, 0],
            largest: vec![9, 9, 1, 0, 0, 0, 0, 0, 0, 0],
            smallest_seqno: 10,
            largest_seqno: 20,
        }
    }

    #[test]
    fn an_empty_edit_encodes_to_nothing() {
        let edit = VersionEdit::new();
        assert!(edit.is_empty());
        assert!(edit.encode().is_empty());
        assert_eq!(VersionEdit::decode(&[]).unwrap(), edit);
    }

    #[test]
    fn every_field_round_trips() {
        let mut edit = VersionEdit::new();
        edit.comparator = Some("esker.BytewiseComparator".into());
        edit.log_number = Some(11);
        edit.next_file_number = Some(12);
        edit.last_seqno = Some(0x00FF_FFFF_FFFF_FFFF);
        edit.cf_added.push((0, "default".into()));
        edit.cf_added.push((7, "write".into()));
        edit.cf_dropped.push(3);
        edit.delete_file(0, 1, 5);
        edit.delete_file(7, 0, 6);
        edit.add_file(0, 0, meta(8));
        edit.add_file(7, 6, meta(9));

        let decoded = VersionEdit::decode(&edit.encode()).unwrap();
        assert_eq!(decoded, edit);
        assert!(!decoded.is_empty());
    }

    /// An edit from a newer format must not be applied half-way: whatever the new tag meant,
    /// skipping it leaves the version quietly wrong.
    #[test]
    fn an_unknown_tag_is_corruption() {
        let mut bytes = VersionEdit::new().encode();
        bytes.push(99);
        let err = VersionEdit::decode(&bytes).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("unknown field tag 99"), "{err}");
    }

    #[test]
    fn a_truncated_edit_is_corruption_not_a_panic() {
        let mut edit = VersionEdit::new();
        edit.comparator = Some("esker.BytewiseComparator".into());
        edit.add_file(1, 2, meta(3));
        let bytes = edit.encode();
        for cut in 0..bytes.len() {
            let result = VersionEdit::decode(&bytes[..cut]);
            if let Ok(partial) = result {
                // A prefix that happens to end on a field boundary is a valid smaller edit.
                assert!(partial != edit, "a truncated edit decoded as the whole one");
            }
        }
    }

    #[test]
    fn a_length_past_the_end_is_corruption() {
        let mut edit = VersionEdit::new();
        edit.comparator = Some("abc".into());
        let mut bytes = edit.encode();
        bytes[1] = 0x7F; // the comparator name's length
        let err = VersionEdit::decode(&bytes).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("past the end"), "{err}");
    }

    #[test]
    fn a_non_utf8_comparator_name_is_corruption() {
        let mut bytes = Vec::new();
        esker_base::varint::put_u32(1, &mut bytes); // COMPARATOR
        esker_base::varint::put_u64(2, &mut bytes);
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        let err = VersionEdit::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("not UTF-8"), "{err}");
    }
}
