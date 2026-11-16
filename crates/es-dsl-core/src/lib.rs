//! Completion engine core (no UI or network dependencies).

mod analyze;
mod candidate;
mod complete;
mod fields;
mod format;
mod highlight;
mod json_path;
mod request;
mod safety;
mod schema;

pub use analyze::{Diagnostic, Hover, Severity, diagnostics, hover};
pub use candidate::{Candidate, Kind};
pub use complete::{NoCompletion, candidates_with, complete, complete_with};
pub use fields::{Field, Fields, NoFields, parse_aliases, parse_mapping, parse_names};
pub use format::{Edit, format_body};
pub use highlight::{Token, block_spans, tokens};
pub use json_path::{Cursor, KeyTok, Seg, Slot, cursor_at, keys, path_at};
pub use request::{Block, block_at, split_blocks};
pub use safety::{Danger, danger, is_write};
pub use schema::{Endpoint, Index, OpenKey, Prop, TypeRef, Url, Version, parse_version};
