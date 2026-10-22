//! Command-line grammar and presentation. Storage semantics live in the library.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use sprig::{CopyMode, MergeStrategy, Mutation, Store};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Native filesystem workspaces with content-addressed revision history"
)]
#[command(
    after_help = "Workspaces are real directories, not sandboxes. Stop external writers before capturing a revision. Use COMMAND --help for details."
)]
struct Cli {
    /// Store directory. Use a new path for `init`.
    #[arg(long, value_name = "PATH")]
    store: PathBuf,

    /// Emit a versioned JSON result on stdout, including errors.
    #[arg(long, global = true)]
    json: bool,

    /// Copy policy for import, fork and workspace recovery; clone never falls back.
    #[arg(long, global = true, value_enum, default_value_t = CopyOption::Auto)]
    copy_mode: CopyOption,

    /// Shorthand for --copy-mode copy.
    #[arg(long, global = true, conflicts_with = "copy_mode")]
    copy: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CopyOption {
    Auto,
    Copy,
    Clone,
}

impl From<CopyOption> for CopyMode {
    fn from(value: CopyOption) -> Self {
        match value {
            CopyOption::Auto => Self::Auto,
            CopyOption::Copy => Self::Copy,
            CopyOption::Clone => Self::Clone,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum StrategyOption {
    Conflict,
    Lww,
}

impl From<StrategyOption> for MergeStrategy {
    fn from(value: StrategyOption) -> Self {
        match value {
            StrategyOption::Conflict => Self::Conflict,
            StrategyOption::Lww => Self::Lww,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EntryKind {
    Workspace,
    Branch,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Branch => "branch",
        }
    }
}

#[derive(Debug, Args)]
struct CommitOptions {
    /// Description recorded in the new revision.
    #[arg(short, long)]
    message: Option<String>,

    /// Require this exact branch head before publishing (optimistic concurrency).
    #[arg(long, value_name = "REVISION")]
    expected_head: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize a store at a new directory; existing paths are not replaced.
    Init,
    /// Import a source directory into a new workspace and its initial history.
    Import { source: PathBuf, name: String },
    /// Fork a workspace into an independent native working tree.
    Fork { workspace: String, new_name: String },
    /// Materialize a branch head into a new workspace attached to that branch.
    Checkout { branch: String, name: String },
    /// Materialize a revision into a new workspace, never overwrite an old one.
    Restore { revision: String, name: String },
    /// Capture a workspace as a revision, advancing its branch with a head check.
    #[command(visible_alias = "commit")]
    Checkpoint {
        workspace: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Acknowledge manually resolved conflict paths after checking for markers.
    Resolve {
        workspace: String,
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
    },
    /// Create a new branch pointing to an existing revision or reference.
    Branch { name: String, revision: String },
    /// Merge into a new branch; neither input moves and conflicts publish nothing.
    Merge {
        ours: String,
        theirs: String,
        output: String,
        #[arg(long, value_enum, default_value_t = StrategyOption::Conflict)]
        strategy: StrategyOption,
        /// Treat these comma-separated paths as one merge unit; may be repeated.
        #[arg(long = "unit", value_name = "PATH,PATH")]
        units: Vec<String>,
        #[arg(long, value_name = "REVISION")]
        expected_head: Option<String>,
    },
    /// Append a new revision restoring older content; never rewrite history.
    Rollback {
        branch: String,
        revision: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Show revisions reachable from a reference, or all branch heads when omitted.
    Log { reference: Option<String> },
    /// Compare two revisions or references.
    Diff { from: String, to: String },
    /// Include unified text patches where possible; identify binary/large changes.
    DiffFiles { from: String, to: String },
    /// Inspect the recorded provenance of a path in a revision or reference.
    Blame { reference: String, path: String },
    /// Read exact file bytes to stdout; --json returns an array of byte values.
    Read { reference: String, path: String },
    /// List immediate children of a revision directory (root when PATH is omitted).
    Ls {
        reference: String,
        path: Option<String>,
    },
    /// Inspect an entry's recorded type, identity, content and metadata.
    Stat { reference: String, path: String },
    /// Search text files in one resolved revision.
    Grep { reference: String, pattern: String },
    /// Match paths in one resolved revision using a glob pattern.
    Glob { reference: String, pattern: String },
    /// List branches, workspaces and recoverable trash.
    List,
    /// Show workspace references, stale bases and active managed leases.
    Status,
    /// Show a workspace's native path and metadata.
    Show { workspace: String },
    /// Run a program in a workspace under a managed-mutation lease; no sandbox.
    #[command(trailing_var_arg = true)]
    Exec {
        workspace: String,
        program: String,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Move a workspace or branch to recoverable trash; not permanent deletion.
    Drop {
        #[arg(value_enum)]
        kind: EntryKind,
        name: String,
    },
    /// Recover a trash entry into a new workspace while retaining the trash.
    Recover { token: String, name: String },
    /// Validate store metadata and content; report problems without deleting data.
    Doctor,
    /// Restore a validated state backup, preserving current metadata for inspection.
    RecoverState { generation: u64 },
    /// Show a garbage-collection plan only. No objects are removed.
    Gc {
        /// Explicitly request the only supported mode: a non-destructive plan.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show logical and stored content statistics.
    Stats,
    /// Describe native-copy backend capabilities and limitations.
    Backend,
    /// Write bytes to a workspace path and commit the managed transaction.
    Write {
        workspace: String,
        path: String,
        /// Read the complete file as bytes, including non-UTF-8 content.
        #[arg(
            long,
            value_name = "INPUT",
            required_unless_present = "text",
            conflicts_with = "text"
        )]
        file: Option<PathBuf>,
        /// Write this UTF-8 text exactly, without appending a newline.
        #[arg(long, required_unless_present = "file")]
        text: Option<String>,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Replace exactly one textual match and commit the managed transaction.
    Edit {
        workspace: String,
        path: String,
        old: String,
        new: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Copy a workspace entry to a new exact path and commit the transaction.
    Cp {
        workspace: String,
        from: String,
        to: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Create a workspace directory and commit the managed transaction.
    Mkdir {
        workspace: String,
        path: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Remove a workspace path and commit the managed transaction.
    Rm {
        workspace: String,
        path: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Move a workspace path and commit the managed transaction.
    Mv {
        workspace: String,
        from: String,
        to: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
    /// Set ordinary Unix permission bits and commit the managed transaction.
    Chmod {
        workspace: String,
        #[arg(value_parser = parse_mode, value_name = "OCTAL_MODE")]
        mode: u32,
        path: String,
        #[command(flatten)]
        commit: CommitOptions,
    },
}

struct CliError {
    code: &'static str,
    message: String,
}

impl CliError {
    fn arguments(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_arguments",
            message: message.into(),
        }
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for CliError {
    fn from(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self {
            code: "operation_failed",
            message: error.to_string(),
        }
    }
}

enum Output {
    Result { value: Value, exit_code: i32 },
    Exit(i32),
}

fn result(value: impl Serialize) -> Result<Output, CliError> {
    let value = serde_json::to_value(value).map_err(|error| CliError {
        code: "serialization_failed",
        message: error.to_string(),
    })?;
    Ok(Output::Result {
        value,
        exit_code: 0,
    })
}

pub(crate) fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    let requested_json = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) if !error.use_stderr() => {
            return if error.print().is_ok() { 0 } else { 1 };
        }
        Err(error) => {
            report_error(requested_json, &CliError::arguments(error.to_string()));
            return 2;
        }
    };
    match execute(&cli) {
        Ok(Output::Exit(code)) => code,
        Ok(Output::Result { value, exit_code }) => {
            let value = if cli.json {
                json!({"schema_version": 1, "result": value})
            } else {
                value
            };
            if let Err(error) = write_json(io::stdout().lock(), &value) {
                eprintln!("sprig: cannot write output: {error}");
                return 1;
            }
            exit_code
        }
        Err(error) => {
            report_error(cli.json, &error);
            if error.code == "invalid_arguments" {
                2
            } else {
                1
            }
        }
    }
}

fn execute(cli: &Cli) -> Result<Output, CliError> {
    if matches!(cli.command, Command::Exec { .. }) && cli.json {
        return Err(CliError::arguments(
            "--json is not supported with exec because the child inherits standard output",
        ));
    }
    if matches!(cli.command, Command::Init) {
        Store::init(&cli.store)?;
        return result(json!({"initialized": true, "store": cli.store}));
    }
    if let Command::RecoverState { generation } = &cli.command {
        return result(Store::recover_state(&cli.store, *generation)?);
    }
    let store = Store::open(&cli.store)?;
    let mode = if cli.copy {
        CopyMode::Copy
    } else {
        cli.copy_mode.into()
    };
    match &cli.command {
        Command::Init | Command::RecoverState { .. } => {
            unreachable!("initialization and state recovery are handled before opening the store")
        }
        Command::Import { source, name } => result(store.import(source, name, mode)?),
        Command::Fork {
            workspace,
            new_name,
        } => result(store.fork(workspace, new_name, mode)?),
        Command::Checkout { branch, name } => result(store.checkout(branch, name)?),
        Command::Restore { revision, name } => result(store.restore(revision, name)?),
        Command::Checkpoint { workspace, commit } => result(store.checkpoint(
            workspace,
            commit.message.as_deref().unwrap_or("checkpoint"),
            commit.expected_head.as_deref(),
        )?),
        Command::Resolve { workspace, paths } => result(store.resolve_conflicts(workspace, paths)?),
        Command::Branch { name, revision } => result(store.branch(name, revision)?),
        Command::Merge {
            ours,
            theirs,
            output,
            strategy,
            units,
            expected_head,
        } => {
            let units = parse_units(units)?;
            let outcome = store.merge(
                ours,
                theirs,
                output,
                (*strategy).into(),
                &units,
                expected_head.as_deref(),
            )?;
            let exit_code = if outcome.committed { 0 } else { 3 };
            let mut output = result(outcome)?;
            if let Output::Result {
                exit_code: code, ..
            } = &mut output
            {
                *code = exit_code;
            }
            Ok(output)
        }
        Command::Rollback {
            branch,
            revision,
            commit,
        } => result(store.rollback(
            branch,
            revision,
            commit.message.as_deref().unwrap_or("rollback"),
            commit.expected_head.as_deref(),
        )?),
        Command::Log { reference } => result(store.log(reference.as_deref())?),
        Command::Diff { from, to } => result(store.diff(from, to)?),
        Command::DiffFiles { from, to } => result(store.diff_files(from, to)?),
        Command::Blame { reference, path } => result(store.blame(reference, path)?),
        Command::Read { reference, path } => {
            if cli.json {
                result(store.read(reference, path)?)
            } else {
                store.read_to(reference, path, &mut io::stdout().lock())?;
                Ok(Output::Exit(0))
            }
        }
        Command::Ls { reference, path } => {
            result(store.ls(reference, path.as_deref().unwrap_or(""))?)
        }
        Command::Stat { reference, path } => result(store.stat(reference, path)?),
        Command::Grep { reference, pattern } => result(store.grep(reference, pattern)?),
        Command::Glob { reference, pattern } => result(store.glob(reference, pattern)?),
        Command::List => result(store.list()?),
        Command::Status => result(store.status()?),
        Command::Show { workspace } => result(store.show(workspace)?),
        Command::Exec {
            workspace,
            program,
            args,
        } => Ok(Output::Exit(store.exec(workspace, program, args)?)),
        Command::Drop { kind, name } => result(store.drop_entry(kind.as_str(), name)?),
        Command::Recover { token, name } => result(store.recover(token, name, mode)?),
        Command::Doctor => {
            let report = store.doctor()?;
            let exit_code = if report.healthy { 0 } else { 1 };
            let mut output = result(report)?;
            if let Output::Result {
                exit_code: code, ..
            } = &mut output
            {
                *code = exit_code;
            }
            Ok(output)
        }
        Command::Gc { .. } => result(store.gc()?),
        Command::Stats => result(store.stats()?),
        Command::Backend => result(store.backend()?),
        Command::Write {
            workspace,
            path,
            file,
            text,
            commit,
        } => {
            let content = match (file, text) {
                (Some(path), None) => fs::read(path).map_err(|error| CliError {
                    code: "input_read_failed",
                    message: format!("cannot read {}: {error}", path.display()),
                })?,
                (None, Some(text)) => text.as_bytes().to_vec(),
                _ => {
                    return Err(CliError::arguments(
                        "provide exactly one of --file or --text",
                    ));
                }
            };
            apply(
                &store,
                workspace,
                Mutation::Write {
                    path: path.clone(),
                    content,
                },
                commit,
                "write",
            )
        }
        Command::Edit {
            workspace,
            path,
            old,
            new,
            commit,
        } => apply(
            &store,
            workspace,
            Mutation::Edit {
                path: path.clone(),
                old: old.clone(),
                new: new.clone(),
            },
            commit,
            "edit",
        ),
        Command::Cp {
            workspace,
            from,
            to,
            commit,
        } => apply(
            &store,
            workspace,
            Mutation::Copy {
                from: from.clone(),
                to: to.clone(),
            },
            commit,
            "copy",
        ),
        Command::Mkdir {
            workspace,
            path,
            commit,
        } => apply(
            &store,
            workspace,
            Mutation::Mkdir { path: path.clone() },
            commit,
            "mkdir",
        ),
        Command::Rm {
            workspace,
            path,
            commit,
        } => apply(
            &store,
            workspace,
            Mutation::Remove { path: path.clone() },
            commit,
            "remove",
        ),
        Command::Mv {
            workspace,
            from,
            to,
            commit,
        } => apply(
            &store,
            workspace,
            Mutation::Move {
                from: from.clone(),
                to: to.clone(),
            },
            commit,
            "move",
        ),
        Command::Chmod {
            workspace,
            mode,
            path,
            commit,
        } => apply(
            &store,
            workspace,
            Mutation::Chmod {
                path: path.clone(),
                mode: *mode,
            },
            commit,
            "chmod",
        ),
    }
}

fn apply(
    store: &Store,
    workspace: &str,
    mutation: Mutation,
    commit: &CommitOptions,
    default_message: &str,
) -> Result<Output, CliError> {
    result(store.apply(
        workspace,
        &[mutation],
        commit.message.as_deref().unwrap_or(default_message),
        commit.expected_head.as_deref(),
    )?)
}

fn parse_mode(value: &str) -> Result<u32, String> {
    let digits = value.strip_prefix("0o").unwrap_or(value);
    let mode = u32::from_str_radix(digits, 8)
        .map_err(|_| "mode must be octal, for example 644 or 0755".to_owned())?;
    if mode > 0o777 {
        return Err("only ordinary permission bits (000 through 777) are supported".to_owned());
    }
    Ok(mode)
}

fn parse_units(values: &[String]) -> Result<Vec<Vec<String>>, CliError> {
    values
        .iter()
        .map(|value| {
            let paths: Vec<String> = value.split(',').map(str::to_owned).collect();
            if paths.iter().any(String::is_empty) {
                return Err(CliError::arguments(
                    "merge units must contain non-empty comma-separated paths",
                ));
            }
            Ok(paths)
        })
        .collect()
}

fn write_json(mut writer: impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut writer, value)?;
    writeln!(writer)
}

fn report_error(machine: bool, error: &CliError) {
    if machine {
        let value = json!({
            "schema_version": 1,
            "error": {"code": error.code, "message": error.message}
        });
        if let Err(error) = write_json(io::stdout().lock(), &value) {
            eprintln!("sprig: cannot write error output: {error}");
        }
    } else {
        eprintln!("sprig: {}", error.message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_grammar() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cases: &[(&[&str], bool)] = &[
            (
                &[
                    "commit",
                    "main",
                    "-m",
                    "note",
                    "--expected-head",
                    "revision",
                ],
                true,
            ),
            (&["write", "main", "file"], false),
            (
                &["write", "main", "file", "--file", "input", "--text", "text"],
                false,
            ),
            (&["write", "main", "file", "--text", ""], true),
            (&["--copy", "fork", "main", "other"], true),
            (
                &["--copy", "--copy-mode", "clone", "fork", "main", "other"],
                false,
            ),
            (&["chmod", "main", "0755", "script"], true),
            (&["chmod", "main", "4755", "script"], false),
            (&["diff-files", "main", "topic"], true),
            (&["merge", "main", "topic", "combined"], true),
            (&["merge", "main", "topic"], false),
            (&["ls", "main"], true),
            (&["resolve", "main"], false),
            (&["resolve", "main", "file", "directory/file"], true),
            (&["read", "main", "file"], true),
            (&["edit", "main", "file", "before", "after"], true),
            (&["cp", "main", "source", "target"], true),
            (&["recover-state", "12"], true),
            (&["recover-state", "-1"], false),
        ];
        for (args, expected) in cases {
            let parsed = Cli::try_parse_from(
                ["sprig", "--store", "store"]
                    .into_iter()
                    .chain(args.iter().copied()),
            );
            assert_eq!(parsed.is_ok(), *expected, "arguments: {args:?}");
        }
        let cli = Cli::try_parse_from([
            "sprig",
            "--store",
            "store",
            "exec",
            "main",
            "--",
            "sh",
            "-c",
            "printf test",
        ])
        .expect("valid child arguments");
        let Command::Exec { program, args, .. } = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(program, "sh");
        assert_eq!(args, ["-c", "printf test"]);
        assert_eq!(parse_mode("0o644").unwrap(), 0o644);
        assert!(parse_mode("888").is_err());
        assert!(parse_units(&["file,".to_owned()]).is_err());
        assert_eq!(
            parse_units(&["a,b".to_owned()]).unwrap_or_default(),
            vec![vec!["a".to_owned(), "b".to_owned()]]
        );
    }
}
