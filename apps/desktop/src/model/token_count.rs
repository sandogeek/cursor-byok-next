//! Parses Cursor token-count strings such as `200k` or `1m`.

pub(crate) fn parse_token_count(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(number) = value.strip_suffix('k') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 1_000_000)
    } else {
        (value.as_str(), 1)
    };
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compact_token_counts() {
        assert_eq!(parse_token_count("200k"), Some(200_000));
        assert_eq!(parse_token_count("1m"), Some(1_000_000));
        assert_eq!(parse_token_count("128000"), Some(128_000));
        assert_eq!(parse_token_count("huge"), None);
    }
}
