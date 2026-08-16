// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Network transport: one long-lived WebSocket per participant.
//!
//! Specification sections 52-53 (transport and serialization), 58 (remote
//! authentication), 64 (host connection model), 65-66 (backpressure and
//! message limits), 67 (heartbeats).
//!
//! Every remote frame travels inside a Noise session; see [`crate::secure`].
//! This module owns the queueing and the limits, not the cryptography.
//!
//! The host's own participation uses an in-process loopback pair carrying the
//! identical JSON frames, so the host's edits travel the same logical path as
//! everyone else's (specification section 5).

use crate::model::{MAX_PROTOCOL_MESSAGE, MAX_QUEUED_BYTES, MAX_QUEUED_MESSAGES};
use crate::proto::{ClientEnvelope, ClientMessage, HostEnvelope, HostMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A bounded outbound queue for one peer.
///
/// Bounded by both message count and total queued bytes; exceeding either bound
/// disconnects the slow peer rather than growing host memory without limit
/// (specification sections 65, 197). The peer recovers on reconnect.
#[derive(Clone)]
pub struct Outbound {
    tx: mpsc::Sender<String>,
    queued_bytes: Arc<AtomicUsize>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Outbound {
    pub fn new(capacity: usize) -> (Outbound, mpsc::Receiver<String>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::channel(capacity);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        (
            Outbound {
                tx,
                queued_bytes: queued_bytes.clone(),
                closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            rx,
            queued_bytes,
        )
    }

    /// Enqueue a frame. Returns `false` when the peer is too slow or gone; the
    /// caller must then drop the connection.
    pub fn send_text(&self, text: String) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        let len = text.len();
        if self.queued_bytes.load(Ordering::Relaxed) + len > MAX_QUEUED_BYTES {
            self.closed.store(true, Ordering::Relaxed);
            return false;
        }
        match self.tx.try_send(text) {
            Ok(()) => {
                self.queued_bytes.fetch_add(len, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.closed.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn send_host(&self, message: HostMessage) -> bool {
        match serde_json::to_string(&HostEnvelope::wrap(message)) {
            Ok(text) => self.send_text(text),
            Err(e) => {
                tracing::error!("could not serialize host message: {e}");
                true
            }
        }
    }

    pub fn send_client(&self, message: ClientMessage) -> bool {
        match serde_json::to_string(&ClientEnvelope::wrap(message)) {
            Ok(text) => self.send_text(text),
            Err(e) => {
                tracing::error!("could not serialize client message: {e}");
                true
            }
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

pub fn default_outbound() -> (Outbound, mpsc::Receiver<String>, Arc<AtomicUsize>) {
    Outbound::new(MAX_QUEUED_MESSAGES)
}

/// Constant-time token comparison for the loopback IPC token.
///
/// The session secret is never compared this way and never crosses the network:
/// remote peers prove possession by completing the Noise handshake in
/// [`crate::secure`].
pub fn secret_matches(expected: &str, provided: &str) -> bool {
    let a = expected.as_bytes();
    let b = provided.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// WebSocket route.
///
/// The `/v2` suffix is the visible half of the protocol break that introduced
/// end-to-end encryption: a Weave 1.x peer asks for `/weave` and gets a 404
/// rather than an unencrypted session, and a Weave 2 peer pointed at a 1.x host
/// gets the same clear failure instead of a silent downgrade.
pub const WS_PATH: &str = "/weave/v2";

/// Largest WebSocket frame accepted. One Noise message per frame, so this is
/// far below the application message limit; large application messages are
/// split across frames by [`crate::secure::SecureChannel`].
pub const MAX_FRAME: usize = crate::secure::MAX_WS_FRAME;

/// Largest application message, after reassembly (specification section 66).
pub const MAX_MESSAGE: usize = MAX_PROTOCOL_MESSAGE;
