use agentfs_core::tree_reconcile::{LineMap, Verdict, verdict};

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

#[test]
fn concurrent_edits_lose_and_deletes_agree() {
    let root = save(&LineMap::default(), b"one\ntwo\n", "r");
    let parent = save(&root, b"parent\ntwo\n", "p");
    let child = save(&root, b"child\ntwo\n", "c");
    let mut graph = parent.ancestors.clone();
    graph.extend(child.ancestors.clone());
    assert_eq!(
        verdict(Some(&parent.lines[0]), &child.lines[0], &graph).unwrap(),
        Verdict::Lose {
            winner: parent.lines[0].version.clone()
        }
    );
    let deleted_p = save(&root, b"two\n", "dp");
    let deleted_c = save(&root, b"two\n", "dc");
    graph = deleted_p.ancestors.clone();
    graph.extend(deleted_c.ancestors.clone());
    assert_eq!(
        verdict(Some(&deleted_p.lines[0]), &deleted_c.lines[0], &graph).unwrap(),
        Verdict::Apply
    );
}

#[test]
fn later_write_over_parent_wins_and_old_version_drops() {
    let root = save(&LineMap::default(), b"one\n", "r");
    let parent = save(&root, b"parent\n", "p");
    let later = save(&parent, b"later\n", "l");
    assert_eq!(
        verdict(Some(&parent.lines[0]), &later.lines[0], &later.ancestors).unwrap(),
        Verdict::Apply
    );
    assert_eq!(
        verdict(Some(&later.lines[0]), &parent.lines[0], &later.ancestors).unwrap(),
        Verdict::Drop
    );
}

#[test]
fn one_collision_loses_the_entire_save_and_names_clean_edits() {
    use agentfs_core::tree_reconcile::{CollisionKind, land_save};
    let base = save(&LineMap::default(), b"a\nb\n", "r");
    let parent = save(&base, b"parent\nb\n", "p");
    let child = save(&base, b"child\nclean\n", "c");
    let landing = land_save(&parent, &base, &child).unwrap();
    assert_eq!(landing.content, parent);
    assert_eq!(landing.collisions.len(), 2);
    assert_eq!(landing.collisions[0].kind, CollisionKind::Line);
    assert_eq!(landing.collisions[1].kind, CollisionKind::Whole);
    assert_eq!(landing.collisions[1].lost, child.lines[1].version);
}

#[test]
fn inserts_keep_parent_first_and_edits_keep_their_place() {
    use agentfs_core::tree_reconcile::land_save;
    let base = save(&LineMap::default(), b"a\nz\n", "r");
    let parent = save(&base, b"a\nparent\nz\n", "p");
    let child = save(&base, b"a\nchild\nz\n", "c");
    let landing = land_save(&parent, &base, &child).unwrap();
    assert!(landing.collisions.is_empty());
    assert_eq!(landing.content.bytes(), b"a\nparent\nchild\nz\n");
    let base = save(&LineMap::default(), b"line\n", "x");
    let parent = save(&base, b"line\nappend\n", "y");
    let child = save(&base, b"edited\n", "z");
    let landing = land_save(&parent, &base, &child).unwrap();
    assert!(landing.collisions.is_empty());
    assert_eq!(landing.content.bytes(), b"edited\nappend\n");
}

#[test]
fn unfinished_parent_tail_collides_and_sync_keeps_undecided_child() {
    use agentfs_core::tree_reconcile::{land_save, sync};
    use std::collections::BTreeSet;
    let base = save(&LineMap::default(), b"base\n", "r");
    let parent = save(&base, b"base\nunfinished", "p");
    let child = save(&base, b"base\nchild\n", "c");
    let landing = land_save(&parent, &base, &child).unwrap();
    assert_eq!(landing.content, parent);
    assert_eq!(landing.collisions.len(), 1);
    assert_eq!(landing.collisions[0].winner, parent.lines[1].version);
    let undecided = BTreeSet::from([child.lines[1].version.clone()]);
    let before = sync(&parent, &child, &undecided).unwrap();
    assert_eq!(before.bytes(), b"base\nchild\n");
    let after = sync(&parent, &before, &BTreeSet::new()).unwrap();
    assert_eq!(after.bytes(), b"base\nunfinished");
}

#[test]
fn agreeing_delete_is_carried_and_clean_changes_land() {
    use agentfs_core::tree_reconcile::land_save;
    let base = save(&LineMap::default(), b"a\nb\nc\n", "r");
    let parent = save(&base, b"b\nc\n", "p");
    let child = save(&base, b"b\nchanged\n", "c");
    let landing = land_save(&parent, &base, &child).unwrap();
    assert!(landing.collisions.is_empty());
    assert_eq!(landing.content.bytes(), b"b\nchanged\n");
    assert_eq!(landing.content.lines[0].version, child.lines[0].version);
}

#[test]
fn unfinished_incoming_edit_before_retained_parent_line_loses_whole() {
    use agentfs_core::tree_reconcile::{CollisionKind, land_save};
    let base = save(&LineMap::default(), b"first\nsecond\n", "r");
    let parent = save(&base, b"first\nparent\n", "p");
    let first = save(&base, b"partial", "c");
    assert!(
        !land_save(&parent, &base, &first)
            .unwrap()
            .collisions
            .is_empty()
    );
    let later = save(&first, b"later", "d");
    let result = land_save(&parent, &first, &later).unwrap();
    assert_eq!(result.content, parent);
    assert_eq!(result.collisions.len(), 1);
    assert_eq!(result.collisions[0].kind, CollisionKind::Line);
    assert_eq!(result.collisions[0].winner, parent.lines[1].version);
}

#[test]
fn an_unfinished_save_past_the_parents_lines_lands() {
    use agentfs_core::tree_reconcile::land_save;
    let base = save(&LineMap::default(), b"a\n", "r");
    let parent = save(&base, b"a\nparent\n", "p");
    let child = save(&base, b"a\nchild", "c");
    let landing = land_save(&parent, &base, &child).unwrap();
    assert!(landing.collisions.is_empty());
    assert_eq!(landing.content.bytes(), b"a\nparent\nchild");
}

#[test]
fn sync_hides_parent_lines_after_an_undecided_unfinished_line() {
    use agentfs_core::tree_reconcile::sync;
    use std::collections::BTreeSet;
    let base = save(&LineMap::default(), b"a\nb\n", "r");
    let parent = save(&base, b"a\nb\nm7\n", "p");
    let changed = |child: &LineMap| -> BTreeSet<String> {
        child
            .lines
            .iter()
            .filter(|l| !base.ancestors.contains_key(&l.version))
            .map(|l| l.version.clone())
            .collect()
    };
    let child = save(&base, b"y", "c");
    let before = sync(&parent, &child, &changed(&child)).unwrap();
    assert_eq!(before.bytes(), b"y");
    let after = sync(&parent, &before, &BTreeSet::new()).unwrap();
    assert_eq!(after.bytes(), b"a\nb\nm7\n");
    // An unfinished line past the parent's lines hides nothing.
    let child = save(&base, b"a\nb\nchild", "d");
    assert_eq!(
        sync(&parent, &child, &changed(&child)).unwrap().bytes(),
        b"a\nb\nm7\nchild"
    );
    // Nor does a finished line.
    let child = save(&base, b"y\n", "e");
    assert_eq!(
        sync(&parent, &child, &changed(&child)).unwrap().bytes(),
        b"y\nm7\n"
    );
}
