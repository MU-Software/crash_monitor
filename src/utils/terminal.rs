//! Escaping for untrusted strings at terminal-rendering boundaries.

use std::fmt::Write as _;

/// Return a printable representation that cannot emit terminal control
/// sequences. This is deliberately separate from JSON escaping: stored report
/// values remain unchanged and only human-facing terminal output is escaped.
#[must_use]
pub fn escape_terminal(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if !character.is_control() {
            output.push(character);
            continue;
        }
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            control if u32::from(control) <= 0xff => {
                let _ = write!(output, "\\x{:02x}", u32::from(control));
            }
            control => {
                let _ = write!(output, "\\u{{{:x}}}", u32::from(control));
            }
        }
    }
    output
}

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
