use super::live_in_group;

#[test]
fn group_membership_ignores_zombies_and_parses_parentheses_in_names() {
    assert!(live_in_group(b"12 (child (with) spaces) S 10 12 10", 12).expect("valid stat"));
    assert!(!live_in_group(b"12 (child) Z 10 12 10", 12).expect("valid stat"));
    assert!(!live_in_group(b"12 (child) S 10 13 10", 12).expect("valid stat"));
    assert!(live_in_group(b"malformed", 12).is_err());
    assert!(live_in_group(b"12 (\xff) S 10 12 10", 12).expect("valid stat"));
}
