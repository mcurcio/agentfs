//! Fixed phase 1 mounted conformance cases driven by the existing model.
//!
//! Each case starts with its base, forks, saves the child then the parent,
//! and drains their queues. Output is JSON with byte strings encoded as hex.
//! This runs only the named cases, not the model's random or verification suites.
use agentfs_model::git::git;
use agentfs_model::world::{Client, Policies, Step, Tree, World, Write};
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
    let mut w = World::new(Policies::default());
    let main = Client::Main("m1".into());
    let base = w.read(Tree::Main, "c").bytes();
    let f = w.create_fork(Tree::Main);
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
    w.save(&main, "c", 1);
    let parent = w.read(Tree::Main, "c").bytes();
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
        w.step(&Step::Next(Tree::Main));
        w.step(&Step::Finish(Tree::Main));
    }
    let content = w.read(Tree::Main, "c");
    let lines = content
        .live()
        .map(|line| format!("\"{}\"", hex(line.text().expect("live line").as_bytes())))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{{\"base\":\"{}\",\"parent\":\"{}\",\"main\":\"{}\",\"live_lines\":[{}]}}",
        hex(base.as_bytes()),
        hex(parent.as_bytes()),
        hex(content.bytes().as_bytes()),
        lines
    );
}

fn p1_append() {
    let mut w = World::new(Policies::default());
    let main = Client::Main("m1".into());
    save(&mut w, &main, "base\n");
    let f = w.create_fork(Tree::Main);
    let held = w.open_write(&main, "a");
    save(&mut w, &Client::Fork(f), "sandbox-log\n");
    w.step(&Step::Next(Tree::Main));
    let append = w.open_write(&Client::Main("m2".into()), "a");
    w.write(append, Write::Append("child\n".into()));
    w.close(append);
    w.close(held);
    w.settle();
    println!(
        "{{\"main\":\"{}\"}}",
        hex(w.read(Tree::Main, "a").bytes().as_bytes())
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
        "loss" => ("a0\na1\na2\n", "main\na1\na2\n", "child\na1\nclean\n"),
        "neighbors" => ("a0\na1\na2\n", "main\na1\na2\n", "a0\nchild\na2\n"),
        "inserts" => ("a0\na1\n", "a0\nmain\na1\n", "a0\nchild\na1\n"),
        "unfinished" => ("base\n", "base\n}", "base\nchild\n"),
        "edited-place" => ("a0\na1\n", "a0\na1\nparent\n", "a0\nchild\n"),
        _ => panic!("unknown case: {name}"),
    };
    let mut world = World::new(Policies::default());
    let main = Client::Main("m1".into());
    save(&mut world, &main, base);
    let fork = world.create_fork(Tree::Main);
    save(&mut world, &Client::Fork(fork), child);
    let child_head = world.head(Tree::Fork(fork));
    save(&mut world, &main, parent);
    let parent_head = world.head(Tree::Main);
    world.settle();
    let history = git(
        world.repo(),
        &["rev-list", "--first-parent", &world.main_head],
    );
    let history = history
        .lines()
        .map(|commit| format!("\"{}\"", hex(world.file(commit, "a").bytes().as_bytes())))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "{{\"main\":\"{}\",\"child\":\"{}\",\"collisions\":{},\"parent_reachable\":{},\"child_reachable\":{},\"main_history\":[{}]}}",
        hex(world.read(Tree::Main, "a").bytes().as_bytes()),
        hex(world.read(Tree::Fork(fork), "a").bytes().as_bytes()),
        world.collisions.len(),
        world.is_ancestor(&parent_head, &world.main_head),
        world.is_ancestor(&child_head, &world.main_head),
        history
    );
}
