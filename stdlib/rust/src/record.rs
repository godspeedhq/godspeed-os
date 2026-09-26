// SPDX-License-Identifier: Apache-2.0
//! Typed records: one table, and every rendering derived from it.
//!
//! # Why this is a re-export and not a wrapper
//!
//! Everything else in this crate wraps the SDK because there is something to translate: a capability
//! to hold, an error to map onto one type, a protocol to hide. A [`Table`] has none of that. It is a
//! bounded arena and some arithmetic - no syscall, no capability, no failure mode that a program
//! should be told about differently. Wrapping twenty-three methods to change nothing would be exactly the
//! speculative layer CLAUDE.md 26.2 warns against, and it would add a second name for one thing
//! (Commandment III).
//!
//! So these are the SDK's types, re-exported here so that a program written against `gs` never has
//! to name the lower layer to use them.
//!
//! # The one idea worth carrying away
//!
//! **The table is the truth; the grid and the JSON are views of it** (Commandment III). Build the
//! table once, then render. Do not build a grid and a JSON separately from the same loop - the moment
//! there are two constructions of one fact, they can disagree, and the one somebody reads will be
//! whichever you did not check.
//!
//! ```ignore
//! let mut t = gs::record::Table::new(&["name", "size"]);
//! let n = t.intern(b"kernel.elf");
//! t.add_row(&[n, gs::record::Value::Int(1930096)]);
//! t.sort("size", true);
//! t.to_grid(&mut sink);            // one view
//! t.to_json(&mut sink);            // another view of the SAME table
//! ```
//!
//! # Bounded, and it will tell you
//!
//! Fixed capacity, no heap (CLAUDE.md 26.6.1): [`REC_MAX_ROWS`] rows, [`REC_MAX_COLS`] columns, and a
//! [`REC_ARENA`]-byte string arena. Past that, rows are dropped and [`Table::overflow`] returns true
//! - which is there to be CHECKED. A truncated table that renders cleanly is a report that lies by
//! omission, and the flag is how you avoid printing one.

pub use godspeed_sdk::record::{
    parse_predicate, AggErr, AggOp, RecordSink, Table, Value, REC_ARENA, REC_COL_NAME, REC_MAX_COLS,
    REC_MAX_ROWS,
};
