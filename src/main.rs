// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `weave` executable.

use clap::Parser;
use weave::cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let json_mode = uses_json(&cli.command);
    match weave::cli::run(cli) {
        Ok(()) => {}
        Err(error) => {
            if json_mode {
                let payload = serde_json::json!({
                    "ok": false,
                    "class": error.class,
                    "message": error.message,
                    "detail": error.detail,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
                );
            }
            // Diagnostics always go to stderr so `--json` stdout stays clean.
            eprintln!("{}", error.message);
            if let Some(detail) = &error.detail {
                eprintln!();
                eprintln!("{detail}");
            }
            std::process::exit(error.exit_code());
        }
    }
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if verbose { "weave=debug" } else { "weave=warn" };
    let filter = EnvFilter::try_from_env("WEAVE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Whether the selected command was asked for machine-readable output, so a
/// failure can also be reported as JSON on stdout.
fn uses_json(command: &Command) -> bool {
    use weave::cli::*;
    match command {
        Command::Status(a)
        | Command::Peers(a)
        | Command::Invite(a)
        | Command::Rescan(a)
        | Command::Leave(a)
        | Command::Stop(a)
        | Command::Push(a) => a.json,
        // `doctor --json` has already printed the report, and the report's
        // own `ready: false` is the machine-readable failure. Adding an error
        // object would put two JSON documents on stdout and break every parser.
        Command::Doctor(_) => false,
        Command::Tunnel(TunnelCommand::Restart(a)) => a.json,
        Command::Agent(AgentCommand::Bootstrap(a)) => a.json,
        Command::Recover(a) => a.json,
        Command::Task(t) => match t {
            TaskCommand::Start { json, .. }
            | TaskCommand::Show { json, .. }
            | TaskCommand::Update { json, .. }
            | TaskCommand::Complete { json, .. }
            | TaskCommand::Cancel { json, .. } => *json,
            TaskCommand::List(a) => a.json,
        },
        Command::Conflict(c) => match c {
            ConflictCommand::Show { json, .. }
            | ConflictCommand::Resolve { json, .. }
            | ConflictCommand::Dismiss { json, .. } => *json,
            ConflictCommand::List(a) => a.json,
        },
        Command::Commit(c) => match c {
            CommitCommand::Prepare { json, .. } | CommitCommand::Create { json, .. } => *json,
        },
        Command::Config(c) => match c {
            ConfigCommand::Get { json, .. } | ConfigCommand::Set { json, .. } => *json,
            ConfigCommand::List(a) => a.json,
        },
        Command::Host(_) | Command::Join(_) | Command::Resume => false,
    }
}
