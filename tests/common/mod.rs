// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared harness for the end-to-end Weave tests.
//!
//! The tests drive the real `weave` executable against real Git repositories,
//! so what is exercised is the shipped behaviour, not an in-process mock.

#![allow(dead_code)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn weave_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_weave"))
}

pub struct Sandbox {
    pub root: PathBuf,
}

impl Sandbox {
    pub fn new(label: &str) -> Sandbox {
        let unique = format!(
            "{}-{}-{}",
            label,
            std::process::id(),
            Instant::now().elapsed().as_nanos() as u64 ^ rand_seed()
        );
        let root = std::env::temp_dir().join("weave-tests").join(unique);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create sandbox");
        Sandbox { root }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Windows keeps SQLite files mapped briefly after the daemon exits.
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.root).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

fn rand_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    RandomState::new().build_hasher().finish()
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn git_allow_failure(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Create a repository with one initial commit and a couple of files.
pub fn init_repo(dir: &Path, name: &str, email: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-b", "main", "-q", "."]);
    git(dir, &["config", "user.name", name]);
    git(dir, &["config", "user.email", email]);
    git(dir, &["config", "core.autocrlf", "false"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    write_file(dir, "README.md", "# Deck\n\nIntro line\n");
    std::fs::create_dir_all(dir.join("slides")).unwrap();
    write_file(dir, "slides/01-intro.md", "L1\nL2\nL3\n");
    write_file(dir, ".gitignore", "node_modules/\n");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "Initial commit"]);
}

pub fn write_file(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR.trim()));
    let path = if path.exists() { path } else { dir.join(rel) };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap_or_else(|e| panic!("write {rel}: {e}"));
}

