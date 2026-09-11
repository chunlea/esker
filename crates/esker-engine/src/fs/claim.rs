//! The claim marker: an object that says whose prefix this is.
//!
//! Two databases pointed at one `--sst-store` prefix overwrite each other's `000007.sst`, and
//! they do it in silence: the object key is derived from a file number, and file numbers restart
//! at one in every database. Nothing about the bytes is wrong, nothing errors, and the loser
//! finds out when a read returns another database's block.
//!
//! So a prefix is **claimed**. The first database to open one writes a small object saying who
//! it is; every later open reads that object and refuses a prefix that belongs to somebody else.
//!
//! # What identity is, and why it is not the store id
//!
//! The obvious identity — `(cluster_id, store_id)` — cannot do the job on its own. Two
//! `esker bench` runs have neither, and two stores misconfigured with the same id are exactly
//! the collision worth catching. So the authority is a [`ClaimId`]: eight random bytes drawn
//! once, when a database first claims any prefix, and kept in the database's **own directory**
//! from then on. Same directory, same claim; different database, different claim, whatever the
//! flags say. The cluster and store ids ride along because an operator who has to fix this needs
//! to be told which two databases collided, and a random number tells them nothing.
//!
//! # The on-disk (in-bucket) format
//!
//! Thirty-seven fixed bytes, little-endian, [ADR 0029](../../../../docs/adr/0029-the-sst-store-claim.md):
//!
//! ```text
//! 0   8  magic "ESKERCLM"
//! 8   1  format version (1)
//! 9   8  claim id
//! 17  8  cluster id      informational
//! 25  8  store id        informational
//! 33  4  CRC32C of bytes 0..33
//! ```
//!
//! Fixed-width on purpose: there is no length field to disagree with a buffer, so a truncated or
//! padded object is caught by its length before anything is parsed out of it. Corruption is an
//! error value and never a panic (`CLAUDE.md` invariant 2 and invariant 9).

use std::fmt;
use std::io;
use std::path::Path;

use esker_base::rng::Pcg32;
use esker_base::{crc32c, hash};
use esker_s3::{ObjectStore, PutOutcome};

/// The marker's magic.
pub const CLAIM_MAGIC: [u8; 8] = *b"ESKERCLM";

/// The format version these bytes are written at.
pub const CLAIM_FORMAT_VERSION: u8 = 1;

/// The encoded length of a marker. Fixed: see the module documentation.
pub const CLAIM_LEN: usize = 37;

/// The object name, appended to the key prefix.
///
/// Deliberately not `NNNNNN.sst`-shaped, so [`crate::filename::classify`] reports `None` for it
/// and no sweep on either side ever mistakes it for a file of the engine's to reclaim.
pub const CLAIM_OBJECT: &str = "ESKER-CLAIM";

/// The name of the file that holds a database's claim id inside its own directory.
pub const CLAIM_ID_FILE: &str = "ESKER-CLAIM-ID";

/// Eight bytes that say which database this is, drawn once and then persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimId(pub u64);

impl fmt::Display for ClaimId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Who a database is, as a prefix records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The authority. Two databases are the same database exactly when this matches.
    pub claim: ClaimId,
    /// The cluster this database belongs to, or `0` when it belongs to none. Informational.
    pub cluster_id: u64,
    /// The store this database backs, or `0`. Informational.
    pub store_id: u64,
}

impl Identity {
    /// An identity with no cluster and no store, which is what a benchmark or an embedded
    /// database has.
    #[must_use]
    pub fn new(claim: ClaimId) -> Self {
        Self {
            claim,
            cluster_id: 0,
            store_id: 0,
        }
    }

    /// The same identity, told which cluster and store it belongs to.
    #[must_use]
    pub fn of(mut self, cluster_id: u64, store_id: u64) -> Self {
        self.cluster_id = cluster_id;
        self.store_id = store_id;
        self
    }

