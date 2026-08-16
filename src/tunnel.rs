// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cloudflare Quick Tunnel lifecycle (specification sections 59-62).
//!
//! The coordinator only ever binds to loopback; `cloudflared` provides the
//! public HTTPS/WebSocket endpoint. Tunnel identity is not session identity: a
//! dead tunnel can be replaced without recreating the Weave session.

use crate::error::{network, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

pub struct Tunnel {
    child: Child,
    pub public_url: String,
}

impl Tunnel {
    /// WebSocket URL participants should connect to.
    pub fn websocket_url(&self) -> String {
        let base = self.public_url.trim_end_matches('/');
        let ws = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            format!("wss://{base}")
        };
        format!("{ws}{}", crate::transport::WS_PATH)
    }

    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

pub fn cloudflared_available() -> bool {
    std::process::Command::new("cloudflared")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn missing_cloudflared_error() -> crate::error::WeaveError {
    network("Remote sessions require cloudflared.").with_detail(
        "Install cloudflared and put it on PATH, or start a local-network session:\n\n\
         weave host --lan",
    )
}

/// Launch `cloudflared tunnel --url http://127.0.0.1:<port>` and wait for the
/// generated `trycloudflare.com` hostname.
pub async fn start(port: u16) -> Result<Tunnel> {
    if !cloudflared_available() {
        return Err(missing_cloudflared_error());
    }
    let mut child = Command::new("cloudflared")
        .arg("tunnel")
        .arg("--no-autoupdate")
        .arg("--url")
        .arg(format!("http://127.0.0.1:{port}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| network(format!("Could not start cloudflared: {e}")))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);

    if let Some(stdout) = stdout {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }
    if let Some(stderr) = stderr {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line).await;
            }
        });
    }
    drop(tx);

    let mut transcript: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let _ = child.kill().await;
            return Err(tunnel_failed(&transcript));
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(line)) => {
                if let Some(url) = extract_url(&line) {
                    // Drain remaining output in the background so cloudflared
                    // never blocks on a full pipe.
                    tokio::spawn(async move { while rx.recv().await.is_some() {} });
                    return Ok(Tunnel {
                        child,
                        public_url: url,
                    });
                }
                transcript.push(line);
                if transcript.len() > 200 {
                    transcript.remove(0);
                }
            }
            Ok(None) => {
                let _ = child.kill().await;
                return Err(tunnel_failed(&transcript));
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(tunnel_failed(&transcript));
            }
        }
    }
}

fn tunnel_failed(transcript: &[String]) -> crate::error::WeaveError {
    let tail: Vec<&str> = transcript
        .iter()
        .rev()
        .take(12)
        .map(|s| s.as_str())
        .collect();
    let mut detail = String::from(
        "cloudflared did not report a Quick Tunnel URL.\n\n\
         Quick Tunnels may not work when a local .cloudflared/config.yaml is present. Weave does \
         not modify your Cloudflare configuration.\n\nAlternatives:\n  weave host --lan\n",
    );
    if !tail.is_empty() {
        detail.push_str("\ncloudflared output:\n");
        for line in tail.into_iter().rev() {
            detail.push_str(line);
            detail.push('\n');
        }
    }
    network("Could not start the Cloudflare Quick Tunnel.").with_detail(detail)
}

/// Pull a `trycloudflare.com` (or other Cloudflare-assigned) URL out of a log
/// line. cloudflared prints it inside a decorated banner.
fn extract_url(line: &str) -> Option<String> {
    let idx = line.find("https://")?;
    let rest = &line[idx..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == '"' || c == '\'')
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',']).to_string();
    if url.contains("trycloudflare.com") || url.contains(".cfargotunnel.com") {
        Some(url)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::extract_url;

    #[test]
    fn finds_quick_tunnel_url() {
        let line = "2026-08-16T10:00:00Z INF |  https://tasty-blue-panda.trycloudflare.com   |";
        assert_eq!(
            extract_url(line).as_deref(),
            Some("https://tasty-blue-panda.trycloudflare.com")
        );
    }

    #[test]
    fn ignores_unrelated_urls() {
        assert!(extract_url("see https://developers.cloudflare.com/docs").is_none());
    }
}
