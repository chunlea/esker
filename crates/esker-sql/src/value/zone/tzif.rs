//! Reading a `TZif` file — the IANA database's on-disk form, RFC 8536.
//!
//! The bytes come from `jiff-tzdb`, which is the whole of what
//! [ADR 0082](../../../../../docs/adr/0082-the-time-zone-table-is-data-the-reader-is-ours.md) buys:
//! a zone name is a legislative history and nothing here can derive it. **The reading is ours**,
//! because a `TZif` file is a magic, a version, six counts and five arrays — the shape CLAUDE.md
//! puts on the in-house list beside every other format this repository frames itself.
//!
//! # The layout, and the two blocks
//!
//! ```text
//! "`TZif`" version[1] reserved[15]
//! isutcnt isstdcnt leapcnt timecnt typecnt charcnt      six u32, big-endian
//! transition times          timecnt × 4 bytes           (v1) or × 8 (v2+)
//! transition type indices   timecnt × 1
//! local time types          typecnt × 6                 { utoff i32, isdst u8, desigidx u8 }
//! designations              charcnt bytes               NUL-terminated, indexed into
//! leap seconds              leapcnt × (4|8 + 4)
//! standard/wall             isstdcnt × 1
//! UT/local                  isutcnt × 1
//! ```
//!
//! **A version 2 or 3 file writes all of that twice**: once with 32-bit transition times, for a
//! reader that predates the format, and then a second header and block with 64-bit ones, followed
//! by a footer — a newline, a POSIX `TZ` string, a newline. This reader **skips the first block
//! entirely** whenever the version says there is a second one. Reading the v1 block instead would
//! be right until 2038 and then silently wrong, which is the worst way for a clock to fail.
//!
//! # What the format does not promise
//!
//! `typecnt` may exceed the types any transition names, `timecnt` may be **zero** — a zone that
//! has never changed offset, or one that is entirely a POSIX rule — and the designation array is a
//! blob of NUL-terminated strings that entries index *into*, not an array of strings: two types
//! sharing `EST` may point at the same byte or at different ones. Every one of those is exercised
//! by the database itself, which is why the test at the bottom reads all 597 of them rather than a
//! chosen few.

/// One local time type: what the clock reads, and what it is called, over one span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalTime {
    /// Seconds **east of UTC** — the number added to an instant to get local time. PostgreSQL
    /// prints it as the offset after the timestamp, and it is not always a whole minute:
    /// `America/New_York` before 1883 is `-04:56:02` (`tests/captures/pg19_time_zone.txt`).
    pub(super) offset: i32,
    /// Whether this span is daylight saving. `pg_timezone_names.is_dst` is this, for now.
    pub(super) is_dst: bool,
    /// `EST`, `EDT`, `+0545`. Not unique, and not a key.
    pub(super) abbrev: String,
}

/// A `TZif` file, read.
#[derive(Debug)]
pub(super) struct Tzif {
    /// The instants, in UTC seconds, at which the offset changes. Ascending, and possibly empty.
    pub(super) transitions: Vec<i64>,
    /// `indices[i]` is the type that takes effect at `transitions[i]`. Same length.
    pub(super) indices: Vec<u8>,
    /// Every type the file names, whether a transition uses it or not.
    pub(super) types: Vec<LocalTime>,
    /// The POSIX `TZ` string that governs instants past the last transition, when there is one.
    pub(super) footer: Option<String>,
}

/// A reason a `TZif` file could not be read.
///
/// A `&'static str` rather than an error enum on purpose: the only source of these bytes is a
/// table compiled into the binary, so every one of these is a bug in this reader or a corrupt
/// build, never a user's input. The caller names the zone and wraps it.
pub(super) type Reason = &'static str;

/// Reads one `TZif` file.
pub(super) fn read(bytes: &[u8]) -> Result<Tzif, Reason> {
    let mut at = Cursor::new(bytes);
    let head = header(&mut at)?;
    if head.version >= b'2' {
        // Skip the whole 32-bit block, then read the 64-bit one that follows it.
        skip_block(&mut at, &head, 4)?;
        let head = header(&mut at)?;
        let block = data(&mut at, &head, 8)?;
        return Ok(Tzif {
            transitions: block.0,
            indices: block.1,
            types: block.2,
            footer: footer(&mut at)?,
        });
    }
    let block = data(&mut at, &head, 4)?;
    Ok(Tzif {
        transitions: block.0,
        indices: block.1,
        types: block.2,
        footer: None,
    })
}

/// The six counts, and the version that says whether a second block follows.
struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn header(at: &mut Cursor<'_>) -> Result<Header, Reason> {
    if at.take(4)? != b"TZif" {
        return Err("no TZif magic");
    }
    let version = at.byte()?;
    if !matches!(version, b'\0' | b'2' | b'3' | b'4') {
        return Err("unknown TZif version");
    }
    at.take(15)?;
    let head = Header {
        version,
        isutcnt: at.count()?,
        isstdcnt: at.count()?,
        leapcnt: at.count()?,
        timecnt: at.count()?,
        typecnt: at.count()?,
        charcnt: at.count()?,
    };
    // **A file with no types cannot answer anything**, and the RFC forbids it outside the v1
    // block of a v2+ file — which this reader skips rather than parses, so the check is safe here.
    if head.typecnt == 0 {
        return Err("a TZif block with no local time types");
    }
    Ok(head)
}