    /// The 37 bytes of the marker.
    #[must_use]
    pub fn encode(&self) -> [u8; CLAIM_LEN] {
        let mut out = [0u8; CLAIM_LEN];
        out[0..8].copy_from_slice(&CLAIM_MAGIC);
        out[8] = CLAIM_FORMAT_VERSION;
        out[9..17].copy_from_slice(&self.claim.0.to_le_bytes());
        out[17..25].copy_from_slice(&self.cluster_id.to_le_bytes());
        out[25..33].copy_from_slice(&self.store_id.to_le_bytes());
        let crc = crc32c::checksum(&out[0..33]);
        out[33..37].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Reads a marker, or says exactly what is wrong with it.
    ///
    /// Every failure is an error value. A marker is read from a bucket that anything may have
    /// written into, so it is untrusted input in the sense `CLAUDE.md` invariant 9 means.
    pub fn decode(bytes: &[u8]) -> Result<Self, ClaimError> {
        if bytes.len() != CLAIM_LEN {
            return Err(ClaimError::Malformed(format!(
                "a claim marker is {CLAIM_LEN} bytes; this object is {}",
                bytes.len()
            )));
        }
        if bytes[0..8] != CLAIM_MAGIC {
            return Err(ClaimError::Malformed(
                "the object under this prefix is not a claim marker: wrong magic".into(),
            ));
        }
        // The CRC is checked before the version, so a corrupt version byte is reported as
        // corruption rather than as an unreadable future format.
        let stored = u32::from_le_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);
        let computed = crc32c::checksum(&bytes[0..33]);
        if stored != computed {
            return Err(ClaimError::Malformed(format!(
                "the claim marker's CRC32C is {stored:#010x}, but its bytes hash to {computed:#010x}"
            )));
        }
        if bytes[8] != CLAIM_FORMAT_VERSION {
            return Err(ClaimError::Version(bytes[8]));
        }
        let word = |at: usize| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(buf)
        };
        Ok(Self {
            claim: ClaimId(word(9)),
            cluster_id: word(17),
            store_id: word(25),
        })
    }
}

impl fmt::Display for Identity {
    /// Written for the operator who has to fix the collision, so it names the ids they set as
    /// well as the one they did not.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "database {}", self.claim)?;
        match (self.cluster_id, self.store_id) {
            (0, 0) => Ok(()),
            (cluster, 0) => write!(f, " (cluster {cluster})"),
            (0, store) => write!(f, " (store {store})"),
            (cluster, store) => write!(f, " (cluster {cluster}, store {store})"),
        }
    }
}

/// What can be wrong with a prefix a database was pointed at.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClaimError {
    /// The prefix belongs to a different database. Both identities are named, because the
    /// operator's next question is always "then whose is it?".
    #[error(
        "the SST store prefix {prefix:?} is claimed by {theirs}, and this is {ours}. \
         Two databases sharing a prefix overwrite each other's SSTs. \
         Give this one a prefix of its own, or point it at the prefix that is really its."
    )]
    Claimed {
        /// The prefix that was asked for.
        prefix: String,
        /// Who this database is.
        ours: Box<Identity>,
        /// Who the marker says owns it.
        theirs: Box<Identity>,
    },

    /// The prefix already holds SSTs but no marker: a prefix written before markers existed, or
    /// one whose marker was deleted. Adopting it silently is the one thing that must not happen,
    /// because the objects may be another live database's.
    #[error(
        "the SST store prefix {prefix:?} holds {objects} object(s) but no claim marker, so \
         there is no way to tell whether another database is still using them. If this prefix is \
         this database's, re-run with --adopt-sst-store to claim it; if it is not, give this \
         database a prefix of its own."
    )]
    Unclaimed {
        /// The prefix that was asked for.
        prefix: String,
        /// How many objects are already there.
        objects: usize,
    },

    /// The marker is there and cannot be read. Refused rather than overruled — including under
    /// `adopt_unclaimed`, because a marker that will not parse might still be somebody's.
    #[error("the SST store prefix {prefix:?} carries a claim marker that cannot be read: {why}")]
    Unreadable {
        /// The prefix that was asked for.
        prefix: String,
        /// What is wrong with the bytes.
        why: Box<ClaimError>,
    },

    /// The marker's bytes are not a marker this build understands.
    #[error("{0}")]
    Malformed(String),

    /// The object store itself failed. Carried as a string because a claim's caller reports it
    /// as a startup failure and never branches on it.
    #[error("the object store, while claiming a prefix: {0}")]
    Store(String),

    /// A marker written at a version this build does not know. Refusing is the only safe
    /// answer: a newer format may mean something this build would misread.
    #[error(
        "the claim marker is format version {0}, and this build understands version \
         {CLAIM_FORMAT_VERSION}. A newer esker wrote this prefix."
    )]
    Version(u8),
}

