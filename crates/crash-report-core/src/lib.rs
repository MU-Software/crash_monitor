//! Platform-neutral loading for crash-monitor JSON and ZIP reports.

use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_REPORT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub enum ReportLoadError {
    Io(String),
    InvalidJson(String),
    InvalidArchive(String),
    MissingReport,
    Oversized { bytes: u64 },
}

impl fmt::Display for ReportLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "cannot read report: {error}"),
            Self::InvalidJson(error) => write!(formatter, "invalid report JSON: {error}"),
            Self::InvalidArchive(error) => write!(formatter, "invalid report archive: {error}"),
            Self::MissingReport => formatter.write_str("archive contains no top-level JSON report"),
            Self::Oversized { bytes } => {
                write!(
                    formatter,
                    "report exceeds {MAX_REPORT_BYTES} bytes: {bytes}"
                )
            }
        }
    }
}

impl std::error::Error for ReportLoadError {}

#[derive(Debug)]
pub struct ReportDocument {
    value: serde_json::Value,
}

impl ReportDocument {
    #[must_use]
    pub const fn value(&self) -> &serde_json::Value {
        &self.value
    }

    #[must_use]
    pub fn report_type(&self) -> Option<&str> {
        self.value.pointer("/header/type")?.as_str()
    }

    #[must_use]
    pub fn process(&self) -> Option<&str> {
        self.value.pointer("/header/process")?.as_str()
    }

    #[must_use]
    pub fn pid(&self) -> Option<u64> {
        self.value.pointer("/header/pid")?.as_u64()
    }

    #[must_use]
    pub fn thread(&self, index: usize) -> Option<&serde_json::Value> {
        self.value.get("threads")?.as_array()?.get(index)
    }
}

/// Load a bounded plain JSON report or top-level JSON member from a ZIP.
///
/// # Errors
/// Returns an error for inaccessible, oversized, malformed, or empty inputs.
pub fn load_report(path: &Path) -> Result<ReportDocument, ReportLoadError> {
    let bytes = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        load_zip_report(path)?
    } else {
        read_bounded(File::open(path).map_err(|error| ReportLoadError::Io(error.to_string()))?)?
    };
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| ReportLoadError::InvalidJson(error.to_string()))?;
    Ok(ReportDocument { value })
}

fn read_bounded(mut input: impl Read) -> Result<Vec<u8>, ReportLoadError> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(MAX_REPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ReportLoadError::Io(error.to_string()))?;
    if bytes.len() as u64 > MAX_REPORT_BYTES {
        return Err(ReportLoadError::Oversized {
            bytes: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

fn load_zip_report(path: &Path) -> Result<Vec<u8>, ReportLoadError> {
    let file = File::open(path).map_err(|error| ReportLoadError::Io(error.to_string()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| ReportLoadError::InvalidArchive(error.to_string()))?;
    let stem = path.file_stem().and_then(|stem| stem.to_str());
    let mut fallback = None;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| ReportLoadError::InvalidArchive(error.to_string()))?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        if name.components().count() != 1
            || !name
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        if stem.is_some_and(|stem| name.file_stem().and_then(|value| value.to_str()) == Some(stem))
        {
            return read_bounded(entry);
        }
        fallback.get_or_insert(index);
    }
    let index = fallback.ok_or(ReportLoadError::MissingReport)?;
    let entry = archive
        .by_index(index)
        .map_err(|error| ReportLoadError::InvalidArchive(error.to_string()))?;
    read_bounded(entry)
}

/// Return a printable representation that cannot emit terminal control
/// sequences.
///
/// This is deliberately separate from JSON escaping: stored report values
/// remain unchanged and only human-facing terminal output is escaped. Callers
/// should add their own structural newlines after escaping individual values.
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
    use super::*;

    #[test]
    fn loads_platform_neutral_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        std::fs::write(
            &path,
            r#"{"header":{"type":"crash","pid":42,"process":"app"},"threads":[]}"#,
        )
        .unwrap();
        let report = load_report(&path).unwrap();
        assert_eq!(report.report_type(), Some("crash"));
        assert_eq!(report.pid(), Some(42));
        assert_eq!(report.process(), Some("app"));
    }

    #[test]
    fn terminal_escape_replaces_every_control_character() {
        let attack = "name\u{1b}[31m\u{1b}]0;owned\u{7}\nnext\tcolumn\rline\u{7f}\u{85}";
        let escaped = escape_terminal(attack);

        assert_eq!(
            escaped,
            "name\\x1b[31m\\x1b]0;owned\\x07\\nnext\\tcolumn\\rline\\x7f\\x85"
        );
        assert!(!escaped.chars().any(char::is_control));
    }

    #[test]
    fn terminal_escape_preserves_printable_unicode() {
        let value = "사용자 \"alice\"";
        assert_eq!(escape_terminal(value), value);
    }
}
