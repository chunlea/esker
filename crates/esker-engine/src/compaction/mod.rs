//! Compaction: moving data down the levels and dropping what nothing can see.
//!
//! Leveled compaction, as `docs/DESIGN.md` §4.7 describes it.
//!
//! * [`picker`] — which files to compact, and the two kinds of score

pub mod picker;

pub use picker::{Compaction, Picker};
