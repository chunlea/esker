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
    /// A file's bytes moved between tiers: cf, level, file number, location.
    ///
    /// Added in phase 6b. Emitted **only** when the location is not [`super::FileLocation::Local`],
    /// so a database that never tiers writes the same bytes it wrote in phase 5 and every
    /// golden manifest still passes. An older binary reading a tiered manifest rejects it,
    /// which is the correct outcome: it cannot fetch those files
    /// ([ADR 0024](../../../../docs/adr/0024-tiering-failure-semantics.md)).
    pub(super) const FILE_LOCATION: u32 = 9;
}

/// Where an SST's bytes are known to be.
///
/// This is a **record of what has happened, not a decision about where to look**. The read
/// path opens the local file if it is there and fetches the object if it is not, in that order,
/// whatever this says (ADR 0024 decision 4). What it is for is the two questions that cannot be
/// answered from the filesystem: which local files are safe to evict, and which objects the
/// obsolete-file sweep may delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileLocation {
    /// The bytes are on local disk, and may or may not also be in the object store.
    ///
    /// The default, and what every file is at the moment its `ADD_FILE` is written: uploads
    /// happen after the edit, never before it (ADR 0024 decision 1).
    #[default]
    Local,
    /// The bytes are in the object store, and the local copy is therefore evictable.
    Tiered,
}

impl FileLocation {
    /// The on-disk discriminant. Frozen.
    fn as_u32(self) -> u32 {
        match self {
            Self::Local => 0,
            Self::Tiered => 1,
        }
    }

