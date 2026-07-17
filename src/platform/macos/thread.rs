//! Pure helpers for thread name extraction.

/// Extract a thread name from a fixed-size byte buffer (null-terminated C string).
pub(crate) fn extract_thread_name(name_bytes: &[u8; 64]) -> Option<String> {
    let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(64);
    if end == 0 {
        return None;
    }
    String::from_utf8(name_bytes[..end].to_vec())
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/macos/thread_tests.rs"]
mod tests;
