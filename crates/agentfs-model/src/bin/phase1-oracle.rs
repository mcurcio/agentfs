//! Fixed phase 1 mounted conformance cases driven by the existing model.
//!
//! Each case starts with its base, forks, saves the child then the upstream,
//! and drains their queues. Output is JSON with byte strings encoded as hex.
//! This runs only the named cases, not the model's random or verification suites.
use agentfs_model::calls::{Client, TreeId};
use agentfs_model::git::git;
use agentfs_model::recording::Write;
use agentfs_model::world::{MODEL_ROOT, Policies, TreeNames, TreeStep, World};
use std::fmt::Write as _;

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut result, "{b:02x}").expect("write to a string");
    }
    result
}

fn save(world: &mut World, client: &Client, bytes: &str) {
    let handle = world.open_trunc(client, "a");
    world.write(
        handle,
        Write::Pwrite {
            offset: 0,
            bytes: bytes.into(),
        },
    );
    world.close(handle);
}

fn finding_32() {
    let mut w = World::new(Policies::default(), TreeNames::new(MODEL_ROOT));
    let root = Client::Root("m1".into());
    let base = w.read(TreeId::ROOT, "c").bytes();
    let f = w.create_fork(TreeId::ROOT);
    let child = Client::Fork(f);
    let cut = w.open_trunc(&child, "c");
    w.write(
        cut,
        Write::Pwrite {
            offset: 0,
            bytes: "y".into(),
        },
    );
    w.close(cut);
    w.save(&root, "c", 1);
    let parent = w.read(TreeId::ROOT, "c").bytes();
    let edit = w.open_write(&child, "c");
    w.write(
        edit,
        Write::Pwrite {
            offset: 0,
            bytes: "n6".into(),
        },
    );
    w.close(edit);
    for _ in 0..2 {
        w.take_step(&TreeStep::Next(TreeId::ROOT));
    }
    let content = w.read(TreeId::ROOT, "c");
    let lines = content
        .live()
        .map(|line| format!("\"{}\"", hex(line.text().expect("live line").as_bytes())))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{{\"base\":\"{}\",\"parent\":\"{}\",\"root\":\"{}\",\"live_lines\":[{}]}}",
        hex(base.as_bytes()),
        hex(parent.as_bytes()),
        hex(content.bytes().as_bytes()),
        lines
    );
}

fn p1_append() {
    let mut w = World::new(Policies::default(), TreeNames::new(MODEL_ROOT));
    let root = Client::Root("m1".into());
    save(&mut w, &root, "base\n");
    let f = w.create_fork(TreeId::ROOT);
    let held = w.open_write(&root, "a");
    save(&mut w, &Client::Fork(f), "sandbox-log\n");
    w.take_step(&TreeStep::Next(TreeId::ROOT));
    let append = w.open_write(&Client::Root("m2".into()), "a");
    w.write(append, Write::Append("child\n".into()));
    w.close(append);
    w.close(held);
    w.settle_trees();
    println!(
        "{{\"root\":\"{}\"}}",
        hex(w.read(TreeId::ROOT, "a").bytes().as_bytes())
    );
}

fn main() {
    let name = std::env::args().nth(1).expect("supply a case name");
    match name.as_str() {
        "finding-32" => {
            finding_32();
            return;
        }
        "p1-append" => {
            p1_append();
            return;
        }
        _ => {}
    }
    let (base, parent, child) = match name.as_str() {
        "loss" => ("a0\na1\na2\n", "root\na1\na2\n", "child\na1\nclean\n"),
        "neighbors" => ("a0\na1\na2\n", "root\na1\na2\n", "a0\nchild\na2\n"),
        "inserts" => ("a0\na1\n", "a0\nroot\na1\n", "a0\nchild\na1\n"),
        "unfinished" => ("base\n", "base\n}", "base\nchild\n"),
        "edited-place" => ("a0\na1\n", "a0\na1\nparent\n", "a0\nchild\n"),
        _ => panic!("unknown case: {name}"),
    };
    let mut world = World::new(Policies::default(), TreeNames::new(MODEL_ROOT));
    let root = Client::Root("m1".into());
    save(&mut world, &root, base);
    let fork = world.create_fork(TreeId::ROOT);
    save(&mut world, &Client::Fork(fork), child);
    let child_head = world.head(fork);
    save(&mut world, &root, parent);
    let parent_head = world.head(TreeId::ROOT);
    world.settle_trees();
    let history = git(
        world.repo(),
        &["rev-list", "--first-parent", &world.head(TreeId::ROOT)],
    );
    let history = history
        .lines()
        .map(|commit| format!("\"{}\"", hex(world.file(commit, "a").bytes().as_bytes())))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{{\"root\":\"{}\",\"child\":\"{}\",\"collisions\":{},\"parent_reachable\":{},\"child_reachable\":{},\"root_history\":[{}]}}",
        hex(world.read(TreeId::ROOT, "a").bytes().as_bytes()),
        hex(world.read(fork, "a").bytes().as_bytes()),
        world.collisions.len(),
        world.is_ancestor(&parent_head, &world.head(TreeId::ROOT)),
        world.is_ancestor(&child_head, &world.head(TreeId::ROOT)),
        history
    );
}
