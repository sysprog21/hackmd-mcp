//! Local-first sync: the tracked state store and the tools that move a note
//! between `HackMD` and a local Markdown file.

pub(crate) mod check;
pub(crate) mod pull;
pub(crate) mod push;
pub(crate) mod snapshot;
pub(crate) mod state;
pub(crate) mod tracking;

/// Note body sizes the sync tools accept. Above the warning size a caller must
/// pass `confirm_large_file`; above the maximum the body is refused outright,
/// because both ends of a sync have to hold the whole body in memory.
pub(crate) const BODY_WARNING_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const BODY_MAX_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeState {
    InSync,
    LocalOnly,
    RemoteOnly,
    Conflict,
}

pub(crate) fn classify_changes(
    baseline: &[u8; 32],
    local: &[u8; 32],
    remote: &[u8; 32],
) -> ChangeState {
    match (local != baseline, remote != baseline) {
        (false, false) => ChangeState::InSync,
        (true, false) => ChangeState::LocalOnly,
        (false, true) => ChangeState::RemoteOnly,
        (true, true) => ChangeState::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::{ChangeState, classify_changes};

    #[test]
    fn classifies_every_three_way_hash_state() {
        let baseline = [0_u8; 32];
        let local = [1_u8; 32];
        let remote = [2_u8; 32];
        for (local_hash, remote_hash, expected) in [
            (&baseline, &baseline, ChangeState::InSync),
            (&local, &baseline, ChangeState::LocalOnly),
            (&baseline, &remote, ChangeState::RemoteOnly),
            (&local, &remote, ChangeState::Conflict),
        ] {
            assert_eq!(
                classify_changes(&baseline, local_hash, remote_hash),
                expected
            );
        }
    }
}