pub fn read_file(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

pub fn file_exists(dir: &Path, rel: &str) -> bool {
    dir.join(rel).exists()
}

/// One Weave participant: a repository plus its own user-level identity.
pub struct Participant {
    pub repo: PathBuf,
    pub home: PathBuf,
    pub log: PathBuf,
    daemon: Option<Child>,
}

impl Participant {
    pub fn new(sandbox: &Sandbox, name: &str) -> Participant {
        let repo = sandbox.root.join(name);
        let home = sandbox.root.join(format!("{name}-home"));
        std::fs::create_dir_all(&home).unwrap();
        Participant {
            log: sandbox.root.join(format!("{name}.log")),
            repo,
            home,
            daemon: None,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(weave_bin());
        cmd.env("WEAVE_HOME", &self.home);
        cmd.env("WEAVE_LAN_ADDRESS", "127.0.0.1");
        cmd.env("WEAVE_LOG", "weave=info");
        cmd.arg("--repo").arg(&self.repo);
        cmd
    }

    /// Run a short-lived Weave command and return its stdout.
    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.command().args(args).output().expect("run weave");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(format!(
                "weave {args:?} exited with {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                out.status.code()
            ))
        }
    }

    pub fn expect(&self, args: &[&str]) -> String {
        self.run(args).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let mut all: Vec<&str> = args.to_vec();
        all.push("--json");
        let text = self.expect(&all);
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("weave {args:?} produced invalid JSON: {e}\n{text}"))
    }

    pub fn json_allow_failure(&self, args: &[&str]) -> Result<Value, String> {
        let mut all: Vec<&str> = args.to_vec();
        all.push("--json");
        self.run(&all)
            .map(|text| serde_json::from_str(&text).expect("valid JSON"))
    }

    /// Start a long-lived daemon (`host`, `join`, `resume`).
    pub fn start_daemon(&mut self, args: &[&str]) {
        assert!(self.daemon.is_none(), "daemon already running");
        let log = std::fs::File::create(&self.log).expect("create log");
        let errlog = log.try_clone().unwrap();
        let child = self
            .command()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errlog))
            .spawn()
            .expect("spawn weave daemon");
        self.daemon = Some(child);
    }

    pub fn daemon_output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Fail immediately if the daemon died in a way that would otherwise show
    /// up only as a wait timing out much later.
    ///
    /// A panic inside a network task once made a participant sit "offline"
    /// forever; every wait below checks for it so the failure names its cause
    /// instead of a stale timeout.
    pub fn assert_daemon_healthy(&self) {
        let log = self.daemon_output();
        for marker in ["panicked at", "connection task stopped"] {
            assert!(
                !log.contains(marker),
                "the daemon in {} logged `{marker}`:\n{log}",
                self.repo.display()
            );
        }
    }

    /// Ask the daemon to stop and wait for the process to exit.
    pub fn stop_daemon(&mut self) {
        self.shutdown_daemon("stop");
    }

    /// Leave the session (forgetting its local record) and wait for exit.
    pub fn leave_session(&mut self) {
        self.shutdown_daemon("leave");
    }

    fn shutdown_daemon(&mut self, command: &str) {
        if self.daemon.is_none() {
            return;
        }
        let _ = self.run(&[command]);
        let mut child = self.daemon.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }

    pub fn status(&self) -> Value {
        self.json(&["status"])
    }

    /// Wait until `predicate` sees a status snapshot it likes.
    pub fn wait_for_status(
        &self,
        what: &str,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        let mut last = Value::Null;
        loop {
            if let Ok(value) = self.json_allow_failure(&["status"]) {
                if predicate(&value) {
                    return value;
                }
                last = value;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what} in {}\nlast status: {}\ndaemon log:\n{}",
                    self.repo.display(),
                    serde_json::to_string_pretty(&last).unwrap_or_default(),
                    self.daemon_output()
                );
            }
            self.assert_daemon_healthy();
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    pub fn wait_online(&self, timeout: Duration) {
        self.wait_for_status("connection to become online", timeout, |v| {
            v["active"].as_bool() == Some(true) && v["connection"].as_str() == Some("online")
        });
    }

    /// Wait for a file to hold exactly `expected`.
    pub fn wait_for_file(&self, rel: &str, expected: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(self.repo.join(rel)) {
                if text == expected {
                    return;
                }
            }
            if Instant::now() >= deadline {
                let actual = std::fs::read_to_string(self.repo.join(rel))
                    .unwrap_or_else(|e| format!("<unreadable: {e}>"));
                panic!(
                    "timed out waiting for {rel} in {}\nexpected:\n{expected:?}\nactual:\n{actual:?}\n\
                     status: {}\ndaemon log:\n{}",
                    self.repo.display(),
                    serde_json::to_string_pretty(&self.json_allow_failure(&["status"]).unwrap_or(Value::Null))
                        .unwrap_or_default(),
                    self.daemon_output()
                );
            }
            self.assert_daemon_healthy();
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Wait for a file to hold exactly `expected` bytes.
    pub fn wait_for_bytes(&self, rel: &str, expected: &[u8], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(bytes) = std::fs::read(self.repo.join(rel)) {
                if bytes == expected {
                    return;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {rel} ({} bytes) in {}\ndaemon log:\n{}",
                    expected.len(),
                    self.repo.display(),
                    self.daemon_output()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Wait until a read-only git command returns `expected`.
    pub fn wait_for_git(&self, args: &[&str], expected: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let (ok, output) = git_allow_failure(&self.repo, args);
            if ok && output.trim() == expected {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for `git {args:?}` to be {expected} in {} (last: {})\n\
                     daemon log:\n{}",
                    self.repo.display(),
                    output.trim(),
                    self.daemon_output()
                );
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Wait until the host can produce an invite, returning it.
    pub fn wait_for_invite(&self, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(value) = self.json_allow_failure(&["invite"]) {
                if value["endpoint"].as_str().is_some_and(|e| !e.is_empty()) {
                    return value;
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for an invite in {}\ndaemon log:\n{}",
                    self.repo.display(),
                    self.daemon_output()
                );
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Wait until this participant has an active Task, returning its id.
    pub fn wait_for_active_task(&self, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(value) = self.json_allow_failure(&["task", "list"]) {
                if let Some(id) = value["active_task_id"].as_str() {
                    return id.to_string();
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for an active Task in {}\ndaemon log:\n{}",
                    self.repo.display(),
                    self.daemon_output()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn wait_for_missing(&self, rel: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.repo.join(rel).exists() {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {rel} to disappear in {}",
                    self.repo.display()
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Wait until at least one open conflict exists, returning its detail.
    pub fn wait_for_conflict(&self, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(list) = self.json_allow_failure(&["conflict", "list"]) {
                if let Some(conflicts) = list["conflicts"].as_array() {
                    if let Some(open) = conflicts.iter().find(|c| c["status"] == "open") {
                        let id = open["id"].as_str().unwrap().to_string();
                        return self.json(&["conflict", "show", &id]);
                    }
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for a conflict in {}\ndaemon log:\n{}",
                    self.repo.display(),
                    self.daemon_output()
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub const LONG: Duration = Duration::from_secs(60);
pub const SHORT: Duration = Duration::from_secs(25);