/// Establishes that `prefix` in `store` is this database's, or says why it is not.
///
/// Three outcomes, and the middle one is the whole point:
///
/// * **the marker is ours** — or we have just written it — and the caller may proceed;
/// * **the marker is somebody else's** — [`ClaimError::Claimed`], naming both databases;
/// * **there is no marker.** An empty prefix is claimed. A prefix that already holds objects is
///   [`ClaimError::Unclaimed`] unless `adopt_unclaimed`, because objects with no marker may be a
///   live database's, written before markers existed. Adopting them silently is the corruption
///   this module exists to stop.
///
/// Callers run this **before** learning anything else about the prefix, so a database that is
/// going to be refused never takes on another database's file numbers.
///
/// # Crashing in the middle
///
/// The marker is written before the caller can upload anything, so a crash between the two
/// leaves a marker over an empty prefix: the next open reads its own marker, matches, and
/// carries on. A crash before the marker landed leaves an empty prefix, which the next open
/// claims. Neither needs a recovery path, which is why there is not one.
///
/// # The race, which the store now settles
///
/// Two databases claiming an empty prefix in the same instant are separated by the store itself:
/// the marker is written with [`ObjectStore::put_if_absent`] — `PutObject` with
/// `If-None-Match: *` — so exactly one of them creates it and the other is told
/// [`PutOutcome::AlreadyThere`]. There is no window in which both believe they won, because there
/// is no instant at which both writes take.
///
/// The **read-back stays**, and does two jobs now rather than one. It is what turns
/// `AlreadyThere` into an answer — the marker names its owner, and a retry of our own claim
/// recognises itself instead of refusing — and it is the whole of the safety story on an endpoint
/// that ignores `If-None-Match`, where a conditional put degrades to an unconditional one and the
/// window is the narrowed one [ADR 0029](../../../../docs/adr/0029-the-sst-store-claim.md)
/// originally shipped. Never wider, never silent.
pub fn settle(
    store: &dyn ObjectStore,
    prefix: &str,
    ours: Identity,
    adopt_unclaimed: bool,
) -> Result<(), ClaimError> {
    let key = format!("{prefix}{CLAIM_OBJECT}");

    match store.get(&key) {
        Ok(response) => return verify(prefix, ours, &response.body),
        Err(error) if error.is_not_found() => {}
        Err(error) => return Err(ClaimError::Store(error.to_string())),
    }

    // No marker. An empty prefix is ours for the taking; one with objects in it is not.
    let listed = store
        .list(prefix)
        .map_err(|error| ClaimError::Store(error.to_string()))?
        .into_iter()
        .filter(|object| object.key != key)
        .count();
    if listed > 0 && !adopt_unclaimed {
        return Err(ClaimError::Unclaimed {
            prefix: prefix.to_owned(),
            objects: listed,
        });
    }
    if listed > 0 {
        tracing::warn!(
            prefix,
            objects = listed,
            identity = %ours,
            "adopting an SST store prefix that holds objects but no claim marker, because \
             adoption was asked for explicitly"
        );
    }

    let outcome = store
        .put_if_absent(&key, &ours.encode())
        .map_err(|error| ClaimError::Store(error.to_string()))?;
    // Read back either way. After `AlreadyThere` it is the only thing that can say *whose* marker
    // is there — including the case where it is ours, which is what a retried claim looks like.
    // After `Stored` it is what catches an endpoint that ignored the precondition.
    let written = store
        .get(&key)
        .map_err(|error| ClaimError::Store(error.to_string()))?;
    verify(prefix, ours, &written.body)?;
    match outcome {
        PutOutcome::Stored(_) => {
            tracing::info!(prefix, identity = %ours, "claimed the SST store prefix");
        }
        PutOutcome::AlreadyThere => {
            tracing::info!(
                prefix,
                identity = %ours,
                "the SST store prefix was already claimed, by us"
            );
        }
    }
    Ok(())
}

/// Checks a marker's bytes against who we are.
///
/// A marker that cannot be read is not a marker that can be overruled: it is refused the same
/// way as one naming somebody else, because it might be one.
fn verify(prefix: &str, ours: Identity, body: &[u8]) -> Result<(), ClaimError> {
    // The prefix goes into the message here rather than being left to the caller: an operator
    // reading "the claim marker's CRC32C is ..." needs to be told *which* prefix's marker, and
    // by the time this reaches a log the caller's context is gone.
    let theirs = Identity::decode(body).map_err(|error| ClaimError::Unreadable {
        prefix: prefix.to_owned(),
        why: Box::new(error),
    })?;
    if theirs.claim == ours.claim {
        return Ok(());
    }
    Err(ClaimError::Claimed {
        prefix: prefix.to_owned(),
        ours: Box::new(ours),
        theirs: Box::new(theirs),
    })
}

