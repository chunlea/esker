//! Reading a table: footer, then index, then one data block per lookup.
//!
//! # Opening
//!
//! `size - 48` gives the footer, the footer gives the properties, the filter and the index,
//! and those three are read once and kept for the life of the reader. Nothing else is read
//! until someone asks for a key.
//!
//! # Two properties are checked, not just reported
//!
//! * **Comparator.** A table sorted by one order and searched with another does not error, it
//!   silently misses keys. [`TableReader::open`] refuses the table instead.
//! * **Prefix extractor.** A filter built over prefixes and probed with whole keys — or the
//!   reverse — reports present keys as absent. On any mismatch the filter is *ignored*, and
//!   the reader falls back to the index. Slower, still correct; the alternative is lost data.
//!
//! # Every block is checked before it is used
//!
//! [`decode_block`] verifies the CRC over the payload and the compression byte before
//! anything is decompressed or parsed, and a handle that points outside the file is rejected
//! before a byte is read. Corruption is a returned error at every step (invariant 2).
//!
//! # `get`
//!
//! Bloom first — it is the only step that can answer without touching the disk — then the
//! index, then exactly one data block. The block cache is consulted by
//! `(file_number, block_offset)`, and holds *uncompressed* blocks, so a hit skips both the
//! read and the decompression.
//!
//! [`decode_block`]: super::footer::decode_block

use std::sync::Arc;

use crate::cache_api::{BlockCache, CacheKey};
use crate::dbformat::Comparator;
use crate::error::{Error, Result};
use crate::format::SST_FOOTER_SIZE;
use crate::fs::{RandomAccessFile, read_exact_at};
use crate::range_del::RangeTombstones;

use super::block::{Block, BlockIter};
use super::builder::TableOptions;
use super::filter::{BloomFilter, filter_key};
use super::footer::{BlockHandle, Footer, decode_block};
use super::props::TableProperties;

/// Everything an open table holds, behind one [`Arc`] so that an iterator can own a share of
/// it rather than borrowing the reader.
struct TableInner {
    file: Box<dyn RandomAccessFile>,
    file_size: u64,
    /// Names this table in error messages and in every cache key it uses.
    file_number: u64,
    context: String,
    options: TableOptions,
    cache: Option<Arc<dyn BlockCache>>,
    index: Block,
    /// `None` when the table has no filter, or when its filter was built over different bytes
    /// than this reader would probe with.
    filter: Option<BloomFilter>,
    /// The ranges this table declares deleted; empty for nearly every table.
    range_tombstones: RangeTombstones,
    props: TableProperties,
}

impl std::fmt::Debug for TableInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Non-exhaustive: the file is a handle, not a value, and `RandomAccessFile` is not
        // `Debug`; the index block is bytes nobody wants printed.
        f.debug_struct("Table")
            .field("file", &self.context)
            .field("file_size", &self.file_size)
            .field("entries", &self.props.entry_count)
            .field("has_filter", &self.filter.is_some())
            .finish_non_exhaustive()
    }
}

impl TableInner {
    /// The pieces `read_block` actually needs, borrowed. Split out so that
    /// [`TableReader::open`] can read the properties, filter and index *before* it has a
    /// `TableInner` to call a method on.
    fn source(&self) -> BlockSource<'_> {
        BlockSource {
            file: self.file.as_ref(),
            file_size: self.file_size,
            file_number: self.file_number,
            context: &self.context,
            cache: self.cache.as_ref(),
        }
    }

    fn read_block(&self, handle: BlockHandle, fill_cache: bool) -> Result<Arc<[u8]>> {
        self.source().read(handle, fill_cache)
    }

    /// Reads the block a handle names and parses it as a block.
    fn read_parsed_block(&self, handle: BlockHandle, fill_cache: bool) -> Result<Block> {
        Block::new(self.read_block(handle, fill_cache)?)
    }

    fn comparator(&self) -> Arc<dyn Comparator> {
        Arc::clone(&self.options.comparator)
    }
}

/// Where blocks come from: the file, or the cache in front of it.
struct BlockSource<'a> {
    file: &'a dyn RandomAccessFile,
    file_size: u64,
    file_number: u64,
    context: &'a str,
    cache: Option<&'a Arc<dyn BlockCache>>,
}

