// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Weave command line (specification sections 151-157).
//!
//! Every command an agent drives supports `--json`: stable field names,
//! machine-readable result on stdout, diagnostics on stderr, meaningful exit
//! codes, and no interactive prompt.

use crate::error::{usage, Result};
use crate::ipc::{self, IpcCommand, ResolveSource};
use crate::session::Paths;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "weave",
    version,
    about = "Weave — a lightweight real-time collaboration layer above Git",
    long_about = "Weave keeps several local copies of one Git repository in sync in real time \
                  while Git keeps its ordinary role as the durable, publishable history.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Run against a different repository directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub repo: Option<PathBuf>,

    /// Verbose diagnostic logging on stderr.
    #[arg(long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Host a Weave session for this repository.
    Host(HostArgs),
    /// Join a Weave session using an invite.
    Join(JoinArgs),
    /// Resume this repository's Weave session after a crash or restart.
    Resume,
    /// Leave the session and forget its local session record.
    Leave(JsonArgs),
    /// Stop the running Weave daemon, keeping the session record.
    Stop(JsonArgs),
    /// Show the state of the live session.
    Status(JsonArgs),
    /// List session participants.
    Peers(JsonArgs),
    /// Print the invite for this session (host only).
    Invite(JsonArgs),
    /// Force a full repository rescan.
    Rescan(JsonArgs),
    /// Describe intent with Tasks and advisory soft locks.
    #[command(subcommand)]
    Task(TaskCommand),
    /// Inspect and resolve Weave conflicts.
    #[command(subcommand)]
    Conflict(ConflictCommand),
    /// Prepare and create Git publications.
    #[command(subcommand)]
    Commit(CommitCommand),
    /// Ask the host to push the published branch to its remote.
    Push(JsonArgs),
    /// Manage the Cloudflare Quick Tunnel.
    #[command(subcommand)]
    Tunnel(TunnelCommand),
    /// Agent integration helpers.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Check that this machine and repository can run Weave.
    Doctor(JsonArgs),
    /// Diagnose and repair Weave storage without discarding data.
    Recover(RecoverArgs),
    /// Read and write local Weave configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Args, Debug)]
pub struct JsonArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct HostArgs {
    /// Serve on the local network instead of a Cloudflare Quick Tunnel.
    #[arg(long)]
    pub lan: bool,
    /// Do not expose any remote endpoint; only this machine participates.
    #[arg(long)]
    pub local: bool,
}

#[derive(Args, Debug)]
pub struct JoinArgs {
    /// Read the invite from a file instead of prompting.
    #[arg(long, value_name = "PATH")]
    pub invite_file: Option<PathBuf>,
    /// Read the invite from standard input instead of prompting.
    #[arg(long)]
    pub invite_stdin: bool,
}

