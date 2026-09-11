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
use crate::options::WriteOptions;

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
        self.inner.sync_wal_now()
    }

    fn require_cf(&self, name: &str) -> Result<u32> {
        Ok(self.inner.cf_by_name(name)?.id())
    }
}

impl DbInner {
    /// Refuses `what` if this database was opened read-only.
    ///
    /// **Every path that changes a byte goes through this**, and they are not only the obvious
    /// ones: a flush writes a table and a manifest edit, a compaction writes tables, the sweep
    /// *deletes* them, and `create_cf` writes an edit. A read-only open has no log to write to
    /// and no claim on the directory, so any of those is not a write that fails — it is a write
    /// into a database another process owns.
    pub(crate) fn writable(&self, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Unsupported(format!(
                "{} is open read-only, so it cannot {what}",
                self.dir.display()
            )));
        }
        Ok(())
    }

    fn write(&self, batch: WriteBatch, options: WriteOptions) -> Result<SeqNo> {
        // **First, before the batch is even checked.** See [`DbInner::writable`].
        self.writable("write")?;
        self.check_batch(&batch)?;
        // The whole precedence rule is [`WriteOptions::wants_sync`], in one place so that the
        // mode and the per-write demand cannot drift apart. Before debt wave c3 this line was
        // `options.sync || mode == PerWrite`, an OR that could only ever *add* syncing — and since
        // `WriteOptions::default()` said `sync: true`, no write in this repository ever left the
        // mode anything to decide (`docs/plans/debt-c3.md` §4).
        let sync = options.wants_sync(self.options.wal_sync_mode);

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

    /// The background WAL syncer: makes the log durable every
    /// [`WalSyncMode::Interval`](crate::WalSyncMode::Interval), so writers never wait for it.
    ///
    /// **This variant used to be read by nothing at all.** The one line that consulted the mode
    /// compared it against `PerWrite`, so `Interval(d)` behaved exactly like `Never` — a database
    /// configured for *bounded* loss had unbounded loss and said nothing about it
    /// (`docs/plans/debt-c3.md` §4).
    ///
    /// It waits on the flush condvar rather than sleeping, which is what makes shutdown prompt:
    /// `Drop` sets `shutdown` under that lock and notifies, so the thread leaves within one wake
    /// rather than within one interval. The cost is that flush activity can wake it early and it
    /// syncs sooner than asked — which only ever makes writes durable sooner, and the contract
    /// this mode offers is an upper bound on what a crash may lose.
    ///
    /// A sync that fails is logged and the loop continues. It cannot be reported to a writer,
    /// because every writer it would concern was acknowledged before it ran; what it must not do
    /// is stop, since a syncer that exits on one bad sync turns a transient I/O error into the
    /// unbounded loss this mode exists to avoid.
    pub(crate) fn wal_sync_loop(&self, interval: std::time::Duration) {
        while !self.shutdown.load(Ordering::Acquire) {
            {
                let Ok(state) = self.flush.lock() else {
                    return;
                };
                let Ok(_guard) = self.flush_wanted.wait_timeout(state, interval) else {
                    return;
                };
            }
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = self.sync_wal_now() {
                tracing::warn!(%error, "the background write-ahead log sync failed");
            }
        }
    }

    /// Makes every buffered log byte durable. The one place that reaches the log to sync it, so
    /// the background loop, [`Db::sync_wal`] and the close all take the same lock in the same way.
    pub(crate) fn sync_wal_now(&self) -> Result<()> {
        let mut wal = lock(&self.wal)?;
        wal.writer.sync()
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
                    if entry.kind == EntryKind::DeleteRange {
                        // Beside the map, never in it: a range delete hides keys the map has
                        // never seen ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md)).
                        // The entry's `value` is the range's exclusive end
                        // ([`crate::batch::WriteBatch::delete_range`]).
                        mem.active.add_range(entry.seqno, entry.key, entry.value);
                    } else {
                        // **A refusal fails the group.** The arena being full means this entry
                        // is not in the table, and the group's bytes are already in the log — so
                        // returning success here would acknowledge a write no read can answer,
                        // which is invariant 1 broken without a word. Failing the group is the
                        // same treatment a log-write failure gets a few lines above, and for the
                        // same reason (`docs/plans/debt-c6.md` §15).
                        mem.active
                            .add(entry.seqno, entry.kind, entry.key, entry.value)?;
                    }
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
    /// Refuses a batch this database cannot honour, before any of it is logged.
    ///
    /// Two reasons, and both are caller errors:
    ///
    /// * a column family the database does not have;
    /// * a [`EntryKind::DeleteRange`] whose `end` is not strictly above its `begin`. `RocksDB`
    ///   treats that as a no-op; this engine refuses it, because "nothing happened" and
    ///   "everything from `begin` was deleted" are far enough apart that guessing between them
    ///   is worse than saying so ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md)
    ///   decision 4). There is no convention that an empty `end` means the end of the key
    ///   space: the engine is byte-opaque and an empty `end` sorts *below* everything, so a
    ///   caller wanting a whole namespace passes that namespace's successor.
    ///
    /// Both checks happen before `make_room`, before the log append and before any memtable
    /// insert, so a refused batch changes nothing.
    ///
    /// The refusal of `DeleteRange` itself is gone: `docs/DESIGN.md` §4.7's limitation was
    /// that no read path honoured one, and they now do
    /// ([ADR 0017](../../../../docs/adr/0017-range-tombstones.md)).
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
                let end = entry.value;
                if self.comparator.user_comparator().cmp(entry.key, end) != std::cmp::Ordering::Less
                {
                    return Err(Error::InvalidArgument(format!(
                        "delete_range({:02x?}, {:02x?}) covers nothing: an end at or below \
                         the begin is a caller error rather than a no-op, because the two \
                         readings differ and only one of them is what the caller meant \
                         (docs/adr/0017-range-tombstones.md)",
                        entry.key, end
                    )));
                }
            }
        }
        Ok(())
    }
}
