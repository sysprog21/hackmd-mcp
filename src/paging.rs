//! Local pagination shared by every list tool.
//!
//! `HackMD`'s list endpoints answer with the whole collection and accept no
//! paging parameters, so the server fetches everything and cuts the page here.

use serde::Serialize;
use thiserror::Error;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

pub(crate) const fn default_limit() -> usize {
    DEFAULT_LIMIT
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("limit must be between 1 and {MAX_LIMIT}")]
pub(crate) struct InvalidLimit;

pub(crate) fn validate_limit(limit: usize) -> Result<(), InvalidLimit> {
    if (1..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(InvalidLimit)
    }
}

/// The counters every list tool reports alongside its page of items.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct PageMeta {
    pub(crate) total: usize,
    pub(crate) count: usize,
    pub(crate) offset: usize,
    pub(crate) has_more: bool,
    pub(crate) next_offset: Option<usize>,
}

/// Cuts one page out of a fully materialized list.
pub(crate) fn paginate<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, PageMeta) {
    let total = items.len();
    let items = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let count = items.len();
    let end = offset.saturating_add(count);
    let has_more = end < total;
    (
        items,
        PageMeta {
            total,
            count,
            offset,
            has_more,
            next_offset: has_more.then_some(end),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{InvalidLimit, MAX_LIMIT, paginate, validate_limit};

    #[test]
    fn limits_outside_the_documented_range_are_rejected() {
        assert_eq!(validate_limit(0), Err(InvalidLimit));
        assert_eq!(validate_limit(MAX_LIMIT + 1), Err(InvalidLimit));
        assert_eq!(validate_limit(1), Ok(()));
        assert_eq!(validate_limit(MAX_LIMIT), Ok(()));
    }

    #[test]
    fn paging_reports_the_next_offset_only_while_items_remain() {
        let (items, meta) = paginate((0..5).collect(), 1, 2);
        assert_eq!(items, vec![1, 2]);
        assert_eq!((meta.total, meta.count, meta.offset), (5, 2, 1));
        assert_eq!(meta.next_offset, Some(3));

        let (items, meta) = paginate((0..5).collect(), 3, 10);
        assert_eq!(items, vec![3, 4]);
        assert!(!meta.has_more);
        assert_eq!(meta.next_offset, None);

        // An offset past the end is a legal empty page, not an error.
        let (items, meta) = paginate((0..5).collect::<Vec<u8>>(), 99, 10);
        assert!(items.is_empty());
        assert!(!meta.has_more);
    }
}
