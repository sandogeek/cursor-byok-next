//! Clamps an in-memory count to the narrower integer a protobuf field or SQL
//! column carries.
//!
//! Every bound here is far above any real prompt, context window, byte length or
//! elapsed time, so clamping is a correctness backstop rather than a value a run
//! reaches. The one owner keeps the spelling identical across the call log, the
//! invention stream and the generated state, where five private copies used to
//! disagree on whether an overrun clamps or disappears.

/// `usize` as `i64`, for lengths sent to an `int64` field.
pub(crate) fn i64_from_usize(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `u64` as `i64`, for token counts and indices.
pub(crate) fn i64_from_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// An optional token count as `i64`, keeping absence.
pub(crate) fn option_i64_from_u64(value: Option<u64>) -> Option<i64> {
    value.map(i64_from_u64)
}

/// A wide count — a monotonic clock reading — as `i64`.
pub(crate) fn i64_from_u128(value: u128) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `u64` as `i32`, for an `int32` field whose absence the protocol spells as `0`.
pub(crate) fn i32_from_u64(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// `u64` as an optional `int32`, where a count too large to represent is absent
/// rather than clamped: the field is a limit, and an absent limit is safer.
pub(crate) fn option_i32_from_u64(value: u64) -> Option<i32> {
    i32::try_from(value).ok()
}

/// `usize` as an optional `int32`, for a position field that is a limit Cursor
/// may drop rather than read clamped.
pub(crate) fn option_i32_from_usize(value: usize) -> Option<i32> {
    i32::try_from(value).ok()
}

/// `usize` as `u32`, for a generated `uint32` field.
pub(crate) fn u32_from_usize(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overrun_clamps_instead_of_wrapping() {
        assert_eq!(
            i64_from_usize(usize::MAX),
            i64::try_from(usize::MAX).unwrap_or(i64::MAX)
        );
        assert_eq!(i32_from_u64(u64::MAX), i32::MAX);
        assert_eq!(u32_from_usize(usize::MAX), u32::MAX);
        assert_eq!(option_i32_from_u64(u64::MAX), None);
        assert_eq!(option_i32_from_usize(usize::MAX), None);
    }

    #[test]
    fn an_in_range_value_passes_through() {
        assert_eq!(i64_from_usize(7), 7);
        assert_eq!(i64_from_u64(7), 7);
        assert_eq!(option_i64_from_u64(Some(7)), Some(7));
        assert_eq!(option_i64_from_u64(None), None);
        assert_eq!(i64_from_u128(7), 7);
        assert_eq!(i32_from_u64(7), 7);
        assert_eq!(option_i32_from_u64(7), Some(7));
        assert_eq!(option_i32_from_usize(7), Some(7));
        assert_eq!(u32_from_usize(7), 7);
    }
}
