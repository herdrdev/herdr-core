// The installable skill is the repo's stub; it points agents to `hc --skill`.
#[test]
fn the_installable_stub_is_a_valid_skill() {
    let stub = include_str!("../../../skills/herdr-core/SKILL.md");
    let mut lines = stub.lines();
    assert_eq!(lines.next(), Some("---"));
    assert_eq!(lines.next(), Some("name: herdr-core"));
    let description = lines.next().unwrap().strip_prefix("description: ").unwrap();
    let description: String = serde_json::from_str(description).expect("quoted YAML description");
    assert!(!description.is_empty() && description.chars().count() < 1024);
    assert!(description.contains("persistent terminals"));
    assert!(!description.contains("sub-agent"), "host discovery must leave delegation to the host");
    assert_eq!(lines.next(), Some("---"));
    assert!(stub.contains("hc --skill"), "the stub points to the manual");
}
