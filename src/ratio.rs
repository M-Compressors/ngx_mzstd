//! Formatting of the `$zstd_ratio` variable.

/// Formats `bytes_in / bytes_out` with three decimals, truncated, as
/// tokers/zstd-nginx-module does.
pub fn format(bytes_in: u64, bytes_out: u64) -> String {
    let int = bytes_in / bytes_out;
    let frac = bytes_in * 1000 / bytes_out % 1000;

    format!("{int}.{frac:03}")
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn truncates_to_three_decimals() {
        assert_eq!(format(100, 100), "1.000");
        assert_eq!(format(17000, 8000), "2.125");
        assert_eq!(format(1999, 1000), "1.999");
        assert_eq!(format(19999, 10000), "1.999");
        assert_eq!(format(1, 3), "0.333");
        assert_eq!(format(2, 3), "0.666");
        assert_eq!(format(1003, 1), "1003.000");
    }
}
