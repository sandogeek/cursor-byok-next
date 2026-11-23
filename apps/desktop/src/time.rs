//! Wall-clock helpers shared by the plugin store, the call records and the
//! backend's generated timestamps.

/// Milliseconds since the Unix epoch, saturating instead of failing when the
/// system clock is set before 1970.
pub(crate) fn now_ms() -> i64 {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}
