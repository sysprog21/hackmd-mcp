//! Local-first sync: the tracked state store and the tools that move a note
//! between `HackMD` and a local Markdown file.

pub(crate) mod check;
pub(crate) mod pull;
pub(crate) mod push;
pub(crate) mod snapshot;
pub(crate) mod state;

/// Note body sizes the sync tools accept. Above the warning size a caller must
/// pass `confirm_large_file`; above the maximum the body is refused outright,
/// because both ends of a sync have to hold the whole body in memory.
pub(crate) const BODY_WARNING_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const BODY_MAX_BYTES: usize = 50 * 1024 * 1024;
