/// Hardcoded ignore rules. Applied to single path components (file or folder names).
pub fn is_ignored(name: &str) -> bool {
    name == ".DS_Store"
        || name.starts_with("._")
        || name.starts_with(".~lock")
        || name.ends_with(".part")
        || name == "Icon\r"
        || name.starts_with(".ncsync-")
        || name.contains(" (conflicted copy ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        let cases = [
            (".DS_Store", true),
            ("x.DS_Store", false),
            ("._foo", true),
            ("_foo", false),
            (".~lock.doc#", true),
            ("movie.part", true),
            ("movie.partial", false),
            ("Icon\r", true),
            ("Icon", false),
            (".ncsync-1234.part", true),
            (".ncsync-x", true),
            ("ncsync-x", false),
            ("a (conflicted copy 2024-01-01 120000).txt", true),
            ("a (conflicted).txt", false),
            ("normal.txt", false),
            (".hidden", false),
        ];
        for (name, want) in cases {
            assert_eq!(is_ignored(name), want, "{name:?}");
        }
    }
}
