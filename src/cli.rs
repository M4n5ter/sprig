//! Small explicit command grammar, without a parser dependency.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use crate::copy_tree::CopyStats;
use crate::store::Store;

const HELP: &str = "Sprig 0.1 — native-filesystem workspaces (Linux)

Usage: sprig --store PATH [--copy] COMMAND [ARGS]

  init                         Create a new store (PATH must not exist)
  import SOURCE NAME           Copy a directory into a new workspace
  fork WORKSPACE NEW_NAME      Create an independent workspace
  checkpoint WORKSPACE NAME    Create a new tool-immutable snapshot
  restore SNAPSHOT NEW_NAME    Restore into a NEW workspace; never overwrite
  drop workspace|snapshot NAME Move an entry to recoverable trash
  recover TRASH_TOKEN NEW_NAME Copy trash into a NEW workspace; retain trash
  list                         List workspaces, snapshots and trash
  status                       Show backend policy, entries and staging count
  show workspace|snapshot|trash NAME
                               Show tree path and creation backend counts

Options must precede COMMAND. --copy disables FICLONE. No permanent purge.
Stop external writers before import, fork, checkpoint, restore or recover.
Snapshots are immutable through Sprig, not protected from filesystem writes.
";

pub(crate) fn run() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(first) = args.next() else {
        print!("{HELP}");
        return Ok(());
    };
    if first == "--help" || first == "-h" {
        print!("{HELP}");
        return Ok(());
    }
    if first == "--version" {
        println!("sprig {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if first != "--store" {
        return Err(usage("expected --store PATH"));
    }
    let root = PathBuf::from(args.next().ok_or_else(|| usage("missing store path"))?);
    let mut command = args.next().ok_or_else(|| usage("missing command"))?;
    let plain = command == "--copy";
    if plain {
        command = args.next().ok_or_else(|| usage("missing command"))?;
    }
    let command = text(&command)?;
    let operands: Vec<OsString> = args.collect();
    if command == "init" {
        arity(&operands, 0)?;
        Store::init(&root)?;
        println!("initialized {}", root.display());
        return Ok(());
    }
    let store = Store::open(&root)?;
    match command {
        "import" => {
            arity(&operands, 2)?;
            report(store.import(&PathBuf::from(&operands[0]), text(&operands[1])?, plain)?);
        }
        "fork" | "checkpoint" | "restore" | "recover" => {
            arity(&operands, 2)?;
            let from_area = match command {
                "restore" => "snapshots",
                "recover" => "trash",
                _ => "workspaces",
            };
            let to_area = if command == "checkpoint" {
                "snapshots"
            } else {
                "workspaces"
            };
            report(store.duplicate(
                from_area,
                text(&operands[0])?,
                to_area,
                text(&operands[1])?,
                plain,
            )?);
        }
        "drop" | "show" => {
            arity(&operands, 2)?;
            let area = match text(&operands[0])? {
                "workspace" => "workspaces",
                "snapshot" => "snapshots",
                "trash" if command == "show" => "trash",
                _ => return Err(usage("expected workspace or snapshot (or trash for show)")),
            };
            let name = text(&operands[1])?;
            if command == "drop" {
                println!("trash token: {}", store.drop_entry(area, name)?);
            } else {
                store.show(area, name)?;
            }
        }
        "list" | "status" => {
            arity(&operands, 0)?;
            if command == "list" {
                store.list()?;
            } else {
                store.status()?;
            }
        }
        _ => return Err(usage("unknown command")),
    }
    Ok(())
}

fn text(value: &OsString) -> io::Result<&str> {
    value
        .to_str()
        .ok_or_else(|| usage("command and names must be UTF-8"))
}

fn arity(args: &[OsString], expected: usize) -> io::Result<()> {
    if args.len() != expected {
        return Err(usage(&format!(
            "expected {expected} arguments, got {}",
            args.len()
        )));
    }
    Ok(())
}

fn usage(message: &str) -> io::Error {
    io::Error::other(format!("{message}; see sprig --help"))
}

fn report(stats: CopyStats) {
    println!(
        "published: reflink={} copy={} symlink={}",
        stats.cloned, stats.copied, stats.symlinks
    );
}
