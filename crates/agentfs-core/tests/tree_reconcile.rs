use agentfs_core::tree_reconcile::LineMap;

fn save(base: &LineMap, bytes: &[u8], prefix: &str) -> LineMap {
    let mut n = 0;
    base.align(bytes, || {
        n += 1;
        format!("{prefix}{n}")
    })
    .unwrap()
}

#[test]
fn alignment_retains_identity_and_exact_binary_bytes() {
    let root = save(&LineMap::default(), b"a\0b\nrepeat\nrepeat\ntail", "r");
    let edited = save(&root, b"a\0b\ninsert\nrepeat\nrepeat\ntail!", "e");
    assert_eq!(edited.bytes(), b"a\0b\ninsert\nrepeat\nrepeat\ntail!");
    assert_eq!(root.lines[0], edited.lines[0]);
    assert_eq!(root.lines[1], edited.lines[2]);
    assert_eq!(root.lines[2], edited.lines[3]);
    assert_eq!(root.lines[3].identity, edited.lines[4].identity);
    assert_eq!(
        edited.ancestors[&edited.lines[4].version],
        Some(root.lines[3].version.clone())
    );
    let decoded = LineMap::decode(&edited.encode().unwrap()).unwrap();
    decoded
        .validate(b"a\0b\ninsert\nrepeat\nrepeat\ntail!")
        .unwrap();
    assert_eq!(decoded, edited);
}
