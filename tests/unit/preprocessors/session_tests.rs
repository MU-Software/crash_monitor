use super::*;

#[test]
fn test_read_session_lock_rfc3339() {
    let tmp_dir = tempfile::tempdir().unwrap();

    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let timestamp = "2026-03-29T10:00:00+09:00";
    let lock_path = tmp_dir.path().join("session.lock");
    std::fs::write(&lock_path, format!("{uuid}\n{timestamp}\n")).unwrap();

    let result = read_session_lock_from(tmp_dir.path());
    assert!(result.is_some(), "should parse RFC3339 session lock");
    let session = result.unwrap();
    assert_eq!(session.id, uuid);
    assert_eq!(session.start, timestamp);
}

#[test]
fn test_read_session_lock_unix_timestamp() {
    let tmp_dir = tempfile::tempdir().unwrap();

    let uuid = "660e8400-e29b-41d4-a716-446655440000";
    let unix_ts = "1711720800"; // 2024-03-29T18:00:00 UTC — well in the past
    let lock_path = tmp_dir.path().join("session.lock");
    std::fs::write(&lock_path, format!("{uuid}\n{unix_ts}\n")).unwrap();

    let result = read_session_lock_from(tmp_dir.path());
    assert!(result.is_some(), "should parse Unix timestamp session lock");
    let session = result.unwrap();
    assert_eq!(session.id, uuid);
    assert_eq!(session.start, unix_ts);
    assert!(
        session.duration_s > 0,
        "duration should be > 0 for past timestamp"
    );
}

#[test]
fn test_read_session_lock_missing_file() {
    let tmp_dir = tempfile::tempdir().unwrap();
    // No session.lock written
    let result = read_session_lock_from(tmp_dir.path());
    assert!(result.is_none(), "missing lock file should return None");
}

#[test]
fn test_read_session_lock_truncated() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let lock_path = tmp_dir.path().join("session.lock");
    // Only one line — missing timestamp
    std::fs::write(&lock_path, "abc-123\n").unwrap();

    let result = read_session_lock_from(tmp_dir.path());
    assert!(result.is_none(), "truncated lock file should return None");
}
