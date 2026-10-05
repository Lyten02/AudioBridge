//! Pure handling of the `setPeers(urisJson)` / `setMuted(idsJson)` arguments.

pub use audiobridge_core::session::MAX_PEERS;

/// Parses a JSON array of strings sent by Kotlin (pairing URIs or peer ids).
pub fn parse_string_list(json: &str) -> Result<Vec<String>, String> {
    serde_json::from_str::<Vec<String>>(json).map_err(|e| format!("expected a JSON array of strings: {e}"))
}

/// Removes entries with a duplicate key. A later entry replaces an earlier one (re-scanning a PC refreshes its
/// addresses) but keeps the earlier position. The result is capped at [`MAX_PEERS`].
pub fn dedupe<T, K: PartialEq>(items: impl IntoIterator<Item = T>, key: impl Fn(&T) -> K) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in items {
        let k = key(&item);
        match out.iter().position(|o| key(o) == k) {
            Some(i) => out[i] = item,
            None => out.push(item),
        }
    }
    out.truncate(MAX_PEERS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_array_and_rejects_other_json() {
        assert_eq!(parse_string_list(r#"["a","b"]"#).unwrap(), ["a", "b"]);
        assert!(parse_string_list("[]").unwrap().is_empty());
        assert!(parse_string_list(r#"{"a":1}"#).is_err());
        assert!(parse_string_list(r#"["a",1]"#).is_err());
        assert!(parse_string_list("").is_err());
    }

    #[test]
    fn later_duplicate_replaces_earlier_in_place() {
        let got = dedupe([("a", 1), ("b", 1), ("a", 2), ("c", 1)], |p| p.0);
        assert_eq!(got, [("a", 2), ("b", 1), ("c", 1)]);
    }

    #[test]
    fn caps_at_max_peers() {
        let got = dedupe(0..20, |v| *v);
        assert_eq!(got, (0..MAX_PEERS as i32).collect::<Vec<_>>());
    }
}