#[derive(Subcommand, Debug)]
pub enum TaskCommand {
    /// Start one Task describing what you are about to change.
    Start {
        #[arg(long, value_name = "TEXT")]
        description: String,
        /// Declare a file scope, optionally with a line range: `path` or `path:50-110`.
        #[arg(long = "file", value_name = "SCOPE")]
        files: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// List Tasks in this session.
    List(JsonArgs),
    /// Show one Task and its overlaps.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Update a Task description or scopes.
    Update {
        id: String,
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
        #[arg(long = "file", value_name = "SCOPE")]
        files: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Complete a Task.
    Complete {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a Task.
    Cancel {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConflictCommand {
    /// List Weave conflicts.
    List(JsonArgs),
    /// Show one conflict, writing its candidates to .git/weave/conflicts.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Resolve a conflict with one atomic request.
    Resolve {
        id: String,
        /// Where the resolved content comes from.
        #[arg(long = "use", value_enum, default_value_t = ResolveChoice::Working)]
        source: ResolveChoice,
        /// Resolve using the contents of this file.
        #[arg(long, value_name = "PATH", conflicts_with = "source")]
        content_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Dismiss a conflict without changing canonical state.
    Dismiss {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ResolveChoice {
    /// The file as it currently is in your working tree (default).
    Working,
    /// Keep the canonical host content.
    Canonical,
    /// The latest preserved local candidate.
    Local,
    /// The rejected incoming candidate.
    Incoming,
    /// Resolve by deleting the path.
    Delete,
}

#[derive(Subcommand, Debug)]
pub enum CommitCommand {
    /// Bind a Git publication to one immutable live revision.
    Prepare {
        /// Publish even though an active Task contributed revisions.
        #[arg(long)]
        allow_active_tasks: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create the prepared Git publication on the host.
    Create {
        prepare_id: String,
        #[arg(long, short = 'm', value_name = "TEXT")]
        message: Option<String>,
        /// Read the commit message from standard input.
        #[arg(long)]
        message_stdin: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TunnelCommand {
    /// Replace the Quick Tunnel, keeping the same Weave session.
    Restart(JsonArgs),
}

#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Write the managed Weave block into the repository's AGENTS.md.
    Bootstrap(JsonArgs),
}

#[derive(Args, Debug)]
pub struct RecoverArgs {
    /// Rebuild the derived canonical manifest from durable revision history.
    #[arg(long)]
    pub rebuild: bool,
    /// Export the latest recoverable canonical files to this directory.
    #[arg(long, value_name = "DIR")]
    pub export: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Show all local Weave configuration.
    List(JsonArgs),
    /// Read one configuration value.
    Get {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Set one configuration value.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub fn run(cli: Cli) -> Result<()> {
    let start_dir = match &cli.repo {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };

    match cli.command {
        Command::Host(args) => crate::daemon::run_host(
            &start_dir,
            crate::daemon::HostOptions {
                lan: args.lan,
                local_only: args.local,
            },
        ),
        Command::Join(args) => {
            let invite = read_invite(&args)?;
            crate::daemon::run_join(&start_dir, crate::daemon::JoinOptions { invite })
        }
        Command::Resume => crate::daemon::run_resume(&start_dir),
        Command::Stop(args) => {
            let paths = Paths::discover(&start_dir)?;
            let value = ipc::call(&paths, IpcCommand::Stop)?;
            emit(args.json, value, |_| println!("Weave daemon stopping."));
            Ok(())
        }
        Command::Leave(args) => {
            let paths = Paths::discover(&start_dir)?;
            let value = ipc::call(&paths, IpcCommand::Leave)?;
            emit(args.json, value, |_| {
                println!("Left the Weave session. The working tree is unchanged.")
            });
            Ok(())
        }
        Command::Status(args) => status(&start_dir, args.json),
        Command::Peers(args) => {
            let paths = Paths::discover(&start_dir)?;
            let value = ipc::call(&paths, IpcCommand::Peers)?;
            emit(args.json, value, render::peers);
            Ok(())
        }
        Command::Invite(args) => {
            let paths = Paths::discover(&start_dir)?;
            let value = ipc::call(&paths, IpcCommand::Invite)?;
            emit(args.json, value, render::invite);
            Ok(())
        }
        Command::Rescan(args) => {
            let paths = Paths::discover(&start_dir)?;
            let value = ipc::call(&paths, IpcCommand::Rescan)?;
            emit(args.json, value, |_| println!("Repository rescanned."));
            Ok(())
        }
        Command::Task(command) => task(&start_dir, command),
        Command::Conflict(command) => conflict(&start_dir, command),
        Command::Commit(command) => commit(&start_dir, command),
        Command::Push(args) => {
            let paths = Paths::discover(&start_dir)?;
            let value = ipc::call(&paths, IpcCommand::Push)?;
            emit(args.json, value, render::push);
            Ok(())
        }
        Command::Tunnel(TunnelCommand::Restart(args)) => {
            let paths = Paths::discover(&start_dir)?;
            // Bringing up a replacement Quick Tunnel can take longer than the
            // default control timeout, and giving up on the reply while the
            // daemon succeeds would report a failure that did not happen.
            let value = ipc::call_with_timeout(
                &paths,
                IpcCommand::TunnelRestart,
                std::time::Duration::from_secs(180),
            )?;
            emit(args.json, value, render::invite);
            Ok(())
        }
        Command::Agent(AgentCommand::Bootstrap(args)) => {
            let paths = Paths::discover(&start_dir)?;
            let result = crate::bootstrap::apply(&paths.repo_root)?;
            let value = serde_json::json!({
                "path": result.path,
                "created": result.created,
                "updated": result.updated,
            });
            emit(args.json, value, |v| {
                let path = v["path"].as_str().unwrap_or("AGENTS.md");
                if v["created"].as_bool() == Some(true) {
                    println!("Created {path} with the Weave collaboration block.");
                } else if v["updated"].as_bool() == Some(true) {
                    println!("Updated the Weave block in {path}.");
                } else {
                    println!("The Weave block in {path} is already up to date.");
                }
            });
            Ok(())
        }
        Command::Doctor(args) => {
            let report = crate::doctor::run(&start_dir);
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                crate::doctor::print_report(&report);
            }
            if report.ready {
                Ok(())
            } else {
                Err(
                    crate::error::repository("Weave is not ready in this repository.")
                        .with_detail("Run `weave doctor` for the full checklist."),
                )
            }
        }
        Command::Recover(args) => {
            let report = crate::recover::run(
                &start_dir,
                crate::recover::RecoverOptions {
                    rebuild: args.rebuild,
                    export: args.export,
                },
            )?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                crate::recover::print_report(&report);
            }
            Ok(())
        }
        Command::Config(command) => config(command),
    }
}

fn status(start_dir: &std::path::Path, json: bool) -> Result<()> {
    let paths = Paths::discover(start_dir)?;
    if !ipc::daemon_is_running(&paths) {
        let value = serde_json::json!({
            "active": false,
            "repository": paths.repo_name(),
            "branch": crate::gitx::current_branch(&paths.repo_root)?,
        });
        emit(json, value, |_| {
            println!("No Weave session is active in this repository.");
            println!();
            println!("Start one with `weave host`, or join one with `weave join`.");
        });
        return Ok(());
    }
    let value = ipc::call(&paths, IpcCommand::Status)?;
    emit(json, value, render::status);
    Ok(())
}

fn task(start_dir: &std::path::Path, command: TaskCommand) -> Result<()> {
    let paths = Paths::discover(start_dir)?;
    match command {
        TaskCommand::Start {
            description,
            files,
            json,
        } => {
            let value = ipc::call(
                &paths,
                IpcCommand::TaskStart {
                    description,
                    scopes: files,
                },
            )?;
            let _ = value;
            let listed = ipc::call(&paths, IpcCommand::TaskList)?;
            emit(json, listed, render::task_started);
            Ok(())
        }
        TaskCommand::List(args) => {
            let value = ipc::call(&paths, IpcCommand::TaskList)?;
            emit(args.json, value, render::task_list);
            Ok(())
        }
        TaskCommand::Show { id, json } => {
            let value = ipc::call(&paths, IpcCommand::TaskShow { id })?;
            emit(json, value, render::task_show);
            Ok(())
        }
        TaskCommand::Update {
            id,
            description,
            files,
            json,
        } => {
            let scopes = if files.is_empty() { None } else { Some(files) };
            ipc::call(
                &paths,
                IpcCommand::TaskUpdate {
                    id: id.clone(),
                    description,
                    scopes,
                },
            )?;
            let value = ipc::call(&paths, IpcCommand::TaskShow { id })?;
            emit(json, value, render::task_show);
            Ok(())
        }
        TaskCommand::Complete { id, json } => {
            ipc::call(&paths, IpcCommand::TaskComplete { id: id.clone() })?;
            let value = ipc::call(&paths, IpcCommand::TaskShow { id })?;
            emit(json, value, |v| {
                println!(
                    "Task completed: {}",
                    v["task"]["description"].as_str().unwrap_or("")
                );
            });
            Ok(())
        }
        TaskCommand::Cancel { id, json } => {
            ipc::call(&paths, IpcCommand::TaskCancel { id: id.clone() })?;
            let value = ipc::call(&paths, IpcCommand::TaskShow { id })?;
            emit(json, value, |v| {
                println!(
                    "Task cancelled: {}",
                    v["task"]["description"].as_str().unwrap_or("")
                );
            });
            Ok(())
        }
    }
}

fn conflict(start_dir: &std::path::Path, command: ConflictCommand) -> Result<()> {
    let paths = Paths::discover(start_dir)?;
    match command {
        ConflictCommand::List(args) => {
            let value = ipc::call(&paths, IpcCommand::ConflictList)?;
            emit(args.json, value, render::conflict_list);
            Ok(())
        }
        ConflictCommand::Show { id, json } => {
            let value = ipc::call(&paths, IpcCommand::ConflictShow { id })?;
            emit(json, value, render::conflict_show);
            Ok(())
        }
        ConflictCommand::Resolve {
            id,
            source,
            content_file,
            json,
        } => {
            let (source, content_b64) = match content_file {
                Some(path) => {
                    let bytes = std::fs::read(&path)?;
                    (
                        ResolveSource::Supplied,
                        Some(crate::util::b64_encode(&bytes)),
                    )
                }
                None => (
                    match source {
                        ResolveChoice::Working => ResolveSource::WorkingTree,
                        ResolveChoice::Canonical => ResolveSource::Canonical,
                        ResolveChoice::Local => ResolveSource::LocalCandidate,
                        ResolveChoice::Incoming => ResolveSource::Incoming,
                        ResolveChoice::Delete => ResolveSource::Delete,
                    },
                    None,
                ),
            };
            let value = ipc::call(
                &paths,
                IpcCommand::ConflictResolve {
                    id,
                    source,
                    content_b64,
                },
            )?;
            emit(json, value, |v| {
                println!(
                    "Conflict resolved. {}",
                    v["note"].as_str().unwrap_or("Canonical state updated.")
                );
            });
            Ok(())
        }
        ConflictCommand::Dismiss { id, json } => {
            let value = ipc::call(&paths, IpcCommand::ConflictDismiss { id })?;
            emit(json, value, |_| println!("Conflict dismissed."));
            Ok(())
        }
    }
}

fn commit(start_dir: &std::path::Path, command: CommitCommand) -> Result<()> {
    let paths = Paths::discover(start_dir)?;
    match command {
        CommitCommand::Prepare {
            allow_active_tasks,
            json,
        } => {
            let value = ipc::call(&paths, IpcCommand::CommitPrepare { allow_active_tasks })?;
            emit(json, value, render::prepare);
            Ok(())
        }
        CommitCommand::Create {
            prepare_id,
            message,
            message_stdin,
            json,
        } => {
            let message = match (message, message_stdin) {
                (Some(message), _) => message,
                (None, true) => {
                    let mut buffer = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)?;
                    buffer
                }
                (None, false) => {
                    return Err(usage("A commit message is required.").with_detail(
                        "Pass --message \"docs: refine market narrative\", or --message-stdin.",
                    ))
                }
            };
            let value = ipc::call(
                &paths,
                IpcCommand::CommitCreate {
                    prepare_id,
                    message,
                },
            )?;
            emit(json, value, render::publication);
            Ok(())
        }
    }
}

fn config(command: ConfigCommand) -> Result<()> {
    let mut identity = crate::session::load_or_create_identity()?;
    match command {
        ConfigCommand::List(args) => {
            let value = serde_json::json!({
                "actor_id": identity.actor_id,
                "display_name": identity.display_name,
                "effective_display_name": identity
                    .display_name
                    .clone()
                    .unwrap_or_else(crate::session::os_username),
            });
            emit(args.json, value, |v| {
                println!("actor_id       {}", v["actor_id"].as_str().unwrap_or(""));
                println!(
                    "display_name   {}",
                    v["display_name"]
                        .as_str()
                        .unwrap_or("(from git config user.name)")
                );
            });
            Ok(())
        }
        ConfigCommand::Get { key, json } => {
            let value = match key.as_str() {
                "actor_id" | "actor-id" => {
                    serde_json::json!({ "key": "actor_id", "value": identity.actor_id })
                }
                "display_name" | "display-name" => {
                    serde_json::json!({ "key": "display_name", "value": identity.display_name })
                }
                other => return Err(unknown_key(other)),
            };
            emit(json, value, |v| {
                println!("{}", v["value"].as_str().unwrap_or(""));
            });
            Ok(())
        }
        ConfigCommand::Set { key, value, json } => {
            match key.as_str() {
                "display_name" | "display-name" => {
                    identity.display_name = if value.trim().is_empty() {
                        None
                    } else {
                        Some(value.trim().to_string())
                    };
                    crate::session::save_identity(&identity)?;
                }
                other => return Err(unknown_key(other)),
            }
            let value = serde_json::json!({ "key": key, "value": identity.display_name });
            emit(json, value, |_| {
                println!("Saved. Restart the Weave session for the new name to take effect.")
            });
            Ok(())
        }
    }
}

fn unknown_key(key: &str) -> crate::error::WeaveError {
    usage(format!("Unknown Weave configuration key `{key}`."))
        .with_detail("Known keys: display_name (read-only: actor_id).")
}

fn read_invite(args: &JoinArgs) -> Result<String> {
    if let Some(path) = &args.invite_file {
        let text = std::fs::read_to_string(path)?;
        return Ok(text.trim().to_string());
    }
    if args.invite_stdin {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)?;
        return Ok(buffer.trim().to_string());
    }
    // The invite carries the session secret, so it is never taken from an
    // ordinary command-line argument (specification section 57).
    let entered = rpassword::prompt_password("Paste Weave invite:\n> ")
        .map_err(|e| usage(format!("Could not read the invite: {e}")))?;
    let entered = entered.trim().to_string();
    if entered.is_empty() {
        return Err(usage("No invite was entered.").with_detail(
            "Ask the host to run `weave invite`, or use --invite-file / --invite-stdin.",
        ));
    }
    Ok(entered)
}

fn emit(json: bool, value: serde_json::Value, human: impl FnOnce(&serde_json::Value)) {
    if json {
        match serde_json::to_string_pretty(&value) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("could not serialize output: {e}"),
        }
    } else {
        human(&value);
    }
}

// ---------------------------------------------------------------------------
// Human-readable rendering
// ---------------------------------------------------------------------------

mod render {
    use serde_json::Value;

    fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
        v.get(key).and_then(|x| x.as_str()).unwrap_or("")
    }

    fn u64_of(v: &Value, key: &str) -> u64 {
        v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
    }

    fn plural(count: u64, singular: &str) -> String {
        if count == 1 {
            format!("{count} {singular}")
        } else {
            format!("{count} {singular}s")
        }
    }

    pub fn status(v: &Value) {
        println!("Weave — {}", str_of(v, "repository"));
        println!();
        println!("Role: {}", str_of(v, "role"));
        println!("Host: {}", str_of(v, "host"));
        println!();
        println!("Branch: {}", str_of(v, "branch"));
        println!();
        match v.get("git_publication") {
            Some(Value::Object(publication)) => {
                println!("Git publication:");
                println!(
                    "{}",
                    publication
                        .get("short_commit")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                );
                println!(
                    "Revision: r{}",
                    publication
                        .get("revision")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0)
                );
                let push = publication
                    .get("push_status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if push != "pushed" && !push.is_empty() {
                    println!("Push: {push}");
                    if let Some(error) = publication.get("push_error").and_then(|x| x.as_str()) {
                        println!("{error}");
                    }
                }
            }
            _ => println!("Git publication:\n(none yet)"),
        }
        println!();
        println!("Live:");
        println!("r{}", u64_of(v, "live_revision"));
        println!("{} ahead", plural(u64_of(v, "revisions_ahead"), "revision"));
        println!();
        println!("State:");
        println!(
            "{}",
            str_of(v, "state").chars().take(16).collect::<String>()
        );
        println!();
        println!("Connection:");
        println!("{}", str_of(v, "connection"));
        if let Some(sync) = v.get("sync_state") {
            let label = sync.get("state").and_then(|x| x.as_str()).unwrap_or("live");
            if label != "live" {
                println!();
                println!(
                    "{}",
                    sync.get("reason").and_then(|x| x.as_str()).unwrap_or(label)
                );
                if let Some(detail) = sync.get("detail").and_then(|x| x.as_str()) {
                    println!();
                    println!("{detail}");
                }
            }
        }
        println!();
        println!("Participants:");
        if let Some(list) = v.get("participants").and_then(|x| x.as_array()) {
            if list.is_empty() {
                println!("(none)");
            }
            for peer in list {
                let mark = if peer.get("online").and_then(|x| x.as_bool()) == Some(true) {
                    "\u{2713}"
                } else {
                    "\u{00b7}"
                };
                println!("{mark} {}", str_of(peer, "display_name"));
            }
        }
        println!();
        println!("Active Task:");
        match v.get("active_task") {
            Some(Value::Object(task)) => println!(
                "{} — {}",
                short_id('T', task.get("id").and_then(|x| x.as_str()).unwrap_or("")),
                task.get("description")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
            ),
            _ => println!("(none)"),
        }
        println!();
        println!("Outbox:");
        println!("{} pending", u64_of(v, "outbox_pending"));
        println!();
        println!("Conflicts:");
        println!("{}", u64_of(v, "conflicts_open"));

        if let Some(rejected) = v.get("rejected_paths").and_then(|x| x.as_array()) {
            if !rejected.is_empty() {
                println!();
                println!("Not synchronized:");
                for item in rejected.iter().take(10) {
                    println!("  {} — {}", str_of(item, "path"), str_of(item, "reason"));
                }
            }
        }
        if let Some(notices) = v.get("notices").and_then(|x| x.as_array()) {
            if !notices.is_empty() {
                println!();
                println!("Notices:");
                for notice in notices.iter().rev().take(5) {
                    if let Some(text) = notice.as_str() {
                        println!("  {text}");
                    }
                }
            }
        }
    }

    pub fn peers(v: &Value) {
        let Some(list) = v.get("peers").and_then(|x| x.as_array()) else {
            println!("(no participants)");
            return;
        };
        if list.is_empty() {
            println!("(no participants)");
            return;
        }
        for peer in list {
            let mark = if peer.get("online").and_then(|x| x.as_bool()) == Some(true) {
                "online "
            } else {
                "offline"
            };
            println!(
                "{mark}  {:<20} {:<12} r{}",
                str_of(peer, "display_name"),
                str_of(peer, "role"),
                peer.get("last_known_revision")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
            );
            if let Some(task) = peer.get("active_task_description").and_then(|x| x.as_str()) {
                println!("         Task: {task}");
            }
        }
    }

    pub fn invite(v: &Value) {
        println!("Weave invite:");
        println!();
        println!("{}", str_of(v, "invite"));
        println!();
        println!("Endpoint: {}", str_of(v, "endpoint"));
        println!();
        println!("The invite grants full read/write access to this session.");
        println!("Send it over a channel you trust.");
    }

    pub fn task_started(v: &Value) {
        if let Some(id) = v.get("active_task_id").and_then(|x| x.as_str()) {
            println!("Task started: {}", short_id('T', id));
        } else {
            println!("Task started.");
        }
        task_list(v);
    }

    pub fn task_list(v: &Value) {
        let Some(list) = v.get("tasks").and_then(|x| x.as_array()) else {
            return;
        };
        let active: Vec<&Value> = list
            .iter()
            .filter(|t| str_of(t, "status") == "active")
            .collect();
        println!();
        if active.is_empty() {
            println!("No active Tasks.");
        } else {
            println!("Active Tasks:");
            for task in active {
                println!(
                    "  {} — {}",
                    short_id('T', str_of(task, "id")),
                    str_of(task, "description")
                );
                if let Some(scopes) = task.get("scopes").and_then(|x| x.as_array()) {
                    for scope in scopes {
                        let stale = scope.get("stale").and_then(|x| x.as_bool()) == Some(true);
                        let range = match (
                            scope.get("line_start").and_then(|x| x.as_u64()),
                            scope.get("line_end").and_then(|x| x.as_u64()),
                        ) {
                            (Some(a), Some(b)) if !stale => format!(":{a}-{b}"),
                            (Some(a), Some(b)) => format!(":{a}-{b} (stale, file-level)"),
                            _ => String::new(),
                        };
                        println!("      {}{range}", str_of(scope, "path"));
                    }
                }
            }
        }
        let done = list.len()
            - list
                .iter()
                .filter(|t| str_of(t, "status") == "active")
                .count();
        if done > 0 {
            println!();
            println!("{done} completed or cancelled Task(s).");
        }
    }

    pub fn task_show(v: &Value) {
        let task = v.get("task").cloned().unwrap_or(Value::Null);
        println!(
            "{} — {}",
            short_id('T', str_of(&task, "id")),
            str_of(&task, "description")
        );
        println!("Status: {}", str_of(&task, "status"));
        if let Some(scopes) = task.get("scopes").and_then(|x| x.as_array()) {
            if !scopes.is_empty() {
                println!();
                println!("Scope:");
                for scope in scopes {
                    println!("  {}", str_of(scope, "path"));
                }
            }
        }
        if let Some(touched) = task.get("touched_paths").and_then(|x| x.as_array()) {
            if !touched.is_empty() {
                println!();
                println!("Touched paths:");
                for path in touched {
                    println!("  {}", path.as_str().unwrap_or(""));
                }
            }
        }
        if let Some(overlaps) = v.get("overlaps").and_then(|x| x.as_array()) {
            if !overlaps.is_empty() {
                println!();
                println!("Potential overlap");
                for other in overlaps {
                    println!();
                    println!(
                        "{} — {}",
                        short_id('T', str_of(other, "task_id")),
                        str_of(other, "description")
                    );
                    if let Some(scopes) = other.get("scopes").and_then(|x| x.as_array()) {
                        println!();
                        println!("Scope:");
                        for scope in scopes {
                            println!("{}", scope.as_str().unwrap_or(""));
                        }
                    }
                }
                println!();
                println!("A Task scope is advisory. It does not prevent anyone from editing.");
            }
        }
    }

    pub fn conflict_list(v: &Value) {
        let Some(list) = v.get("conflicts").and_then(|x| x.as_array()) else {
            return;
        };
        let open: Vec<&Value> = list
            .iter()
            .filter(|c| str_of(c, "status") == "open")
            .collect();
        if open.is_empty() {
            println!("No open conflicts.");
        } else {
            for conflict in &open {
                println!(
                    "{}  {}  {}",
                    short_id('C', str_of(conflict, "id")),
                    str_of(conflict, "kind"),
                    str_of(conflict, "path")
                );
            }
            println!();
            println!("Run `weave conflict show <id>` for candidate content.");
        }
        let closed = list.len() - open.len();
        if closed > 0 {
            println!();
            println!("{closed} resolved or dismissed conflict(s).");
        }
    }

    pub fn conflict_show(v: &Value) {
        let conflict = v.get("conflict").cloned().unwrap_or(Value::Null);
        println!("Conflict {}", str_of(v, "short_id"));
        println!();
        println!("{}", str_of(&conflict, "path"));
        println!();
        println!("{}", str_of(v, "kind_description"));
        println!("No work was discarded.");
        println!();
        if let Some(actor) = v.get("incoming_actor").and_then(|x| x.as_str()) {
            println!("Incoming from: {actor}");
        }
        if let Some(task) = v.get("incoming_task") {
            if let Some(description) = task.get("description").and_then(|x| x.as_str()) {
                println!("Task: {description}");
            }
        }
        if let Some(files) = v.get("candidate_files").and_then(|x| x.as_object()) {
            if !files.is_empty() {
                println!();
                println!("Candidate content:");
                for (label, path) in files {
                    println!("  {label:<10} {}", path.as_str().unwrap_or(""));
                }
            }
        }
        println!();
        println!("Working file: {}", str_of(v, "working_tree_path"));
        println!();
        println!("Edit the working file to one coherent result, then run:");
        println!("  weave conflict resolve {}", str_of(v, "short_id"));
    }

    pub fn prepare(v: &Value) {
        println!("Prepare:");
        println!("{}", short_id('P', str_of(v, "prepare_id")));
        println!();
        println!("Target revision:");
        println!("r{}", u64_of(v, "target_revision"));
        println!();
        println!("Parent Git commit:");
        println!("{}", crate::util::short_oid(str_of(v, "parent_commit_oid")));
        println!();
        if let Some(diff) = v.get("diff_summary") {
            let count = |key: &str| {
                diff.get(key)
                    .and_then(|x| x.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            };
            println!(
                "Changes since r{}: {} added, {} modified, {} deleted",
                u64_of(v, "previous_published_revision"),
                count("added"),
                count("modified"),
                count("deleted")
            );
        }
        if let Some(contributors) = v.get("contributors").and_then(|x| x.as_array()) {
            if !contributors.is_empty() {
                println!();
                println!("Contributors:");
                for c in contributors {
                    println!(
                        "  {} ({})",
                        str_of(c, "display_name"),
                        plural(
                            c.get("revisions").and_then(|x| x.as_u64()).unwrap_or(0),
                            "revision"
                        )
                    );
                }
            }
        }
        if let Some(unassigned) = v.get("unassigned_revisions").and_then(|x| x.as_array()) {
            if !unassigned.is_empty() {
                println!();
                println!(
                    "{} not associated with a Task.",
                    plural(unassigned.len() as u64, "revision")
                );
            }
        }
        if let Some(disconnected) = v
            .get("disconnected_participants")
            .and_then(|x| x.as_array())
        {
            if !disconnected.is_empty() {
                println!();
                println!(
                    "{} disconnected.",
                    plural(disconnected.len() as u64, "participant is")
                );
                println!("Unsynchronized local work on those machines cannot be included.");
            }
        }
        println!();
        println!("Create the Git publication with:");
        println!(
            "  weave commit create {} --message \"...\"",
            str_of(v, "prepare_id")
        );
    }

    pub fn publication(v: &Value) {
        let descriptor = v.get("descriptor").cloned().unwrap_or(Value::Null);
        println!("Commit created:");
        println!(
            "{}",
            crate::util::short_oid(str_of(&descriptor, "commit_oid"))
        );
        println!();
        println!(
            "Revision: r{}",
            descriptor
                .get("target_revision")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
        );
        println!("Branch:   {}", str_of(&descriptor, "branch"));
        println!();
        match str_of(v, "push_status") {
            "pushed" => println!("Pushed to the remote."),
            "no_upstream" => println!("No upstream remote is configured; nothing was pushed."),
            "not_attempted" => println!("Push was not attempted."),
            other => {
                println!("Remote push failed: {other}");
                if let Some(error) = v.get("push_error").and_then(|x| x.as_str()) {
                    println!();
                    println!("{error}");
                }
                println!();
                println!("Live collaboration continues.");
                println!("Run:");
                println!("weave push");
            }
        }
    }

    pub fn push(v: &Value) {
        println!("{}", str_of(v, "message"));
    }

    fn short_id(prefix: char, id: &str) -> String {
        let cleaned: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let head: String = cleaned.chars().take(4).collect();
        format!("{prefix}-{}", head.to_uppercase())
    }
}