    /// The inverse, rejecting anything a future format might add rather than guessing.
    fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Local),
            1 => Some(Self::Tiered),
            _ => None,
        }
    }
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
    /// Where the bytes are.
    ///
    /// Not encoded inside `ADD_FILE`: it travels as its own `FILE_LOCATION` record, because a
    /// file is promoted after the edit that adds it and a promotion should cost four varints
    /// rather than a second copy of every key bound.
    pub location: FileLocation,
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
    /// Files whose location changed: `(cf, level, file number, location)`.
    ///
    /// Almost always a *promotion* of a file some earlier edit added, because an upload
    /// finishes after the edit that names the file. [`VersionEdit::add_file`] also fills this
    /// when handed a meta that is already tiered, so that no location can be set on a struct
    /// and silently lost on the way to disk.
    pub file_locations: Vec<(u32, u32, u64, FileLocation)>,
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
        // `ADD_FILE` has no location field, so a non-default location has to travel as its own
        // record or it would round-trip to `Local` and the file would look un-evictable
        // forever. Ordinarily this is dead: a file is `Local` when its add is written.
        if meta.location != FileLocation::Local {
            self.file_locations
                .push((cf, level, meta.number, meta.location));
        }
        self.added_files.push((cf, level, meta));
    }

    /// Records that a file's bytes are now somewhere else — the promote edit an upload writes.
    pub fn set_location(&mut self, cf: u32, level: u32, number: u64, location: FileLocation) {
        self.file_locations.push((cf, level, number, location));
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
        for (cf, level, number, location) in &self.file_locations {
            put_tag(tag::FILE_LOCATION, &mut out);
            varint::put_u32(*cf, &mut out);
            varint::put_u32(*level, &mut out);
            varint::put_u64(*number, &mut out);
            varint::put_u32(location.as_u32(), &mut out);
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
                        location: FileLocation::Local,
                    };
                    edit.added_files.push((cf, level, meta));
                }
                tag::FILE_LOCATION => {
                    let cf = cursor.u32("column family id")?;
                    let level = cursor.u32("level")?;
                    let number = cursor.u64("file number")?;
                    let raw = cursor.u32("file location")?;
                    let location = FileLocation::from_u32(raw).ok_or_else(|| {
                        Error::corruption(
                            "version edit",
                            format!("file location {raw}, which this build does not know"),
                        )
                    })?;
                    // A location for a file this same edit adds belongs on that meta too, or
                    // the edit would not round-trip through encode/decode.
                    for (add_cf, add_level, meta) in &mut edit.added_files {
                        if (*add_cf, *add_level, meta.number) == (cf, level, number) {
                            meta.location = location;
                        }
                    }
                    edit.file_locations.push((cf, level, number, location));
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
    use super::{FileLocation, FileMeta, VersionEdit, tag};
    use esker_base::varint;

    fn meta(number: u64) -> FileMeta {
        FileMeta {
            number,
            size: 4096 * number,
            smallest: vec![1, 2, 3, 0, 0, 0, 0, 0, 0, 0],
            largest: vec![9, 9, 1, 0, 0, 0, 0, 0, 0, 0],
            smallest_seqno: 10,
            largest_seqno: 20,
            location: FileLocation::Local,
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

    /// The format change is **additive**: an edit that tiers nothing encodes to exactly the
    /// bytes it encoded to before tag 9 existed. This is the test that says the phase-5 golden
    /// manifests are still valid, and it would fail the moment somebody made the location a
    /// field of `ADD_FILE` instead of a record of its own.
    #[test]
    fn an_untiered_edit_encodes_to_the_bytes_it_always_did() {
        let mut edit = VersionEdit::new();
        edit.log_number = Some(11);
        edit.add_file(0, 0, meta(8));
        let bytes = edit.encode();

        // Byte for byte against an encoder that does not know tag 9 exists. Scanning for the
        // tag byte instead would prove nothing: 9 is a perfectly ordinary key length.
        let mut expected = Vec::new();
        varint::put_u32(tag::LOG_NUMBER, &mut expected);
        varint::put_u64(11, &mut expected);
        varint::put_u32(tag::ADD_FILE, &mut expected);
        varint::put_u32(0, &mut expected);
        varint::put_u32(0, &mut expected);
        let file = meta(8);
        varint::put_u64(file.number, &mut expected);
        varint::put_u64(file.size, &mut expected);
        varint::put_u64(file.smallest.len() as u64, &mut expected);
        expected.extend_from_slice(&file.smallest);
        varint::put_u64(file.largest.len() as u64, &mut expected);
        expected.extend_from_slice(&file.largest);
        varint::put_u64(file.smallest_seqno, &mut expected);
        varint::put_u64(file.largest_seqno, &mut expected);
        assert_eq!(bytes, expected);
    }

    /// A promotion is four varints and does not repeat the file's metadata, which is the whole
    /// reason it is a separate tag: an upload finishing should not cost a second copy of every
    /// key bound in the file.
    #[test]
    fn a_promotion_round_trips_and_is_small() {
        let mut edit = VersionEdit::new();
        edit.set_location(7, 2, 4242, FileLocation::Tiered);
        let bytes = edit.encode();
        assert!(bytes.len() < 12, "a promotion cost {} bytes", bytes.len());
        let decoded = VersionEdit::decode(&bytes).unwrap();
        assert_eq!(decoded, edit);
        assert_eq!(decoded.file_locations, [(7, 2, 4242, FileLocation::Tiered)]);
    }

    /// A location set on a meta that is added in the same edit must survive the round trip.
    /// `ADD_FILE` has no location field, so without `add_file` also emitting the record the
    /// location would decode back to `Local` and the file would look un-evictable forever.
    #[test]
    fn a_location_on_an_added_file_is_not_lost() {
        let mut tiered = meta(9);
        tiered.location = FileLocation::Tiered;
        let mut edit = VersionEdit::new();
        edit.add_file(1, 3, tiered);

        let decoded = VersionEdit::decode(&edit.encode()).unwrap();
        assert_eq!(decoded, edit);
        assert_eq!(decoded.added_files[0].2.location, FileLocation::Tiered);
        assert_eq!(decoded.file_locations, [(1, 3, 9, FileLocation::Tiered)]);
    }

    /// A location this build does not know is corruption, for the same reason an unknown tag
    /// is: whatever it meant, treating it as `Local` would let the governor evict the only copy
    /// of a file whose bytes are somewhere we cannot name.
    #[test]
    fn an_unknown_location_is_corruption() {
        let mut bytes = Vec::new();
        varint::put_u32(tag::FILE_LOCATION, &mut bytes);
        varint::put_u32(0, &mut bytes);
        varint::put_u32(0, &mut bytes);
        varint::put_u64(1, &mut bytes);
        varint::put_u32(7, &mut bytes);
        let err = VersionEdit::decode(&bytes).unwrap_err();
        assert!(err.is_corruption(), "{err}");
        assert!(err.to_string().contains("file location 7"), "{err}");
    }

    /// A truncated promotion is corruption, not a partially applied one.
    #[test]
    fn a_truncated_promotion_is_corruption() {
        let mut edit = VersionEdit::new();
        edit.set_location(0, 0, 5, FileLocation::Tiered);
        let bytes = edit.encode();
        for cut in 1..bytes.len() {
            assert!(
                VersionEdit::decode(&bytes[..cut]).is_err(),
                "a {cut}-byte prefix of a promotion decoded"
            );
        }
    }

    #[test]
    fn a_non_utf8_comparator_name_is_corruption() {
        let mut bytes = Vec::new();
        varint::put_u32(1, &mut bytes); // COMPARATOR
        varint::put_u64(2, &mut bytes);
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        let err = VersionEdit::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("not UTF-8"), "{err}");
    }
}
