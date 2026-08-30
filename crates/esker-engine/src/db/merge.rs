//! Merging several sorted cursors into one.
//!
//! A read at the `Db` surface has to look at the active memtable, every immutable one, every
//! overlapping L0 file and one file per level below — all sorted by the same internal
//! comparator, and all needing to be walked as a single sequence. That is this.
//!
//! # Direction is state, and it is the hard part
//!
//! Each child is positioned at *its* next entry in the direction of travel. Turning around
//! therefore invalidates every child except the current one: they are all sitting just after
//! the key we are on, when reversing needs them just before it. So a change of direction
//! re-seeks every other child, and that re-seek is the only interesting code here. Forgetting
//! it makes `prev` after `next` return the entry it just returned, or skip one — a bug that
//! only shows up in a mixed-direction scan.
//!
//! # An unreadable child ends iteration, so its status is kept
//!
//! A child that could not read a block goes invalid, exactly as one that ran off the end does.
//! [`MergeCursor::status`] reports the first such failure so that a caller never mistakes an
//! unreadable file for an exhausted one.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::dbformat::{Comparator, InternalKeyComparator};
use crate::error::Result;
use crate::iterator::Cursor;

/// Which way the cursor is travelling. Children are positioned for this direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Reverse,
}

/// One cursor over several sorted children.
pub struct MergeCursor {
    children: Vec<Box<dyn Cursor + Send>>,
    comparator: Arc<InternalKeyComparator>,
    current: Option<usize>,
    direction: Direction,
}

impl std::fmt::Debug for MergeCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeCursor")
            .field("children", &self.children.len())
            .field("current", &self.current)
            .field("direction", &self.direction)
            .finish_non_exhaustive()
    }
}

impl MergeCursor {
    /// Merges `children`, which must all be sorted by `comparator`.
    pub fn new(
        children: Vec<Box<dyn Cursor + Send>>,
        comparator: Arc<InternalKeyComparator>,
    ) -> Self {
        Self {
            children,
            comparator,
            current: None,
            direction: Direction::Forward,
        }
    }

    /// How many children are being merged. A read consults this many sources.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    fn find_smallest(&mut self) {
        let mut best: Option<usize> = None;
        for index in 0..self.children.len() {
            if !self.children[index].valid() {
                continue;
            }
            match best {
                None => best = Some(index),
                Some(current) => {
                    if self
                        .comparator
                        .cmp(self.children[index].key(), self.children[current].key())
                        == Ordering::Less
                    {
                        best = Some(index);
                    }
                }
            }
        }
        self.current = best;
    }

    fn find_largest(&mut self) {
        let mut best: Option<usize> = None;
        for index in (0..self.children.len()).rev() {
            if !self.children[index].valid() {
                continue;
            }
            match best {
                None => best = Some(index),
                Some(current) => {
                    if self
                        .comparator
                        .cmp(self.children[index].key(), self.children[current].key())
                        == Ordering::Greater
                    {
                        best = Some(index);
                    }
                }
            }
        }
        self.current = best;
    }

    /// Re-seeks every child but the current one so they sit just after the current key.
    fn face_forward(&mut self) {
        let Some(current) = self.current else {
            return;
        };
        let key = self.children[current].key().to_vec();
        for index in 0..self.children.len() {
            if index == current {
                continue;
            }
            let child = &mut self.children[index];
            child.seek(&key);
            // `seek` lands at or after the key; a child sitting exactly on it has already been
            // reported through `current`, so step past it.
            if child.valid() && self.comparator.cmp(child.key(), &key) == Ordering::Equal {
                child.next();
            }
        }
        self.direction = Direction::Forward;
    }

    /// Re-seeks every child but the current one so they sit just before the current key.
    fn face_backward(&mut self) {
        let Some(current) = self.current else {
            return;
        };
        let key = self.children[current].key().to_vec();
        for index in 0..self.children.len() {
            if index == current {
                continue;
            }
            let child = &mut self.children[index];
            child.seek_for_prev(&key);
            if child.valid() && self.comparator.cmp(child.key(), &key) == Ordering::Equal {
                child.prev();
            }
        }
        self.direction = Direction::Reverse;
    }
}

