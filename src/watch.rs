// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Filesystem watcher with debouncing.
//!
//! Specification sections 31-33. Watcher events are hints, never the source of
//! truth: a dropped or overflowed event turns into a full rescan request, and
//! the engine rescans at every point the specification requires anyway.

use crate::error::{persistence, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// Debounce window (specification section 33).
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Never hold a batch longer than this, even under continuous writes.
const MAX_BATCH_AGE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// Repository-relative paths (with `/` separators) that may have changed.
    Changed(Vec<String>),
    /// The watcher lost events or failed; the engine must rescan fully.
    RescanNeeded(String),
}

pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
    _debouncer: std::thread::JoinHandle<()>,
}

/// Start watching `root` recursively. Debounced batches are delivered on `out`.
pub fn start(root: &Path, out: Sender<WatchEvent>) -> Result<WatchHandle> {
    let (raw_tx, raw_rx): (Sender<RawEvent>, Receiver<RawEvent>) = channel();

    let tx_for_watcher = raw_tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(event) => {
            let overflow = matches!(event.kind, EventKind::Other) && event.paths.is_empty();
            if overflow {
                let _ = tx_for_watcher.send(RawEvent::Rescan("watcher overflow".into()));
            } else {
                let _ = tx_for_watcher.send(RawEvent::Paths(event.paths));
            }
        }
        Err(e) => {
            let _ = tx_for_watcher.send(RawEvent::Rescan(format!("watcher error: {e}")));
        }
    })
    .map_err(|e| persistence(format!("Could not start the filesystem watcher: {e}")))?;

    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| persistence(format!("Could not watch {}: {e}", root.display())))?;

    let root_owned = root.to_path_buf();
    let debouncer = std::thread::Builder::new()
        .name("weave-watch-debounce".into())
        .spawn(move || debounce_loop(root_owned, raw_rx, out))
        .map_err(|e| persistence(format!("Could not start the watcher thread: {e}")))?;

    Ok(WatchHandle {
        _watcher: watcher,
        _debouncer: debouncer,
    })
}

enum RawEvent {
    Paths(Vec<PathBuf>),
    Rescan(String),
}

fn debounce_loop(root: PathBuf, rx: Receiver<RawEvent>, out: Sender<WatchEvent>) {
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut first_seen: Option<Instant> = None;

    loop {
        let timeout = match first_seen {
            Some(start) => {
                let age = start.elapsed();
                if age >= MAX_BATCH_AGE {
                    Duration::from_millis(0)
                } else {
                    DEBOUNCE.min(MAX_BATCH_AGE - age)
                }
            }
            None => Duration::from_millis(500),
        };

        match rx.recv_timeout(timeout) {
            Ok(RawEvent::Paths(paths)) => {
                for p in paths {
                    if let Some(rel) = relativize(&root, &p) {
                        pending.insert(rel);
                    }
                }
                if !pending.is_empty() && first_seen.is_none() {
                    first_seen = Some(Instant::now());
                }
            }
            Ok(RawEvent::Rescan(reason)) => {
                pending.clear();
                first_seen = None;
                if out.send(WatchEvent::RescanNeeded(reason)).is_err() {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !pending.is_empty() {
                    let batch: Vec<String> = std::mem::take(&mut pending).into_iter().collect();
                    first_seen = None;
                    if out.send(WatchEvent::Changed(batch)).is_err() {
                        return;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if !pending.is_empty() {
                    let batch: Vec<String> = std::mem::take(&mut pending).into_iter().collect();
                    let _ = out.send(WatchEvent::Changed(batch));
                }
                return;
            }
        }
    }
}

/// Convert an absolute event path into a repository-relative `/`-separated
/// string, discarding anything inside `.git` (specification section 46).
fn relativize(root: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(os) => {
                let s = os.to_str()?;
                if s.eq_ignore_ascii_case(".git") {
                    return None;
                }
                parts.push(s.to_string());
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}
