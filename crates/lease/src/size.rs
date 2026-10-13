//! Memory sizes as people write them: `512MiB`, `512M`, `2g`.
//! Units are binary throughout, as Podman and Docker read them.

pub fn parse_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits.parse().ok()?;
    let shift = match unit.trim() {
        "" | "B" | "b" => 0,
        "K" | "k" | "KiB" | "kib" => 10,
        "M" | "m" | "MiB" | "mib" => 20,
        "G" | "g" | "GiB" | "gib" => 30,
        _ => return None,
    };
    n.checked_mul(1u64 << shift)
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [(u32, &str); 3] = [(30, "GiB"), (20, "MiB"), (10, "KiB")];
    for (shift, unit) in UNITS {
        if n >= 1 << shift && n.is_multiple_of(1 << shift) {
            return format!("{}{unit}", n >> shift);
        }
    }
    format!("{n}B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_binary_units() {
        assert_eq!(parse_bytes("512MiB"), Some(512 << 20));
        assert_eq!(parse_bytes("512M"), Some(512 << 20));
        assert_eq!(parse_bytes("2g"), Some(2 << 30));
        assert_eq!(parse_bytes("64 KiB"), Some(64 << 10));
        assert_eq!(parse_bytes("1024"), Some(1024));
    }

    #[test]
    fn refuses_nonsense() {
        assert_eq!(parse_bytes(""), None);
        assert_eq!(parse_bytes("MiB"), None);
        assert_eq!(parse_bytes("1.5G"), None);
        assert_eq!(parse_bytes("12 parsecs"), None);
        assert_eq!(parse_bytes("99999999999G"), None);
    }

    #[test]
    fn formats_round_trip() {
        assert_eq!(format_bytes(512 << 20), "512MiB");
        assert_eq!(format_bytes(3 << 30), "3GiB");
        assert_eq!(format_bytes(1000), "1000B");
    }
}