/// The length of the local claim-id file: the id and a CRC32C over it.
const CLAIM_ID_FILE_LEN: usize = 12;

/// This database's claim id, read from its directory or drawn and written there.
///
/// The id lives in the database's **own** directory, which is what makes "the same database"
/// mean the same database rather than the same command line: two `esker bench` runs share every
/// flag and must still not share a prefix, and two stores misconfigured with one store id are
/// exactly the collision worth catching.
///
/// Written before any claim is made, and fsynced with its directory, so a crash can leave an id
/// with no marker — harmless, the next open claims — but never a marker with no id, which would
/// be a database that cannot recognise its own prefix.
///
/// # If this file is lost
///
/// The database draws a new id and is then refused by its own prefix, which is the safe way to
/// be wrong: the alternative is a database that adopts a prefix on the strength of a file
/// anybody could have deleted. The way back is deliberate and takes two steps — delete the
/// marker object, then re-open with `--adopt-sst-store`, which is the hatch for a prefix that
/// holds objects and no marker ([ADR 0029](../../../../docs/adr/0029-the-sst-store-claim.md)).
pub fn id_for_directory(fs: &dyn super::FileSystem, dir: &Path) -> io::Result<ClaimId> {
    let path = dir.join(CLAIM_ID_FILE);
    if fs.exists(&path)? {
        let file = fs.open(&path)?;
        let mut bytes = [0u8; CLAIM_ID_FILE_LEN];
        super::read_exact_at(file.as_ref(), 0, &mut bytes)?;
        let stored = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if stored != crc32c::checksum(&bytes[0..8]) {
            // Not regenerated: a fresh id here would silently make this database a different
            // database, which is the exact confusion the file exists to prevent.
            return Err(io::Error::other(format!(
                "{}: the claim id is corrupt. Deleting it draws a new identity, which this \
                 database's SST store prefix will then refuse; see ADR 0029.",
                path.display()
            )));
        }
        let mut id = [0u8; 8];
        id.copy_from_slice(&bytes[0..8]);
        return Ok(ClaimId(u64::from_le_bytes(id)));
    }

    let id = ClaimId(draw_id(dir));
    let mut bytes = [0u8; CLAIM_ID_FILE_LEN];
    bytes[0..8].copy_from_slice(&id.0.to_le_bytes());
    bytes[8..12].copy_from_slice(&crc32c::checksum(&id.0.to_le_bytes()).to_le_bytes());

    // Whole-file write, then fsync the file and its directory: this is the one local record
    // that a claim depends on (`CLAUDE.md` invariant 1).
    let mut file = fs.create(&path)?;
    file.append(&bytes)?;
    file.sync_data()?;
    drop(file);
    fs.fsync_dir(dir)?;
    Ok(id)
}

