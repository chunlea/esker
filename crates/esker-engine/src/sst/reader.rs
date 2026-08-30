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
                props: TableProperties { file_size, ..props },
            }),
        })
    }

    /// What the table records about itself.
    #[must_use]
    pub fn properties(&self) -> &TableProperties {
        &self.inner.props
    }

    /// Whether a usable bloom filter was found. False either because the table has none or
    /// because it was built over different bytes — see the module docs.
    #[must_use]
    pub fn has_filter(&self) -> bool {
        self.inner.filter.is_some()
    }

    /// Looks one key up.
    ///
    /// Bloom, then index, then one data block. `Ok(None)` means the key is not in this table;
    /// an `Err` means the table could not be read and the caller must not treat it as absence.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let inner = &self.inner;
        if let Some(filter) = inner.filter.as_ref() {
            // The probe key goes through the same `filter_key` the builder used. A key outside
            // the extractor's domain was never added, so it cannot be filtered out.
            match filter_key(inner.options.prefix_extractor.as_deref(), key) {
                Some(bytes) if !filter.may_contain(bytes) => return Ok(None),
                _ => {}
            }
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

#[cfg(test)]
mod tests {
    use super::{TableIter, TableReader};
    use crate::cache::ShardedLruCache;
    use crate::cache_api::BlockCache;
    use crate::dbformat::Comparator;
    use crate::fs::FileSystem;
    use crate::memfs::MemFileSystem;
    use crate::options::{Compression, StripSuffix};
    use crate::sst::builder::{TableBuilder, TableOptions};
    use std::cmp::Ordering;
    use std::path::Path;
    use std::sync::Arc;

    /// Reverse bytewise order, to prove the reader takes its order from the comparator.
    #[derive(Debug)]
    struct ReverseComparator;

    impl Comparator for ReverseComparator {
        fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
            b.cmp(a)
        }
        fn name(&self) -> &'static str {
            "test.ReverseComparator"
        }
    }

    fn kv(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..count)
            .map(|i| {
                (
                    format!("key{i:06}").into_bytes(),
                    format!("value-{i}-{}", "p".repeat(i % 37)).into_bytes(),
                )
            })
            .collect()
    }

    /// Writes a table into a fresh in-memory filesystem and opens it again.
    fn round_trip(
        write: TableOptions,
        read: TableOptions,
        entries: &[(Vec<u8>, Vec<u8>)],
        cache: Option<Arc<dyn BlockCache>>,
    ) -> crate::error::Result<TableReader> {
        let fs = MemFileSystem::new();
        let mut builder = TableBuilder::new(write, fs.create(Path::new("/t.sst")).unwrap());
        for (key, value) in entries {
            builder.add(key, value).unwrap();
        }
        builder.finish().unwrap();
        TableReader::open(fs.open(Path::new("/t.sst")).unwrap(), 7, read, cache)
    }

    fn open_default(entries: &[(Vec<u8>, Vec<u8>)]) -> TableReader {
        round_trip(
            TableOptions::default(),
            TableOptions::default(),
            entries,
            None,
        )
        .expect("a table this writer built opens")
    }

    fn collect(iter: &mut TableIter) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        iter.seek_to_first();
        while iter.valid() {
            out.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.next();
        }
        out
    }

    /// Every key written comes back, by `get` and by walking, in both directions, across many
    /// data blocks.
    #[test]
    fn every_key_round_trips_across_block_boundaries() {
        let entries = kv(2_000);
        let table = open_default(&entries);
        assert!(
            table.properties().data_block_count > 10,
            "the test needs many blocks, got {}",
            table.properties().data_block_count
        );

        for (key, value) in &entries {
            assert_eq!(table.get(key).unwrap().as_ref(), Some(value), "get {key:?}");
        }
        assert_eq!(table.get(b"key999999").unwrap(), None);
        assert_eq!(table.get(b"").unwrap(), None);

        let mut iter = table.iter();
        assert_eq!(collect(&mut iter), entries);

        let mut backwards = Vec::new();
        iter.seek_to_last();
        while iter.valid() {
            backwards.push((iter.key().to_vec(), iter.value().to_vec()));
            iter.prev();
        }
        backwards.reverse();
        assert_eq!(backwards, entries);
        iter.status().unwrap();
    }

    /// Seeking lands on the right entry even when the target falls between two data blocks,
    /// which is where a two-level iterator goes wrong.
    #[test]
    fn seeking_between_blocks() {
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..600u32)
            .map(|i| (format!("k{:06}", i * 10).into_bytes(), b"v".to_vec()))
            .collect();
        let options = TableOptions {
            block_size: 128,
            ..TableOptions::default()
        };
        let table = round_trip(options.clone(), options, &entries, None).unwrap();
        assert!(table.properties().data_block_count > 30);

        let mut iter = table.iter();
        for (i, (key, _)) in entries.iter().enumerate() {
            // Exactly on a key.
            iter.seek(key);
            assert!(iter.valid());
            assert_eq!(iter.key(), &key[..], "seek to {key:?}");

            // Between this key and the next: k000005 sits between k000000 and k000010.
            let mut between = key.clone();
            between.extend_from_slice(b"5");
            iter.seek(&between);
            let expected = entries.get(i + 1).map(|(k, _)| k.clone());
            assert_eq!(iter.valid().then(|| iter.key().to_vec()), expected);

            iter.seek_for_prev(&between);
            assert!(iter.valid());
            assert_eq!(iter.key(), &key[..], "seek_for_prev past {key:?}");
        }

        iter.seek(b"a");
        assert_eq!(iter.key(), &entries[0].0[..], "before the first key");
        iter.seek_for_prev(b"a");
        assert!(!iter.valid(), "nothing at or before the first key");
        iter.seek(b"zzz");
        assert!(!iter.valid(), "past the last key");
        iter.seek_for_prev(b"zzz");
        assert_eq!(iter.key(), &entries[599].0[..]);
        iter.status().unwrap();
    }

    /// An empty table opens, reads as empty, and does not panic on any cursor operation.
    #[test]
    fn an_empty_table_reads_as_empty() {
        let table = open_default(&[]);
        assert_eq!(table.properties().entry_count, 0);
        assert!(!table.has_filter());
        assert_eq!(table.get(b"anything").unwrap(), None);

        let mut iter = table.iter();
        assert_eq!(collect(&mut iter), vec![]);
        iter.seek_to_last();
        assert!(!iter.valid());
        iter.seek(b"k");
        assert!(!iter.valid());
        iter.seek_for_prev(b"k");
        assert!(!iter.valid());
        iter.next();
        iter.prev();
        assert!(!iter.valid());
        iter.status().unwrap();
    }

    /// A single entry is the first, the last, and the whole of its only block.
    #[test]
    fn a_single_entry_table() {
        let entries = vec![(b"only".to_vec(), b"value".to_vec())];
        let table = open_default(&entries);
        assert_eq!(table.get(b"only").unwrap().as_deref(), Some(&b"value"[..]));
        assert_eq!(table.get(b"onlz").unwrap(), None);
        assert_eq!(table.get(b"onlx").unwrap(), None);

        let mut iter = table.iter();
        assert_eq!(collect(&mut iter), entries);
        iter.seek_to_last();
        assert_eq!(iter.key(), b"only");
        iter.next();
        assert!(!iter.valid());
        iter.prev();
        assert!(!iter.valid(), "prev from invalid must stay invalid");
    }

    /// Order comes from the injected comparator, not from `memcmp`, all the way through the
    /// reader.
    #[test]
    fn a_reverse_comparator_is_honoured_end_to_end() {
        let options = TableOptions {
            comparator: Arc::new(ReverseComparator),
            block_size: 64,
            ..TableOptions::default()
        };
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..200u32)
            .rev()
            .map(|i| (format!("k{i:04}").into_bytes(), b"v".to_vec()))
            .collect();
        let table = round_trip(options.clone(), options, &entries, None).unwrap();

        for (key, _) in &entries {
            assert!(table.get(key).unwrap().is_some(), "get {key:?}");
        }
        let mut iter = table.iter();
        assert_eq!(
            collect(&mut iter),
            entries,
            "iteration follows reverse order"
        );
    }

    /// A table read with the wrong comparator would silently miss keys, so opening it fails.
    #[test]
    fn a_comparator_mismatch_is_refused_at_open() {
        let error = round_trip(
            TableOptions::default(),
            TableOptions {
                comparator: Arc::new(ReverseComparator),
                ..TableOptions::default()
            },
            &kv(20),
            None,
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("comparator"), "{text}");
        assert!(text.contains("wrong order"), "{text}");
    }

    /// The unforgivable bloom bug, at the table level: a filter built over whole keys and a
    /// reader configured with a prefix extractor (or the reverse) must not be used, and every
    /// key must still be found.
    #[test]
    fn a_prefix_extractor_mismatch_disables_the_filter_rather_than_losing_keys() {
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..500u32)
            .map(|i| (format!("k{i:04}ts{i:04}").into_bytes(), b"v".to_vec()))
            .collect();
        let with = TableOptions {
            prefix_extractor: Some(Arc::new(StripSuffix::new(6))),
            ..TableOptions::default()
        };
        let without = TableOptions::default();

        for (write, read) in [
            (with.clone(), without.clone()),
            (without.clone(), with.clone()),
            (
                with.clone(),
                TableOptions {
                    prefix_extractor: Some(Arc::new(StripSuffix::new(4))),
                    ..TableOptions::default()
                },
            ),
        ] {
            let table = round_trip(write, read, &entries, None).unwrap();
            assert!(
                !table.has_filter(),
                "a filter built over different bytes was kept"
            );
            for (key, value) in &entries {
                assert_eq!(
                    table.get(key).unwrap().as_ref(),
                    Some(value),
                    "mismatched filter lost {key:?}"
                );
            }
        }

        // Matching extractors keep the filter, and it still finds every key.
        let table = round_trip(with.clone(), with, &entries, None).unwrap();
        assert!(table.has_filter());
        for (key, value) in &entries {
            assert_eq!(table.get(key).unwrap().as_ref(), Some(value));
        }
    }

    /// The filter's job: an absent key must usually be answered without reading a block.
    #[test]
    fn the_filter_answers_absent_keys_without_reading_a_block() {
        let entries = kv(2_000);
        let cache = Arc::new(ShardedLruCache::new(8 * 1024 * 1024));
        let table = round_trip(
            TableOptions::default(),
            TableOptions::default(),
            &entries,
            Some(cache.clone()),
        )
        .unwrap();
        assert!(table.has_filter());

        let before = cache.stats();
        for i in 0..2_000u32 {
            assert_eq!(table.get(format!("absent{i:06}").as_bytes()).unwrap(), None);
        }
        let after = cache.stats();
        let block_reads = (after.hits + after.misses) - (before.hits + before.misses);
        assert!(
            block_reads * 50 < 2_000,
            "{block_reads} block lookups for 2000 absent keys; the filter is not being used"
        );
    }

    /// A cache in front of the file must be filled and then hit, and the table must read the
    /// same with a cache as without one.
    #[test]
    fn the_block_cache_is_filled_and_then_hit() {
        let entries = kv(500);
        let cache = Arc::new(ShardedLruCache::new(4 * 1024 * 1024));
        let table = round_trip(
            TableOptions::default(),
            TableOptions::default(),
            &entries,
            Some(cache.clone()),
        )
        .unwrap();

        assert_eq!(
            cache.stats().entries,
            0,
            "opening should not fill the cache"
        );
        for (key, value) in &entries {
            assert_eq!(table.get(key).unwrap().as_ref(), Some(value));
        }
        let warm = cache.stats();
        assert!(warm.entries > 0, "no data block was cached");
        assert!(warm.misses > 0);

        let before_hits = warm.hits;
        for (key, value) in &entries {
            assert_eq!(table.get(key).unwrap().as_ref(), Some(value));
        }
        let after = cache.stats();
        assert!(
            after.hits > before_hits,
            "a second pass read nothing from the cache"
        );
        assert_eq!(after.misses, warm.misses, "a warm cache still missed");
    }

    /// Compression is invisible above the block trailer: the same entries come back either way.
    #[test]
    fn both_codecs_read_the_same() {
        let entries = kv(400);
        for compression in [Compression::None, Compression::Lz4] {
            let options = TableOptions {
                compression,
                ..TableOptions::default()
            };
            let table = round_trip(options.clone(), options, &entries, None).unwrap();
            let mut iter = table.iter();
            assert_eq!(collect(&mut iter), entries, "{compression:?}");
        }
    }

    /// Files that are not tables must be named as such rather than parsed into nonsense.
    #[test]
    fn files_that_are_not_tables_are_refused() {
        let fs = MemFileSystem::new();
        let open = |name: &str| {
            TableReader::open(
                fs.open(Path::new(name)).unwrap(),
                1,
                TableOptions::default(),
                None,
            )
        };

        fs.install("/empty.sst", Vec::new()).unwrap();
        assert!(open("/empty.sst").is_err());

        fs.install("/short.sst", vec![0u8; 47]).unwrap();
        assert!(open("/short.sst").is_err());

        fs.install("/garbage.sst", vec![0xab; 4096]).unwrap();
        assert!(open("/garbage.sst").is_err());

        // A real table with its magic rewritten.
        let mut builder = TableBuilder::new(
            TableOptions::default(),
            fs.create(Path::new("/good.sst")).unwrap(),
        );
        builder.add(b"k", b"v").unwrap();
        builder.finish().unwrap();
        let mut bytes = fs.contents("/good.sst").unwrap();
        let last = bytes.len() - 1;
        bytes[last] = b'9';
        fs.install("/bad-magic.sst", bytes).unwrap();
        let error = open("/bad-magic.sst").unwrap_err();
        assert!(error.is_corruption(), "{error}");
    }

    /// Truncating a table at any point must be an error at open or at read, never a wrong
    /// answer. The footer is last, so most truncations lose it outright.
    #[test]
    fn truncation_is_detected() {
        let fs = MemFileSystem::new();
        let mut builder = TableBuilder::new(
            TableOptions {
                block_size: 128,
                ..TableOptions::default()
            },
            fs.create(Path::new("/t.sst")).unwrap(),
        );
        let entries = kv(200);
        for (key, value) in &entries {
            builder.add(key, value).unwrap();
        }
        builder.finish().unwrap();
        let full = fs.contents("/t.sst").unwrap();

        for cut in (1..full.len()).step_by(7) {
            fs.install("/cut.sst", full[..cut].to_vec()).unwrap();
            let opened = TableReader::open(
                fs.open(Path::new("/cut.sst")).unwrap(),
                2,
                TableOptions::default(),
                None,
            );
            let Ok(table) = opened else { continue };
            // Opening a truncated file can only succeed if the footer survived, which means
            // the file is intact; anything else must fail when the missing bytes are needed.
            let mut iter = table.iter();
            let mut seen = 0;
            iter.seek_to_first();
            while iter.valid() {
                seen += 1;
                iter.next();
            }
            if iter.status().is_ok() {
                assert_eq!(
                    seen,
                    entries.len(),
                    "a truncation at {cut} read cleanly but lost entries"
                );
            }
        }
    }
}