impl BlockSource<'_> {
    /// Fetches one block, from the cache if it is there and from the file if it is not.
    ///
    /// What goes into the cache is the *uncompressed* payload, charged at its own length, so
    /// a hit skips the read and the decompression together.
    fn read(&self, handle: BlockHandle, fill_cache: bool) -> Result<Arc<[u8]>> {
        let key = CacheKey::new(self.file_number, handle.offset);
        if let Some(bytes) = self.cache.and_then(|cache| cache.lookup(&key)) {
            return Ok(bytes);
        }

        // Bound the read by the file before allocating for it: a corrupt handle must not turn
        // into an allocation request.
        let end = handle
            .offset
            .checked_add(handle.total_len())
            .filter(|end| *end <= self.file_size)
            .ok_or_else(|| {
                Error::corruption(
                    self.context,
                    format!(
                        "block handle {{offset: {}, size: {}}} does not fit in {} bytes",
                        handle.offset, handle.size, self.file_size
                    ),
                )
            })?;
        let len = usize::try_from(end - handle.offset).map_err(|_| {
            Error::corruption(
                self.context,
                format!("block of {} bytes", handle.total_len()),
            )
        })?;

        let mut raw = vec![0u8; len];
        read_exact_at(self.file, handle.offset, &mut raw)
            .map_err(|e| Error::io(self.context, e))?;
        let payload = decode_block(&raw, self.context)?;
        let bytes: Arc<[u8]> = Arc::from(payload.into_boxed_slice());

        if fill_cache {
            if let Some(cache) = self.cache {
                cache.insert(key, Arc::clone(&bytes), bytes.len());
            }
        }
        Ok(bytes)
    }

    fn read_parsed(&self, handle: BlockHandle, fill_cache: bool) -> Result<Block> {
        Block::new(self.read(handle, fill_cache)?)
    }
}

/// An open sorted string table.
///
/// Cheap to clone: every clone shares one open file, one parsed index and one filter.
#[derive(Debug, Clone)]
pub struct TableReader {
    inner: Arc<TableInner>,
}

impl TableReader {
    /// Opens `file` as a table numbered `file_number`.
    ///
    /// Reads the footer, the properties, the filter and the index — four small reads — and
    /// checks that the file was built the way `options` says it should be read. The index and
    /// filter bypass the block cache: they are held for the life of the reader, so caching
    /// them too would only evict data blocks.
    pub fn open(
        file: Box<dyn RandomAccessFile>,
        file_number: u64,
        options: TableOptions,
        cache: Option<Arc<dyn BlockCache>>,
    ) -> Result<Self> {
        let context = format!("{file_number:06}.sst");
        let file_size = file.size().map_err(|e| Error::io(&context, e))?;
        if file_size < SST_FOOTER_SIZE as u64 {
            return Err(Error::corruption(
                context,
                format!(
                    "file of {file_size} bytes is smaller than a {SST_FOOTER_SIZE}-byte footer"
                ),
            ));
        }

        let mut footer_bytes = [0u8; SST_FOOTER_SIZE];
        read_exact_at(
            file.as_ref(),
            file_size - SST_FOOTER_SIZE as u64,
            &mut footer_bytes,
        )
        .map_err(|e| Error::io(&context, e))?;
        let footer = Footer::decode(&footer_bytes)?;

        let source = BlockSource {
            file: file.as_ref(),
            file_size,
            file_number,
            context: &context,
            cache: cache.as_ref(),
        };

        // The index, filter and properties bypass the cache: they are held for the life of the
        // reader, so caching them too would only evict data blocks.
        let props = TableProperties::decode(source.read(footer.properties, false)?)?;
        let expected = options.comparator.name();
        if props.comparator_name != expected {
            return Err(Error::InvalidArgument(format!(
                "{context}: table was built with comparator {:?} but is being read with \
                 {expected:?}; its keys would be searched in the wrong order",
                props.comparator_name
            )));
        }

        // The filter is only usable if it was built over the bytes this reader will probe
        // with. Any disagreement and it is dropped: a wrong filter loses keys.
        let configured = options
            .prefix_extractor
            .as_ref()
            .map(|extractor| extractor.name().to_string());
        let filter = if footer.filter.is_none() {
            None
        } else if props.prefix_extractor_name == configured {
            Some(BloomFilter::parse(source.read(footer.filter, false)?)?)
        } else {
            tracing::warn!(
                file = %context,
                built_with = ?props.prefix_extractor_name,
                reading_with = ?configured,
                "ignoring the bloom filter: it was built over different bytes than this reader \
                 would probe with"
            );
            None
        };

        let index = source.read_parsed(footer.index, false)?;

        // The range-deletion block, when there is one. Its handle is in the properties rather
        // than the footer, which has no room for a fourth
        // ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)). Read at open and held for
        // the life of the reader, like the index and the filter: every read of this table has
        // to consult it, and nearly every table has none at all.
        let range_tombstones = if props.range_del_count == 0 {
            RangeTombstones::new()
        } else {
            let handle = BlockHandle::new(props.range_del_offset, props.range_del_size);
            let payload = source.read(handle, false)?;
            let tombstones = RangeTombstones::decode(&payload, options.comparator.as_ref())?;
            if tombstones.len() as u64 != props.range_del_count {
                return Err(Error::corruption(
                    &context,
                    format!(
                        "the properties claim {} range tombstones and the block holds {}",
                        props.range_del_count,
                        tombstones.len()
                    ),
                ));
            }
            tombstones
        };

        Ok(Self {
            inner: Arc::new(TableInner {
                file,
                file_size,
                file_number,
                context,
                options,
                cache,
                index,
                filter,
                range_tombstones,
                props: TableProperties { file_size, ..props },
            }),
        })
    }

