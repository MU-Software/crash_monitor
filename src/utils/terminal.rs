//! Escaping for untrusted strings at terminal-rendering boundaries.

// Keep the platform-specific monitor and portable report CLI on the same
// terminal-safety contract.
pub use crash_report_core::escape_terminal;

#[cfg(test)]
mod tests {
    use super::escape_terminal;

    #[test]
    fn escapes_ansi_osc_and_nonprinting_characters() {
        let attack = "name\u{1b}[31m\u{1b}]0;owned\u{7}\nnext\u{7f}";
        let escaped = escape_terminal(attack);

        assert_eq!(escaped, "name\\x1b[31m\\x1b]0;owned\\x07\\nnext\\x7f");
        assert!(!escaped.chars().any(char::is_control));
    }

    #[test]
    fn preserves_printable_unicode_and_json_semantics() {
        let value = "사용자 \"alice\"";
        assert_eq!(escape_terminal(value), value);
        assert_eq!(
            serde_json::to_string(value).unwrap(),
            "\"사용자 \\\"alice\\\"\""
        );
    }

    #[test]
    fn report_controlled_fields_are_escaped_only_for_rendering() {
        let fields = [
            ("annotation", "value\u{1b}[2J"),
            ("thread", "main\u{1b}]0;thread\u{7}"),
            ("attachment", "log\nforged-entry"),
            ("process", "app\rspoof"),
        ];
        for (kind, raw) in fields {
            let stored = serde_json::to_string(raw).unwrap();
            let round_trip: String = serde_json::from_str(&stored).unwrap();
            assert_eq!(round_trip, raw, "JSON meaning changed for {kind}");

            let rendered = escape_terminal(raw);
            assert!(!rendered.chars().any(char::is_control), "unsafe {kind}");
            assert_ne!(rendered, raw, "{kind} was not escaped at rendering");
        }
    }
}
