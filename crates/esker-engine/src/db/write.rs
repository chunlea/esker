//! Group commit: one log write and one `fsync` for everyone waiting.
//!
//! The first writer to find the queue idle becomes the **leader**. It drains what is waiting —
//! up to a megabyte or 128 batches (`docs/DESIGN.md` §4.2) — merges it into one batch, writes
//! one log record, syncs once, inserts into the memtables, and only then publishes the
//! sequence numbers and wakes everyone. Under load that turns N `fsync`s into one, which is
//! the whole reason a log-structured engine is fast at writing.
//!
//! # Two rules that are easy to break
//!
//! **The queue lock is never held across the `fsync`.** The leader takes the group, releases
//! the lock, and does the I/O without it. Holding it would serialise every arriving writer
//! behind a disk flush and turn group commit into a queue with extra steps.
//!
//! **A follower learns its sequence number only after the leader publishes.** Results go into
//! the queue's `done` map after the memtable inserts, under the lock, so nobody can observe a
//! sequence number for a batch that is not yet readable.
//!
//! # A `sync = false` write in a synced group
//!
//! It gets durability for free. That is correct, not a bug: `sync = false` is permission to
//! acknowledge early, never a requirement to. The reverse — a `sync = true` write riding a
//! group that did not sync — is the thing that must not happen, and does not, because the
//! leader syncs if *any* member asked.

use std::sync::atomic::Ordering;

use crate::batch::WriteBatch;
use crate::dbformat::{EntryKind, SeqNo};
use crate::error::{Error, Result};
use crate::options::{WalSyncMode, WriteOptions};

use super::{Db, DbInner, Pending, lock, read_lock};

impl Db {
    /// Applies `batch` atomically, returning the sequence number it became visible at.
    ///
    /// Blocks until the batch is in the log — and, unless `sync` is off, until those bytes are
    /// durable (invariant 1). An empty batch still takes a turn and returns the sequence
    /// number current when it did.
    pub fn write(&self, batch: WriteBatch, options: &WriteOptions) -> Result<SeqNo> {
        self.inner.write(batch, *options)
    }

    /// Writes one key. A convenience over [`Db::write`] with the default write options.
    pub fn put(&self, cf: &str, key: &[u8], value: &[u8]) -> Result<SeqNo> {
        let id = self.require_cf(cf)?;
        let mut batch = WriteBatch::new();
        batch.put(id, key, value);
        self.write(batch, &WriteOptions::default())
    }

    /// Deletes one key.
    pub fn delete(&self, cf: &str, key: &[u8]) -> Result<SeqNo> {
        let id = self.require_cf(cf)?;
        let mut batch = WriteBatch::new();
        batch.delete(id, key);
        self.write(batch, &WriteOptions::default())
    }

    /// Makes every buffered log byte durable, whatever the sync mode.
    ///
    /// A `sync = false` write is acknowledged before its bytes reach the device; this is how a
    /// caller that batched many of them makes them all durable at once.
    pub fn sync_wal(&self) -> Result<()> {
        let mut wal = lock(&self.inner.wal)?;
        wal.writer.sync()
    }

    fn require_cf(&self, name: &str) -> Result<u32> {
        Ok(self.inner.cf_by_name(name)?.id())
    }
}

impl DbInner {
    fn write(&self, batch: WriteBatch, options: WriteOptions) -> Result<SeqNo> {
        self.check_batch(&batch)?;
        let sync = options.sync || self.options.wal_sync_mode == WalSyncMode::PerWrite;

        // Before anything is logged: make room, which may switch a memtable, roll the log
        // and stall this writer.
        self.make_room_for_write()?;

        let ticket = {
            let mut queue = lock(&self.writers)?;
            let ticket = queue.next_ticket;
            queue.next_ticket += 1;
            queue.pending.push_back(Pending {
                ticket,
                batch,
                sync,
            });
            ticket
        };

        loop {
            let mut queue = lock(&self.writers)?;
            if let Some(result) = queue.done.remove(&ticket) {
                return result.map_err(Error::GroupCommit);
            }
            let leading =
                !queue.writing && queue.pending.front().is_some_and(|p| p.ticket == ticket);
            if !leading {
                // Not our turn. `wait` releases the lock, so arriving writers still get in.
                let _guard = self.write_ready.wait(queue).map_err(|_| {
                    Error::Poisoned("the write queue lock was poisoned".to_string())
                })?;
                continue;
            }

            queue.writing = true;
            let mut group = Vec::new();
            let mut bytes = 0usize;
            while let Some(front) = queue.pending.front() {
                let next = bytes + front.batch.byte_size();
                // The leader always takes itself, however large it is.
                if !group.is_empty()
                    && (next > self.options.group_commit_max_bytes
                        || group.len() >= self.options.group_commit_max_batches)
                {
                    break;
                }
                bytes = next;
                if let Some(pending) = queue.pending.pop_front() {
                    group.push(pending);
                }
            }
            drop(queue);

            // No lock held here: this is the log append and the fsync.
            let outcome = self.commit_group(&group);

            let mut queue = lock(&self.writers)?;
            queue.writing = false;
            let mine = match outcome {
                Ok(assigned) => {
                    let mut mine = Err(Error::GroupCommit("the leader lost its own batch".into()));
                    for (other, seqno) in assigned {
                        if other == ticket {
                            mine = Ok(seqno);
                        } else {
                            queue.done.insert(other, Ok(seqno));
                        }
                    }
                    mine
                }
                Err(err) => {
                    // One failure belongs to everyone in the group: their bytes shared the
                    // record that did not land.
                    let reason = err.to_string();
                    for pending in &group {
                        if pending.ticket != ticket {
                            queue.done.insert(pending.ticket, Err(reason.clone()));
                        }
                    }
                    Err(err)
                }
            };
            self.write_ready.notify_all();
            drop(queue);
            return mine;
        }
    }