    /// What the table records about itself.
    #[must_use]
    pub fn properties(&self) -> &TableProperties {
        &self.inner.props
    }

    /// The range tombstones this table carries; empty for nearly every table
    /// ([ADR 0017](../../../docs/adr/0017-range-tombstones.md)).
    ///
    /// A read that finds an entry in this table — or in any *lower* one — has to ask these
    /// whether it is covered, which is the second question a range delete forces on every read
    /// path.
    #[must_use]
    pub fn range_tombstones(&self) -> &RangeTombstones {
        &self.inner.range_tombstones
    }

    /// Whether a usable bloom filter was found. False either because the table has none or
    /// because it was built over different bytes — see the module docs.
    #[must_use]
    pub fn has_filter(&self) -> bool {
        self.inner.filter.is_some()
    }

    /// Whether this table could hold `key`, according to its bloom filter.
    ///
    /// A filter can only ever rule a key *out*, so this answers `true` whenever it cannot
    /// help: no filter, one built over different bytes, or a key outside the extractor's
    /// domain — such a key was never added, so its absence from the filter means nothing.
    ///
    /// Exposed separately from [`get`](TableReader::get) because the engine's point-read path
    /// is a *seek*, not an exact match: it looks for `(user_key, snapshot)`, which is almost
    /// never stored verbatim, so it drives [`iter`](TableReader::iter) and needs to consult
    /// the filter itself before opening a cursor and reading a block.
    #[must_use]
    pub fn may_contain(&self, key: &[u8]) -> bool {
        let inner = &self.inner;
        let Some(filter) = inner.filter.as_ref() else {
            return true;
        };
        // The probe key goes through the same `filter_key` the builder used.
        match filter_key(inner.options.prefix_extractor.as_deref(), key) {
            Some(bytes) => filter.may_contain(bytes),
            None => true,
        }
    }

    /// Looks one key up.
    ///
    /// Bloom, then index, then one data block. `Ok(None)` means the key is not in this table;
    /// an `Err` means the table could not be read and the caller must not treat it as absence.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let inner = &self.inner;
        if !self.may_contain(key) {
            return Ok(None);
        }

        let mut index_iter = inner.index.iter(inner.comparator());
        index_iter.seek(key);
        index_iter.status()?;
        if !index_iter.valid() {
            // Past the separator of the last block, so past every key in the table.
            return Ok(None);
        }

        let (handle, _) = BlockHandle::decode_from(index_iter.value())?;
        let block = inner.read_parsed_block(handle, true)?;
        let mut iter = block.iter(inner.comparator());
        iter.seek(key);
        iter.status()?;
        if iter.valid() && inner.options.comparator.cmp(iter.key(), key).is_eq() {
            return Ok(Some(iter.value().to_vec()));
        }
        Ok(None)
    }

    /// A cursor over every entry in the table.
    ///
    /// Not a `std::iter::Iterator`: this is the `seek / seek_for_prev / next / prev / key /
    /// value / valid` cursor of `docs/DESIGN.md` §4.1, which the merge iterator above needs.
    #[allow(clippy::iter_not_returning_iterator)]
    #[must_use]
    pub fn iter(&self) -> TableIter {
        TableIter {
            index_iter: self.inner.index.iter(self.inner.comparator()),
            inner: Arc::clone(&self.inner),
            data_iter: None,
            status: None,
        }
    }
}

/// A cursor over one table: an iterator over the index, and an iterator over the data block
/// the index is currently pointing at.
///
/// Owns a share of the table, so it can outlive the [`TableReader`] it came from.
#[derive(Debug)]
pub struct TableIter {
    inner: Arc<TableInner>,
    index_iter: BlockIter,
    /// `None` when the cursor is off either end, or when a block could not be read.
    data_iter: Option<BlockIter>,
    status: Option<Error>,
}

