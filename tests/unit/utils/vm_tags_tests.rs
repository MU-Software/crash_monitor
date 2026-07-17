use super::*;

#[test]
fn test_user_tag_label_known() {
    assert_eq!(user_tag_label(1), "malloc");
    assert_eq!(user_tag_label(30), "Stack");
    assert_eq!(user_tag_label(55), "dyld");
}

#[test]
fn test_user_tag_label_unknown() {
    assert_eq!(user_tag_label(999), "");
}

#[test]
fn test_is_malloc_tag() {
    // Tags 1-11 and 56 are malloc family
    for tag in 1..=11 {
        assert!(is_malloc_tag(tag), "tag {tag} should be malloc");
    }
    assert!(is_malloc_tag(56), "tag 56 (dyld_malloc) should be malloc");

    // Non-malloc tags
    assert!(!is_malloc_tag(0), "tag 0 should not be malloc");
    assert!(!is_malloc_tag(12), "tag 12 should not be malloc");
    assert!(!is_malloc_tag(30), "tag 30 (Stack) should not be malloc");
}