    /// Writes one group: merge, log, sync, insert, publish. Returns each ticket's sequence
    /// number.
    fn commit_group(&self, group: &[Pending]) -> Result<Vec<(u64, SeqNo)>> {
        let total: u64 = group.iter().map(|p| u64::from(p.batch.count())).sum();
        let base = self.next_seqno.fetch_add(total, Ordering::SeqCst);

        let mut merged = WriteBatch::new();
        merged.set_seqno(base);
        let mut assigned = Vec::with_capacity(group.len());
        let mut cursor = base;
        for pending in group {
            let count = u64::from(pending.batch.count());
            // The sequence number a caller gets back is the last one its batch consumed. An
            // empty batch consumed none, so it reports the sequence number that was current
            // where it sat in the group — which is what a reader would see just after it.
            assigned.push((pending.ticket, (cursor + count).saturating_sub(1)));
            cursor += count;
            merged.append(&pending.batch);
        }

        {
            let mut wal = lock(&self.wal)?;
            wal.writer.add_record(merged.as_bytes());
            if group.iter().any(|pending| pending.sync) {
                // Invariant 1: durable before anyone is told it happened.
                wal.writer.sync()?;
            } else {
                wal.writer.flush()?;
            }
        }

        let cfs = read_lock(&self.cfs)?;
        for entry in &merged {
            let entry = entry?;
            match cfs.get(&entry.cf) {
                Some(cf) => {
                    let mem = read_lock(&cf.mem)?;
                    mem.active
                        .add(entry.seqno, entry.kind, entry.key, entry.value);
                }
                None => {
                    // Dropped between the check at the top of `write` and here. The record is
                    // in the log, which is correct: replay will skip it for the same reason.
                    tracing::debug!(cf = entry.cf, "write for a column family that was dropped");
                }
            }
        }
        drop(cfs);

        // Publish last. Until this store, the writes are durable but invisible, which is the
        // window in which a half-inserted batch would otherwise be readable.
        if total > 0 {
            self.visible_seqno
                .store(base + total - 1, Ordering::Release);
        }
        Ok(assigned)
    }

    /// Rejects a batch naming a column family that does not exist, before anything is logged.
    /// Refuses a batch this version cannot honour, before any of it is logged.
    ///
    /// Two reasons, and the second is a limitation rather than a mistake:
    ///
    /// * a column family the database does not have;
    /// * a [`EntryKind::DeleteRange`], which **v1 does not implement**. The entry kind is part
    ///   of the frozen `WriteBatch` and log formats ([`crate::batch`], `docs/DESIGN.md` §4.3)
    ///   so that making it real in phase 5 is not a format change — but no read path honours
    ///   it: the memtable, `get` and both iterators treat it as a point `Delete` at the
    ///   range's `begin`. Storing one would therefore delete a single key while telling the
    ///   caller a range was gone, which is a silent wrong answer and the worst kind. Refusing
    ///   it is the honest version of the check `docs/DESIGN.md` §4.7 always described.
    ///
    /// Both checks happen before `make_room`, before the log append and before any memtable
    /// insert, so a refused batch changes nothing.
    fn check_batch(&self, batch: &WriteBatch) -> Result<()> {
        let cfs = read_lock(&self.cfs)?;
        for entry in batch {
            let entry = entry?;
            if !cfs.contains_key(&entry.cf) {
                return Err(Error::InvalidArgument(format!(
                    "no column family with id {}",
                    entry.cf
                )));
            }
            if entry.kind == EntryKind::DeleteRange {
                return Err(Error::Unsupported(
                    "DeleteRange is not implemented in v1: the entry kind is part of the \
                     format, but no read path honours it, so storing one would delete only \
                     the key at the range's start. Delete a range through esker-store, which \
                     does it as a bounded scan and point deletes in one atomic batch \
                     (docs/adr/0006-rawkv-delete-range.md); real range tombstones are phase 5 \
                     (docs/DESIGN.md §4.7)"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}
