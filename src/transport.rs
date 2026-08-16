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

/// Data frames a connection will hold before a transfer has to wait.
///
/// Small on purpose. It exists to keep the socket busy between reads of the
/// blob store, not to buffer a transfer: the real queue is the file on disk,
/// which costs nothing to leave there. At one Noise message each, this is under
/// 512 KiB per connection regardless of how large the file is.
pub const MAX_QUEUED_DATA_FRAMES: usize = 8;

/// One outbound application message, tagged with the plane it belongs to.
pub enum Frame {
    /// A JSON protocol envelope.
    Control(String),
    /// A blob-plane payload, framed by the blob transfer protocol.
    Data(Vec<u8>),
}

/// A bounded outbound queue for one peer, split into two planes.
///
/// The control queue is bounded by both message count and total queued bytes,
/// and exceeding either bound disconnects the slow peer rather than growing
/// host memory without limit (specification sections 65, 197). It has to work
/// that way: control messages are produced by a synchronous state machine that
/// cannot wait for a socket.
///
/// The data queue never disconnects anybody. Its producers are async transfer
/// pumps that can simply wait, so a slow participant slows its own transfers
/// down instead of losing its session. See `docs/BLOB-PLANE.md`.
#[derive(Clone)]
pub struct Outbound {
    tx: mpsc::Sender<String>,
    data_tx: mpsc::Sender<Vec<u8>>,
    queued_bytes: Arc<AtomicUsize>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

/// The receiving half of both planes, drained by a connection's writer task.
pub struct OutboundRx {
    control: mpsc::Receiver<String>,
    data: mpsc::Receiver<Vec<u8>>,
}

impl OutboundRx {
    /// The next frame to write, control first.
    ///
    /// Strict priority, not a weighted share: a bulk transfer must never delay
    /// an acknowledgement or a heartbeat by more than the single data frame
    /// already being written, or a healthy session looks frozen.
    pub async fn next(&mut self) -> Option<Frame> {
        loop {
            match self.control.try_recv() {
                Ok(text) => return Some(Frame::Control(text)),
                Err(mpsc::error::TryRecvError::Empty) => {}
                // Control is gone for good; drain whatever data remains.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return self.data.recv().await.map(Frame::Data)
                }
            }
            tokio::select! {
                biased;
                text = self.control.recv() => match text {
                    Some(text) => return Some(Frame::Control(text)),
                    // Closed and drained: the next pass takes the branch above.
                    None => continue,
                },
                bytes = self.data.recv() => return bytes.map(Frame::Data),
            }
        }
    }
}

impl Outbound {
    pub fn new(capacity: usize) -> (Outbound, OutboundRx, Arc<AtomicUsize>) {
        let (tx, control) = mpsc::channel(capacity);
        let (data_tx, data) = mpsc::channel(MAX_QUEUED_DATA_FRAMES);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        (
            Outbound {
                tx,
                data_tx,
                queued_bytes: queued_bytes.clone(),
                closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            OutboundRx { control, data },
            queued_bytes,
        )
    }

    /// Enqueue a control frame. Returns `false` when the peer is too slow or
    /// gone; the caller must then drop the connection.
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

    /// Enqueue a data frame, waiting for room rather than failing.
    ///
    /// This is the backpressure: the caller is a transfer pump reading from the
    /// blob store, and making it wait here is exactly what "slow down" means.
    /// Returns `false` only when the connection is finished.
    pub async fn send_data(&self, payload: Vec<u8>) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        debug_assert!(payload.len() <= crate::secure::MAX_DATA_MESSAGE);
        self.data_tx.send(payload).await.is_ok()
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

pub fn default_outbound() -> (Outbound, OutboundRx, Arc<AtomicUsize>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(frame: Option<Frame>) -> String {
        match frame {
            Some(Frame::Control(text)) => text,
            Some(Frame::Data(_)) => panic!("expected a control frame, got data"),
            None => panic!("expected a frame, got end of stream"),
        }
    }

    fn data(frame: Option<Frame>) -> Vec<u8> {
        match frame {
            Some(Frame::Data(bytes)) => bytes,
            Some(Frame::Control(text)) => panic!("expected a data frame, got control: {text}"),
            None => panic!("expected a frame, got end of stream"),
        }
    }

    /// The regression this whole split exists to prevent: a transfer with
    /// frames already queued must not delay a control message behind them.
    #[tokio::test]
    async fn control_frames_overtake_queued_data() {
        let (out, mut rx, _) = default_outbound();
        for i in 0..MAX_QUEUED_DATA_FRAMES {
            assert!(out.send_data(vec![i as u8; 16]).await);
        }
        assert!(out.send_text("{\"ack\":1}".into()));

        assert_eq!(text(rx.next().await), "{\"ack\":1}");
        for i in 0..MAX_QUEUED_DATA_FRAMES {
            assert_eq!(data(rx.next().await), vec![i as u8; 16]);
        }
    }

    /// A full data queue makes the sender wait. It must not close the
    /// connection, and it must not drop the frame.
    #[tokio::test]
    async fn a_full_data_queue_slows_the_sender_instead_of_dropping_it() {
        let (out, mut rx, _) = default_outbound();
        for i in 0..MAX_QUEUED_DATA_FRAMES {
            assert!(out.send_data(vec![i as u8]).await);
        }

        let sender = tokio::spawn({
            let out = out.clone();
            async move { out.send_data(vec![0xFF]).await }
        });
        // Nothing is dropped and nothing is closed; the send is simply pending.
        tokio::task::yield_now().await;
        assert!(!sender.is_finished());
        assert!(!out.is_closed());

        assert_eq!(data(rx.next().await), vec![0u8]);
        assert!(sender.await.unwrap());

        for i in 1..MAX_QUEUED_DATA_FRAMES {
            assert_eq!(data(rx.next().await), vec![i as u8]);
        }
        assert_eq!(data(rx.next().await), vec![0xFFu8]);
    }

    /// Control accounting still has to disconnect: it is fed by a synchronous
    /// state machine with nowhere to wait.
    #[tokio::test]
    async fn an_oversized_control_backlog_still_closes_the_connection() {
        let (out, _rx, queued) = default_outbound();
        let big = "x".repeat(MAX_QUEUED_BYTES / 4);
        for _ in 0..4 {
            assert!(out.send_text(big.clone()));
        }
        assert_eq!(queued.load(Ordering::Relaxed), MAX_QUEUED_BYTES);
        assert!(!out.send_text("one byte too many".into()));
        assert!(out.is_closed());
    }

    #[tokio::test]
    async fn a_closed_connection_accepts_no_further_frames_on_either_plane() {
        let (out, _rx, _) = default_outbound();
        out.close();
        assert!(!out.send_text("{}".into()));
        assert!(!out.send_data(vec![1, 2, 3]).await);
    }

    /// Both senders live in the same `Outbound`, so dropping it must end the
    /// writer rather than leave it parked on one plane forever.
    #[tokio::test]
    async fn dropping_the_sender_ends_the_stream() {
        let (out, mut rx, _) = default_outbound();
        assert!(out.send_data(vec![7]).await);
        drop(out);
        assert_eq!(data(rx.next().await), vec![7u8]);
        assert!(rx.next().await.is_none());
    }
}
