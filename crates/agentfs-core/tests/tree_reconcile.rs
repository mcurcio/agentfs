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

/// Against an empty old image the alignment's hunks are git's, with no child:
/// each input's every line is new.
#[test]
fn an_empty_old_image_aligns_as_git_does() {
    let inputs: [&[u8]; 9] = [
        b"",
        b"x",
        b"x\n",
        b"a\nb\nc\n",
        b"a\nb\nno newline",
        b"\n",
        b"\n\n\n",
        b"a\0b\nbinary\0\n\xff\xfe",
        b"repeat\nrepeat\nrepeat\n",
    ];
    for new in inputs {
        let ours = agentfs_core::tree_reconcile::histogram(b"", new).unwrap();
        let git = agentfs_align::histogram(b"", new).unwrap();
        assert_eq!(ours, git, "{new:?}");
        let map = save(&LineMap::default(), new, "n");
        assert_eq!(map.bytes(), new);
        assert!(
            map.lines
                .iter()
                .all(|l| map.ancestors[&l.version].is_none())
        );
    }
}
