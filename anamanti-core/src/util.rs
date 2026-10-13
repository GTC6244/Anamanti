//! Small, dependency-free helpers shared across integrations.

/// Treat a blank (empty / whitespace-only) optional string as unset, trimming it
/// otherwise. The shared form of the "advertise only with a key" check every
/// integration's provider selection does (`weather::from_config`,
/// `places::from_key`, `directions::from_token`, `recipe::from_search`).
pub fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// A top-level string field of a free-form JSON object, trimmed; empty when absent or
/// not a string. For schema-less JSON (recipe JSON-LD); fixed-schema APIs use
/// `#[derive(Deserialize)]` response structs instead.
pub fn json_str(node: &serde_json::Value, key: &str) -> String {
    node.get(key)
        .and_then(serde_json::Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_str_trims_and_defaults() {
        let v = serde_json::json!({"a": "  x ", "n": 1});
        assert_eq!(json_str(&v, "a"), "x");
        assert_eq!(json_str(&v, "n"), "");
        assert_eq!(json_str(&v, "missing"), "");
    }

    #[test]
    fn non_empty_trims_and_drops_blanks() {
        assert_eq!(non_empty(Some("  k ")), Some("k"));
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }
}