impl TableIter {
    /// Whether the cursor is on an entry.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.data_iter.as_ref().is_some_and(BlockIter::valid)
    }

    /// The current key. Empty when invalid.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        match self.data_iter.as_ref() {
            Some(iter) => iter.key(),
            None => &[],
        }
    }

    /// The current value. Empty when invalid.
    #[must_use]
    pub fn value(&self) -> &[u8] {
        match self.data_iter.as_ref() {
            Some(iter) => iter.value(),
            None => &[],
        }
    }

    /// The first error this cursor met: a failed read, a bad checksum, a corrupt block.
    ///
    /// A caller that has walked to the end must check this before concluding it saw
    /// everything, because an unreadable block also ends iteration.
    pub fn status(&self) -> Result<()> {
        if let Some(error) = &self.status {
            return Err(clone_error(error));
        }
        self.index_iter.status()?;
        match self.data_iter.as_ref() {
            Some(iter) => iter.status(),
            None => Ok(()),
        }
    }

    fn fail(&mut self, error: Error) {
        if self.status.is_none() {
            self.status = Some(error);
        }
        self.data_iter = None;
    }

    /// Opens the data block the index cursor names, or clears the data cursor when the index
    /// cursor is off the end.
    fn open_current_block(&mut self) {
        if !self.index_iter.valid() {
            self.data_iter = None;
            return;
        }
        let handle = match BlockHandle::decode_from(self.index_iter.value()) {
            Ok((handle, _)) => handle,
            Err(error) => return self.fail(error),
        };
        match self.inner.read_parsed_block(handle, true) {
            Ok(block) => self.data_iter = Some(block.iter(self.inner.comparator())),
            Err(error) => self.fail(error),
        }
    }

    /// After a move that may have run off the end of a block, walks forward over any blocks
    /// that have nothing left to give.
    fn skip_forward(&mut self) {
        while !self.valid() {
            if self.status.is_some() || !self.index_iter.valid() {
                self.data_iter = None;
                return;
            }
            self.index_iter.next();
            self.open_current_block();
            if let Some(iter) = self.data_iter.as_mut() {
                iter.seek_to_first();
            }
        }
    }

    /// The mirror of [`TableIter::skip_forward`].
    fn skip_backward(&mut self) {
        while !self.valid() {
            if self.status.is_some() || !self.index_iter.valid() {
                self.data_iter = None;
                return;
            }
            self.index_iter.prev();
            self.open_current_block();
            if let Some(iter) = self.data_iter.as_mut() {
                iter.seek_to_last();
            }
        }
    }

    /// Positions on the first entry of the table.
    pub fn seek_to_first(&mut self) {
        self.index_iter.seek_to_first();
        self.open_current_block();
        if let Some(iter) = self.data_iter.as_mut() {
            iter.seek_to_first();
        }
        self.skip_forward();
    }

    /// Positions on the last entry of the table.
    pub fn seek_to_last(&mut self) {
        self.index_iter.seek_to_last();
        self.open_current_block();
        if let Some(iter) = self.data_iter.as_mut() {
            iter.seek_to_last();
        }
        self.skip_backward();
    }

    /// Positions on the first entry whose key is `>= target`.
    pub fn seek(&mut self, target: &[u8]) {
        self.index_iter.seek(target);
        self.open_current_block();
        if let Some(iter) = self.data_iter.as_mut() {
            iter.seek(target);
        }
        self.skip_forward();
    }

    /// Positions on the last entry whose key is `<= target`.
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        self.seek(target);
        if self.status.is_some() {
            return;
        }
        if !self.valid() {
            self.seek_to_last();
            if self.valid()
                && self
                    .inner
                    .options
                    .comparator
                    .cmp(self.key(), target)
                    .is_gt()
            {
                self.data_iter = None;
            }
            return;
        }
        if self
            .inner
            .options
            .comparator
            .cmp(self.key(), target)
            .is_gt()
        {
            self.prev();
        }
    }

    /// Advances one entry, crossing into the next block when this one runs out.
    pub fn next(&mut self) {
        if !self.valid() {
            return;
        }
        if let Some(iter) = self.data_iter.as_mut() {
            iter.next();
        }
        self.skip_forward();
    }

    /// Steps back one entry, crossing into the previous block when this one runs out.
    pub fn prev(&mut self) {
        if !self.valid() {
            return;
        }
        if let Some(iter) = self.data_iter.as_mut() {
            iter.prev();
        }
        self.skip_backward();
    }
}

/// [`Error`] is not `Clone` — an `io::Error` is not — so a stored status is rebuilt rather
/// than cloned. Corruption keeps its shape because callers branch on it.
fn clone_error(error: &Error) -> Error {
    match error {
        Error::Corruption { context, detail } => Error::corruption(context.clone(), detail.clone()),
        other => Error::InvalidArgument(other.to_string()),
    }
}