impl Cursor for MergeCursor {
    fn valid(&self) -> bool {
        self.current.is_some()
    }

    fn key(&self) -> &[u8] {
        match self.current {
            Some(index) => self.children[index].key(),
            None => &[],
        }
    }

    fn value(&self) -> &[u8] {
        match self.current {
            Some(index) => self.children[index].value(),
            None => &[],
        }
    }

    fn seek(&mut self, target: &[u8]) {
        for child in &mut self.children {
            child.seek(target);
        }
        self.direction = Direction::Forward;
        self.find_smallest();
    }

    fn seek_for_prev(&mut self, target: &[u8]) {
        for child in &mut self.children {
            child.seek_for_prev(target);
        }
        self.direction = Direction::Reverse;
        self.find_largest();
    }

    fn seek_to_first(&mut self) {
        for child in &mut self.children {
            child.seek_to_first();
        }
        self.direction = Direction::Forward;
        self.find_smallest();
    }

    fn seek_to_last(&mut self) {
        for child in &mut self.children {
            child.seek_to_last();
        }
        self.direction = Direction::Reverse;
        self.find_largest();
    }

    fn next(&mut self) {
        if self.direction != Direction::Forward {
            self.face_forward();
        }
        if let Some(index) = self.current {
            self.children[index].next();
        }
        self.find_smallest();
    }

    fn prev(&mut self) {
        if self.direction != Direction::Reverse {
            self.face_backward();
        }
        if let Some(index) = self.current {
            self.children[index].prev();
        }
        self.find_largest();
    }

    fn status(&self) -> Result<()> {
        for child in &self.children {
            child.status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MergeCursor;
    use crate::dbformat::{
        BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, internal_key,
    };
    use crate::error::{Error, Result};
    use crate::iterator::Cursor;
    use std::sync::Arc;

    /// A cursor over a fixed, sorted list, so the merge logic can be tested on its own.
    #[derive(Debug)]
    struct Fixed {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        position: Option<usize>,
        comparator: Arc<InternalKeyComparator>,
        broken: bool,
    }

    impl Fixed {
        fn new(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Box<dyn Cursor + Send> {
            Box::new(Self {
                entries,
                position: None,
                comparator: comparator(),
                broken: false,
            })
        }

        fn broken() -> Box<dyn Cursor + Send> {
            Box::new(Self {
                entries: Vec::new(),
                position: None,
                comparator: comparator(),
                broken: true,
            })
        }
    }

    impl Cursor for Fixed {
        fn valid(&self) -> bool {
            self.position.is_some()
        }
        fn key(&self) -> &[u8] {
            &self.entries[self.position.unwrap()].0
        }
        fn value(&self) -> &[u8] {
            &self.entries[self.position.unwrap()].1
        }
        fn seek(&mut self, target: &[u8]) {
            self.position = self
                .entries
                .iter()
                .position(|(key, _)| self.comparator.cmp(key, target) != std::cmp::Ordering::Less);
        }
        fn seek_for_prev(&mut self, target: &[u8]) {
            self.position = self.entries.iter().rposition(|(key, _)| {
                self.comparator.cmp(key, target) != std::cmp::Ordering::Greater
            });
        }
        fn seek_to_first(&mut self) {
            self.position = (!self.entries.is_empty()).then_some(0);
        }
        fn seek_to_last(&mut self) {
            self.position = self.entries.len().checked_sub(1);
        }
        fn next(&mut self) {
            self.position = self
                .position
                .and_then(|p| (p + 1 < self.entries.len()).then_some(p + 1));
        }
        fn prev(&mut self) {
            self.position = self.position.and_then(|p| p.checked_sub(1));
        }
        fn status(&self) -> Result<()> {
            if self.broken {
                return Err(Error::corruption("test", "a block could not be read"));
            }
            Ok(())
        }
    }

    fn comparator() -> Arc<InternalKeyComparator> {
        Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)))
    }

