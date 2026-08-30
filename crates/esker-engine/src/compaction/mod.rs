//! Compaction: moving data down the levels and dropping what nothing can see.
//!
//! Leveled compaction, as `docs/DESIGN.md` §4.7 describes it.
//!
//! * [`picker`] — which files to compact, and the two kinds of score
//! * [`job`] — the merge itself, and the three rules for dropping an entry

pub mod job;
pub mod picker;

pub use job::{CompactionFilter, CompactionJob, CompactionOutput, CompactionStats, FilterDecision};
pub use picker::{Compaction, Picker};
