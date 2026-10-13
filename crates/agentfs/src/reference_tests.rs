//! Checks docs/configuration.md against the CLI (design §17.1): every flag
//! of every subcommand, with its environment variable, and every `OTEL_*`
//! variable agentfs reads.
//!
//! The flags live in this binary's clap structs, so only a unit test of
//! the binary can walk them (`Cli::command()`). The page's conventions are
//! stated at its top: one row per flag in the tables of "Flags and
//! environment", the flag as a code span starting the first cell
//! (`` `--state DIR` ``), its variable as a code span in the row, and the
//! subcommands in the Commands column as code spans (`` `patch submit` ``),
//! or `all` for the global flags.

use clap::CommandFactory;

use crate::Cli;
use crate::doc_tables::{self, Section};

/// A long flag of a (sub)command.
struct Flag {
    /// The subcommand path, `""` for the top level (`"patch submit"`).
    command: String,
    long: String,
    env: Option<String>,
    global: bool,
}

fn flags(cmd: &clap::Command, path: &str, out: &mut Vec<Flag>) {
    for arg in cmd.get_arguments() {
        let Some(long) = arg.get_long() else { continue };
        if matches!(long, "help" | "version") {
            continue;
        }
        // Global flags are listed once, at the top level.
        if arg.is_global_set() && !path.is_empty() {
            continue;
        }
        out.push(Flag {
            command: path.to_owned(),
            long: long.to_owned(),
            env: arg.get_env().map(|e| e.to_string_lossy().into_owned()),
            global: arg.is_global_set(),
        });
    }
    for sub in cmd.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let path = if path.is_empty() {
            sub.get_name().to_owned()
        } else {
            format!("{path} {}", sub.get_name())
        };
        flags(sub, &path, out);
    }
}

/// The long flags a first cell names: code spans whose first word starts
/// with `--`.
pub(crate) fn named_flags(cell: &str) -> Vec<String> {
    doc_tables::code_spans(cell)
        .iter()
        .filter_map(|span| {
            let word = span.split_whitespace().next()?;
            word.strip_prefix("--").map(str::to_owned)
        })
        .collect()
}

fn doc() -> Vec<Section> {
    doc_tables::sections(&doc_tables::read_doc(
        env!("CARGO_MANIFEST_DIR"),
        "configuration.md",
    ))
}

fn flag_tables(sections: &[Section]) -> &Section {
    doc_tables::section(sections, "Flags and environment")
        .expect("docs/configuration.md has no `## Flags and environment` section")
}

#[test]
fn every_flag_is_documented() {
    let sections = doc();
    let flags_section = flag_tables(&sections);
    let mut all = Vec::new();
    flags(&Cli::command(), "", &mut all);
    assert!(all.len() > 20, "the walk found too few flags");
    let mut missing = Vec::new();
    for flag in &all {
        let documented = flags_section.tables.iter().any(|table| {
            let commands = table.column("Commands");
            table.rows.iter().any(|row| {
                if !named_flags(&row[0]).contains(&flag.long) {
                    return false;
                }
                let names_command = match commands.and_then(|c| row.get(c)) {
                    None => false,
                    Some(cell) if flag.global => cell.split([',', ' ', ':']).any(|w| w == "all"),
                    Some(cell) => doc_tables::code_spans(cell).contains(&flag.command),
                };
                let names_env = flag.env.as_ref().is_none_or(|env| {
                    row.iter()
                        .any(|cell| doc_tables::code_spans(cell).contains(env))
                });
                names_command && names_env
            })
        });
        if !documented {
            let command = if flag.global {
                "all".to_owned()
            } else {
                format!("`{}`", flag.command)
            };
            missing.push(format!(
                "--{} (command {command}{})",
                flag.long,
                flag.env
                    .as_ref()
                    .map(|e| format!(", environment `{e}`"))
                    .unwrap_or_default()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "docs/configuration.md, \"Flags and environment\": add a row (or the command \
         or variable to an existing row) for:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn every_documented_flag_exists() {
    let sections = doc();
    let mut all = Vec::new();
    flags(&Cli::command(), "", &mut all);
    let stale: Vec<String> = flag_tables(&sections)
        .rows()
        .flat_map(|row| named_flags(&row[0]))
        .filter(|long| !all.iter().any(|f| f.long == *long))
        .collect();
    assert!(
        stale.is_empty(),
        "docs/configuration.md documents flags agentfs does not have: {stale:?}"
    );
}

#[test]
fn every_environment_variable_is_documented() {
    let sections = doc();
    let section = doc_tables::section(&sections, "Environment variables")
        .expect("docs/configuration.md has no `## Environment variables` section");
    let spans = doc_tables::code_spans(&section.text);
    let mut all = Vec::new();
    flags(&Cli::command(), "", &mut all);
    let missing: Vec<&str> = all
        .iter()
        .filter_map(|f| f.env.as_deref())
        .chain(crate::otel::ENV_VARS.iter().copied())
        .chain(crate::otel::SDK_ENV_VARS.iter().copied())
        .filter(|var| !spans.iter().any(|s| s == var))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/configuration.md, \"Environment variables\": list {missing:?}"
    );
}

#[test]
fn named_flags_reads_first_cells() {
    assert_eq!(named_flags("`--state DIR`"), ["state"]);
    assert_eq!(
        named_flags("`--pod NAME`, `--namespace NAME`"),
        ["pod", "namespace"]
    );
    assert_eq!(named_flags("`-m`, `--message TEXT`"), ["message"]);
    assert!(named_flags("`AGENTFS_STATE`").is_empty());
}