    fn entry(key: &str, seqno: u64, value: &str) -> (Vec<u8>, Vec<u8>) {
        (
            internal_key(key.as_bytes(), seqno, EntryKind::Put),
            value.as_bytes().to_vec(),
        )
    }

    fn merged() -> MergeCursor {
        MergeCursor::new(
            vec![
                Fixed::new(vec![entry("a", 3, "a3"), entry("c", 3, "c3")]),
                Fixed::new(vec![entry("a", 2, "a2"), entry("b", 2, "b2")]),
                Fixed::new(vec![entry("b", 1, "b1"), entry("d", 1, "d1")]),
            ],
            comparator(),
        )
    }

    fn walk_forward(cursor: &mut MergeCursor) -> Vec<String> {
        let mut out = Vec::new();
        cursor.seek_to_first();
        while cursor.valid() {
            out.push(String::from_utf8_lossy(cursor.value()).into_owned());
            cursor.next();
        }
        out
    }

    #[test]
    fn forward_order_is_the_internal_order() {
        let mut cursor = merged();
        assert_eq!(cursor.child_count(), 3);
        assert_eq!(
            walk_forward(&mut cursor),
            ["a3", "a2", "b2", "b1", "c3", "d1"]
        );
    }

    #[test]
    fn backward_order_is_the_reverse() {
        let mut cursor = merged();
        let mut out = Vec::new();
        cursor.seek_to_last();
        while cursor.valid() {
            out.push(String::from_utf8_lossy(cursor.value()).into_owned());
            cursor.prev();
        }
        assert_eq!(out, ["d1", "c3", "b1", "b2", "a2", "a3"]);
    }

    /// Turning around leaves every child sitting on the wrong side of the current key. This is
    /// the case that breaks if `face_forward` and `face_backward` are missing.
    #[test]
    fn changing_direction_lands_on_the_neighbour() {
        let mut cursor = merged();
        cursor.seek_to_first();
        for _ in 0..3 {
            cursor.next();
        }
        assert_eq!(cursor.value(), b"b1", "forward to the fourth entry");

        cursor.prev();
        assert_eq!(cursor.value(), b"b2", "one step back is the previous entry");
        cursor.next();
        assert_eq!(cursor.value(), b"b1", "and forward again returns to it");
        cursor.prev();
        cursor.prev();
        assert_eq!(cursor.value(), b"a2");
        cursor.next();
        assert_eq!(cursor.value(), b"b2");
    }

    #[test]
    fn seeking_lands_between_children() {
        let mut cursor = merged();
        cursor.seek(&internal_key(b"b", 100, EntryKind::Put));
        assert_eq!(cursor.value(), b"b2", "the newest entry for b");

        cursor.seek_for_prev(&internal_key(b"b", 0, EntryKind::Delete));
        assert_eq!(cursor.value(), b"b1", "the oldest entry for b");

        cursor.seek(&internal_key(b"z", 0, EntryKind::Delete));
        assert!(!cursor.valid());
        cursor.seek_for_prev(&internal_key(b"", 0, EntryKind::Delete));
        assert!(!cursor.valid());
    }

    #[test]
    fn an_empty_merge_is_valid_and_empty() {
        let mut cursor = MergeCursor::new(Vec::new(), comparator());
        cursor.seek_to_first();
        assert!(!cursor.valid());
        cursor.next();
        assert!(!cursor.valid());
        cursor.prev();
        assert!(!cursor.valid());
        cursor.status().unwrap();
    }

    /// A child that failed to read must not look like a child that ran out.
    #[test]
    fn a_broken_child_is_reported_rather_than_read_as_the_end() {
        let mut cursor = MergeCursor::new(
            vec![Fixed::new(vec![entry("a", 1, "a1")]), Fixed::broken()],
            comparator(),
        );
        assert_eq!(walk_forward(&mut cursor), ["a1"]);
        assert!(
            cursor.status().is_err(),
            "the failure must survive iteration"
        );
    }
}