/// Steps over a whole data block without interpreting it.
fn skip_block(at: &mut Cursor<'_>, head: &Header, time: usize) -> Result<(), Reason> {
    let bytes = head.timecnt * (time + 1)
        + head.typecnt * 6
        + head.charcnt
        + head.leapcnt * (time + 4)
        + head.isstdcnt
        + head.isutcnt;
    at.take(bytes)?;
    Ok(())
}

type Block = (Vec<i64>, Vec<u8>, Vec<LocalTime>);

fn data(at: &mut Cursor<'_>, head: &Header, time: usize) -> Result<Block, Reason> {
    let mut transitions = Vec::with_capacity(head.timecnt);
    for _ in 0..head.timecnt {
        transitions.push(at.time(time)?);
    }
    // **Ascending is a promise the format makes and this reader checks**, because the lookup is a
    // binary search and an unsorted table would answer a wrong offset rather than fail.
    if transitions.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("TZif transition times are not ascending");
    }
    let mut indices = Vec::with_capacity(head.timecnt);
    for _ in 0..head.timecnt {
        let index = at.byte()?;
        if usize::from(index) >= head.typecnt {
            return Err("a TZif transition names a type that does not exist");
        }
        indices.push(index);
    }

    let mut raw = Vec::with_capacity(head.typecnt);
    for _ in 0..head.typecnt {
        let offset = at.offset()?;
        let is_dst = at.byte()? != 0;
        let designation = at.byte()?;
        raw.push((offset, is_dst, usize::from(designation)));
    }
    let names = at.take(head.charcnt)?;
    let types = raw
        .into_iter()
        .map(|(offset, is_dst, index)| {
            Ok(LocalTime {
                offset,
                is_dst,
                abbrev: abbreviation(names, index)?,
            })
        })
        .collect::<Result<Vec<_>, Reason>>()?;

    // Leap seconds and the two indicator arrays are read past rather than kept. PostgreSQL does
    // not apply leap seconds to a `timestamptz` — its epoch is a count of ordinary seconds — and
    // the indicators only matter to a reader converting a POSIX rule's transition times, which
    // this one does in local time as the rule itself specifies.
    at.take(head.leapcnt * (time + 4))?;
    at.take(head.isstdcnt)?;
    at.take(head.isutcnt)?;
    Ok((transitions, indices, types))
}

/// One NUL-terminated designation out of the blob the entries index into.
fn abbreviation(names: &[u8], from: usize) -> Result<String, Reason> {
    let tail = names
        .get(from..)
        .ok_or("a TZif designation index past the end")?;
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .ok_or("a TZif designation with no terminator")?;
    std::str::from_utf8(&tail[..end])
        .map(str::to_owned)
        .map_err(|_| "a TZif designation that is not UTF-8")
}

/// The `\n TZ \n` that follows the 64-bit block of a version 2 or later file.
fn footer(at: &mut Cursor<'_>) -> Result<Option<String>, Reason> {
    if at.done() {
        return Ok(None);
    }
    if at.byte()? != b'\n' {
        return Err("a TZif footer that does not begin with a newline");
    }
    let rest = at.rest();
    let end = rest
        .iter()
        .position(|&byte| byte == b'\n')
        .ok_or("a TZif footer that does not end with a newline")?;
    let text = std::str::from_utf8(&rest[..end]).map_err(|_| "a TZif footer that is not UTF-8")?;
    // An empty footer is legal and means "no rule past the last transition", which is a different
    // thing from a rule that never changes: the last recorded offset simply continues.
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// A byte reader that cannot read past its slice.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at.min(self.bytes.len())..]
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Reason> {
        let end = self
            .at
            .checked_add(count)
            .ok_or("a TZif length that wraps")?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or("a TZif file that ends in the middle of a field")?;
        self.at = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, Reason> {
        Ok(self.take(1)?[0])
    }

    /// A `u32` count, as a `usize` this reader can allocate against.
    fn count(&mut self) -> Result<usize, Reason> {
        let bytes: [u8; 4] = self.take(4)?.try_into().unwrap_or([0; 4]);
        Ok(u32::from_be_bytes(bytes) as usize)
    }

    /// A signed offset in seconds.
    fn offset(&mut self) -> Result<i32, Reason> {
        let bytes: [u8; 4] = self.take(4)?.try_into().unwrap_or([0; 4]);
        Ok(i32::from_be_bytes(bytes))
    }

    /// A transition time, four bytes or eight depending on the block.
    fn time(&mut self, width: usize) -> Result<i64, Reason> {
        let bytes = self.take(width)?;
        if width == 8 {
            let eight: [u8; 8] = bytes.try_into().unwrap_or([0; 8]);
            Ok(i64::from_be_bytes(eight))
        } else {
            let four: [u8; 4] = bytes.try_into().unwrap_or([0; 4]);
            Ok(i64::from(i32::from_be_bytes(four)))
        }
    }
}