/// Draws an id with enough entropy that two databases created in the same second on one machine
/// do not collide.
///
/// Not a cryptographic identity and not trying to be: it distinguishes databases, it does not
/// authenticate them. The clock, the process and the path are independent enough for that, and
/// they need no dependency (`CLAUDE.md`, dependency policy).
fn draw_id(dir: &Path) -> u64 {
    // Seconds and sub-second nanoseconds separately, so nothing is cast from `u128` and the
    // whole of the clock's resolution survives.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            since
                .as_secs()
                .wrapping_mul(1_000_000_000)
                .wrapping_add(u64::from(since.subsec_nanos()))
        });
    let seed = hash::mix64(nanos)
        ^ hash::mix64(u64::from(std::process::id()))
        ^ hash::hash64(dir.to_string_lossy().as_bytes());
    // One more round through the generator, so two seeds differing in one bit do not produce
    // two ids differing in one bit.
    let mut rng = Pcg32::from_seed(seed);
    let drawn = rng.next_u64();
    // Zero is reserved for "no id", so a marker of all zeroes can never look like a claim.
    if drawn == 0 { 1 } else { drawn }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{CLAIM_FORMAT_VERSION, CLAIM_LEN, CLAIM_MAGIC, ClaimError, ClaimId, Identity};

    fn sample() -> Identity {
        Identity::new(ClaimId(0x0123_4567_89ab_cdef)).of(7, 3)
    }

    /// **The golden test.** These are the bytes on the wire and in the bucket; a change to them
    /// is a format change, and a format change needs an ADR and a version bump, not an edit to
    /// this array (`CLAUDE.md`, "Ask before doing").
    #[test]
    fn the_marker_bytes_are_exactly_these() {
        let encoded = sample().encode();
        assert_eq!(
            encoded,
            [
                // "ESKERCLM"
                0x45, 0x53, 0x4b, 0x45, 0x52, 0x43, 0x4c, 0x4d, //
                0x01, // format version
                0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01, // claim id, little-endian
                0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // cluster 7
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // store 3
                0x83, 0x4c, 0xb0, 0xb8, // CRC32C of the 33 bytes above
            ]
        );
        assert_eq!(encoded.len(), CLAIM_LEN);
        assert_eq!(&encoded[0..8], &CLAIM_MAGIC);
        assert_eq!(encoded[8], CLAIM_FORMAT_VERSION);
    }

    #[test]
    fn a_marker_round_trips() {
        let identity = sample();
        assert_eq!(Identity::decode(&identity.encode()).unwrap(), identity);
        // And one with no cluster and no store, which is what a benchmark writes.
        let bare = Identity::new(ClaimId(1));
        assert_eq!(Identity::decode(&bare.encode()).unwrap(), bare);
    }

    /// Every byte matters: flipping any one of them must be caught rather than believed.
    #[test]
    fn a_single_flipped_bit_anywhere_is_refused() {
        let good = sample().encode();
        for byte in 0..CLAIM_LEN {
            for bit in 0..8 {
                let mut bad = good;
                bad[byte] ^= 1 << bit;
                assert!(
                    Identity::decode(&bad).is_err(),
                    "a flip at byte {byte} bit {bit} was accepted"
                );
            }
        }
    }

    #[test]
    fn a_truncated_or_padded_object_is_refused_by_its_length() {
        let good = sample().encode();
        assert!(matches!(
            Identity::decode(&good[..CLAIM_LEN - 1]),
            Err(ClaimError::Malformed(_))
        ));
        let mut long = good.to_vec();
        long.push(0);
        assert!(matches!(
            Identity::decode(&long),
            Err(ClaimError::Malformed(_))
        ));
        assert!(matches!(
            Identity::decode(&[]),
            Err(ClaimError::Malformed(_))
        ));
    }

    /// A future version is refused *as a version*, so the message tells an operator to upgrade
    /// rather than sending them looking for corruption.
    #[test]
    fn a_newer_format_version_is_named_rather_than_guessed_at() {
        let mut bytes = sample().encode();
        bytes[8] = CLAIM_FORMAT_VERSION + 1;
        // Re-CRC, so this is a well-formed marker of a version we do not know.
        let crc = esker_base::crc32c::checksum(&bytes[0..33]);
        bytes[33..37].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            Identity::decode(&bytes),
            Err(ClaimError::Version(CLAIM_FORMAT_VERSION + 1))
        );
    }

    /// Something else entirely under the prefix is not reported as corruption: the operator's
    /// problem is a prefix that is not ours, not a bad checksum.
    #[test]
    fn a_foreign_object_is_reported_as_not_a_marker() {
        let error = Identity::decode(&[0u8; CLAIM_LEN]).unwrap_err();
        assert!(format!("{error}").contains("not a claim marker"), "{error}");
    }

    /// The refusal has to name **both** databases, or the operator cannot act on it.
    #[test]
    fn a_refusal_names_both_databases() {
        let ours = Identity::new(ClaimId(0xaaaa)).of(1, 2);
        let theirs = Identity::new(ClaimId(0xbbbb)).of(9, 8);
        let error = ClaimError::Claimed {
            prefix: "s3://esker/shared/".into(),
            ours: Box::new(ours),
            theirs: Box::new(theirs),
        };
        let text = format!("{error}");
        assert!(text.contains("000000000000aaaa"), "{text}");
        assert!(text.contains("000000000000bbbb"), "{text}");
        assert!(text.contains("cluster 1, store 2"), "{text}");
        assert!(text.contains("cluster 9, store 8"), "{text}");
        assert!(text.contains("s3://esker/shared/"), "{text}");
    }

    /// A benchmark has neither id, and its message must not read like it has store 0.
    #[test]
    fn an_identity_with_no_cluster_or_store_says_only_what_it_knows() {
        assert_eq!(
            format!("{}", Identity::new(ClaimId(0xfe))),
            "database 00000000000000fe"
        );
    }
}
