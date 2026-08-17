// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The data plane: how blob bytes are framed, and how a receiver installs them.
//!
//! Content no longer travels inside the JSON control messages. It is streamed
//! beside them on the same Noise session, in frames the control plane may
//! overtake at any point, and referenced everywhere else by hash alone. See
//! `docs/BLOB-PLANE.md`.
//!
//! Both ends run the same two half-machines, because both ends both send and
//! receive: a participant uploads a blob before submitting the operation that
//! references it, and downloads canonical blobs it does not hold. Only the
//! receiving half needs state, and it lives here.

use crate::blobs::{BlobStore, BlobWriter};
use crate::error::{protocol, Result};
use crate::secure::MAX_DATA_MESSAGE;
use std::collections::HashMap;

/// `[kind][transfer_id]`, big-endian.
const HEADER: usize = 1 + 8;

/// Payload bytes in one data frame.
///
/// Whatever is left of a single Noise message after the header, which is what
/// keeps a data frame from ever delaying a control message by more than one
/// frame on the wire.
pub const WIRE_CHUNK: usize = MAX_DATA_MESSAGE - HEADER;

const KIND_CHUNK: u8 = 0;
const KIND_END: u8 = 1;
const KIND_ABORT: u8 = 2;

/// Transfers one peer may have open towards this one at the same time.
///
/// Each open transfer costs one `.part` file and one hasher, so this is a
/// bound on what an authenticated peer can make the other side hold.
pub const MAX_OPEN_TRANSFERS: usize = 16;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// A decoded data frame, borrowing the payload rather than copying it.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming<'a> {
    Chunk {
        transfer_id: u64,
        bytes: &'a [u8],
    },
    /// Every byte has been sent. In-band, not a control message: control has
    /// strict priority and an end marker sent that way would overtake the
    /// chunks it terminates.
    End {
        transfer_id: u64,
    },
    /// The sender gave up. Also in-band, for the same reason.
    Abort {
        transfer_id: u64,
    },
}

fn frame(kind: u8, transfer_id: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(kind);
    out.extend_from_slice(&transfer_id.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn chunk_frame(transfer_id: u64, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= WIRE_CHUNK);
    frame(KIND_CHUNK, transfer_id, payload)
}

pub fn end_frame(transfer_id: u64) -> Vec<u8> {
    frame(KIND_END, transfer_id, &[])
}

pub fn abort_frame(transfer_id: u64) -> Vec<u8> {
    frame(KIND_ABORT, transfer_id, &[])
}

pub fn decode(bytes: &[u8]) -> Result<Incoming<'_>> {
    if bytes.len() < HEADER {
        return Err(protocol("Truncated Weave data frame."));
    }
    let transfer_id = u64::from_be_bytes(bytes[1..HEADER].try_into().expect("8 bytes"));
    let payload = &bytes[HEADER..];
    match bytes[0] {
        KIND_CHUNK => Ok(Incoming::Chunk {
            transfer_id,
            bytes: payload,
        }),
        KIND_END if payload.is_empty() => Ok(Incoming::End { transfer_id }),
        KIND_ABORT if payload.is_empty() => Ok(Incoming::Abort { transfer_id }),
        _ => Err(protocol("Unknown Weave data frame.")),
    }
}

// ---------------------------------------------------------------------------
// Receiving
// ---------------------------------------------------------------------------

/// What accepting one frame produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivered {
    /// Bytes were written; the transfer continues.
    More,
    /// The blob is durably installed under the announced hash.
    Installed { transfer_id: u64, hash: String },
    /// The transfer is over and installed nothing. Every path here leaves the
    /// blob store exactly as it was.
    Failed {
        transfer_id: u64,
        hash: String,
        reason: String,
    },
    /// A frame for a transfer this side never opened, or already finished.
    /// Not fatal: an abort and a late chunk can cross.
    Stray { transfer_id: u64 },
}

struct Transfer {
    hash: String,
    /// Bytes still expected. A sender that overshoots it is refused before
    /// anything is written, so `size` bounds the disk one transfer can use.
    remaining: u64,
    writer: BlobWriter,
}

/// The receiving half of the data plane for one peer.
///
/// Transfers are keyed by the sender's `transfer_id` and hold nothing in
/// common, so concurrent transfers cannot affect each other: separate hashers,
/// separate temporary files, separate byte budgets.
pub struct BlobReceiver {
    blobs: BlobStore,
    open: HashMap<u64, Transfer>,
}

impl BlobReceiver {
    pub fn new(blobs: BlobStore) -> BlobReceiver {
        BlobReceiver {
            blobs,
            open: HashMap::new(),
        }
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    pub fn is_open(&self, transfer_id: u64) -> bool {
        self.open.contains_key(&transfer_id)
    }

    /// Accept an offer and prepare to receive, returning the offset the sender
    /// should start from.
    ///
    /// The receiver chooses the offset because only it knows what it already
    /// holds. A previous attempt at the same content - cut short by a
    /// disconnection, a crash, or a peer that went away - leaves a partial file
    /// named after that content, and this is where it is picked up again. The
    /// offset is what the writer has re-hashed off the disk, never what a
    /// previous run claimed to have written, so resuming can only ever install
    /// bytes this side has verified end to end.
    pub fn accept_offer(&mut self, transfer_id: u64, hash: &str, size: u64) -> Result<u64> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(protocol(format!("Malformed blob reference: {hash}")));
        }
        if self.open.len() >= MAX_OPEN_TRANSFERS && !self.open.contains_key(&transfer_id) {
            return Err(protocol(format!(
                "Too many Weave blob transfers at once (limit {MAX_OPEN_TRANSFERS})."
            )));
        }
        // Re-offering an open transfer id would silently orphan the writer
        // already under it, and with it the bytes already received.
        if self.open.contains_key(&transfer_id) {
            return Err(protocol(format!(
                "Weave blob transfer {transfer_id} is already open."
            )));
        }
        self.blobs.ensure_room_for(size)?;
        let mut writer = match self.blobs.resume_writer(hash)? {
            Some(writer) => writer,
            // Another receiver in this process is already fetching this
            // content. Falling back to an anonymous writer is always correct,
            // merely not resumable, and it keeps the two transfers from
            // appending into one file.
            None => self.blobs.writer()?,
        };
        let mut from_offset = writer.written();
        if from_offset > size {
            // Longer than what is on offer, so it cannot be a prefix of it,
            // whatever it is. Continuing could only produce a mismatch.
            writer.reset()?;
            from_offset = 0;
        }
        if from_offset > 0 {
            tracing::info!(
                "resuming blob {} at offset {from_offset} of {size}",
                crate::util::short_oid(hash)
            );
        }
        self.open.insert(
            transfer_id,
            Transfer {
                hash: hash.to_string(),
                remaining: size - from_offset,
                writer,
            },
        );
        Ok(from_offset)
    }

    /// Apply one decoded data frame.
    ///
    /// Nothing here can install content that does not hash to the announced
    /// value: the writer verifies on `finish_expecting`, and every failure
    /// path drops the writer, whose `Drop` removes the partial file.
    pub fn accept(&mut self, frame: Incoming<'_>) -> Delivered {
        match frame {
            Incoming::Chunk { transfer_id, bytes } => self.chunk(transfer_id, bytes),
            Incoming::End { transfer_id } => self.end(transfer_id),
            Incoming::Abort { transfer_id } => match self.open.remove(&transfer_id) {
                Some(transfer) => Delivered::Failed {
                    transfer_id,
                    hash: transfer.hash,
                    reason: "the sender aborted the transfer".into(),
                },
                None => Delivered::Stray { transfer_id },
            },
        }
    }

    fn chunk(&mut self, transfer_id: u64, bytes: &[u8]) -> Delivered {
        let Some(transfer) = self.open.get_mut(&transfer_id) else {
            return Delivered::Stray { transfer_id };
        };
        if bytes.len() as u64 > transfer.remaining {
            return self.fail(transfer_id, "the sender exceeded the announced size");
        }
        if let Err(e) = transfer.writer.write(bytes) {
            let reason = e.message.clone();
            return self.fail(transfer_id, &reason);
        }
        transfer.remaining -= bytes.len() as u64;
        Delivered::More
    }

    fn end(&mut self, transfer_id: u64) -> Delivered {
        let Some(transfer) = self.open.remove(&transfer_id) else {
            return Delivered::Stray { transfer_id };
        };
        if transfer.remaining != 0 {
            return Delivered::Failed {
                transfer_id,
                hash: transfer.hash,
                reason: format!("{} byte(s) never arrived", transfer.remaining),
            };
        }
        match transfer.writer.finish_expecting(&transfer.hash) {
            Ok(_) => Delivered::Installed {
                transfer_id,
                hash: transfer.hash,
            },
            Err(e) => Delivered::Failed {
                transfer_id,
                hash: transfer.hash,
                reason: e.message,
            },
        }
    }

    fn fail(&mut self, transfer_id: u64, reason: &str) -> Delivered {
        // Removing drops the writer. Nothing is installed either way; a
        // resumable partial keeps the prefix it verified, an anonymous one is
        // deleted.
        let hash = self
            .open
            .remove(&transfer_id)
            .map(|t| t.hash)
            .unwrap_or_default();
        Delivered::Failed {
            transfer_id,
            hash,
            reason: reason.to_string(),
        }
    }

    /// Abandon one transfer without installing anything.
    pub fn cancel(&mut self, transfer_id: u64) {
        self.open.remove(&transfer_id);
    }

    /// Abandon every open transfer, installing nothing.
    ///
    /// Used when the connection they were negotiated on ends: transfer ids mean
    /// nothing across sockets, so every transfer has to be renegotiated. The
    /// bytes are not thrown away with the ids - each partial is named after its
    /// content, and the transfer that replaces it resumes from there.
    pub fn clear(&mut self) {
        self.open.clear();
    }
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// Allocates transfer ids for one direction of one connection.
///
/// Ids are only ever interpreted by the peer they were sent to, so each sender
/// numbering from zero is enough to keep them unambiguous.
#[derive(Debug, Default)]
pub struct TransferIds(u64);

impl TransferIds {
    pub fn next_id(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::sha256_hex;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir = std::env::temp_dir().join(format!("weave-blobwire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn store(&self) -> BlobStore {
            BlobStore::open(self.0.join("blobs")).unwrap()
        }

        /// Anonymous temporaries live directly in the blob root; a clean store
        /// has none of them.
        fn parts(&self) -> usize {
            std::fs::read_dir(self.0.join("blobs"))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count()
        }

        /// Resumable partials, and how much each holds.
        fn partials(&self) -> Vec<u64> {
            let dir = self.0.join("blobs").join(".partial");
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn send(receiver: &mut BlobReceiver, id: u64, bytes: &[u8]) -> Delivered {
        let hash = sha256_hex(bytes);
        receiver
            .accept_offer(id, &hash, bytes.len() as u64)
            .unwrap();
        send_open(receiver, id, bytes)
    }

    /// Same, for a transfer whose offer the caller has already made.
    fn send_open(receiver: &mut BlobReceiver, id: u64, bytes: &[u8]) -> Delivered {
        for slice in bytes.chunks(WIRE_CHUNK).filter(|s| !s.is_empty()) {
            let frame = chunk_frame(id, slice);
            let outcome = receiver.accept(decode(&frame).unwrap());
            assert_eq!(outcome, Delivered::More);
        }
        let end = end_frame(id);
        receiver.accept(decode(&end).unwrap())
    }

    #[test]
    fn frames_round_trip_including_the_empty_payload() {
        let payload = vec![0xA5u8; WIRE_CHUNK];
        let encoded = chunk_frame(7, &payload);
        assert!(encoded.len() <= MAX_DATA_MESSAGE);
        assert_eq!(
            decode(&encoded).unwrap(),
            Incoming::Chunk {
                transfer_id: 7,
                bytes: &payload
            }
        );
        assert_eq!(
            decode(&end_frame(u64::MAX)).unwrap(),
            Incoming::End {
                transfer_id: u64::MAX
            }
        );
        assert_eq!(
            decode(&abort_frame(0)).unwrap(),
            Incoming::Abort { transfer_id: 0 }
        );
    }

    #[test]
    fn malformed_frames_are_refused_rather_than_guessed_at() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[KIND_CHUNK, 0, 0]).is_err());
        assert!(decode(&frame(99, 1, b"body")).is_err());
        // An end marker carrying a payload is a sender this side does not
        // understand, not an end marker with something extra.
        assert!(decode(&frame(KIND_END, 1, b"body")).is_err());
    }

    #[test]
    fn a_transfer_spanning_many_frames_installs_the_exact_bytes() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store.clone());
        let bytes: Vec<u8> = (0..(WIRE_CHUNK * 3 + 11))
            .map(|i| (i % 251) as u8)
            .collect();
        let hash = sha256_hex(&bytes);

        assert_eq!(
            send(&mut receiver, 1, &bytes),
            Delivered::Installed {
                transfer_id: 1,
                hash: hash.clone()
            }
        );
        assert_eq!(store.get(&hash).unwrap(), bytes);
        assert_eq!(receiver.open_count(), 0);
        assert_eq!(scratch.parts(), 0);
    }

    #[test]
    fn concurrent_transfers_do_not_contaminate_each_other() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store.clone());
        let a: Vec<u8> = (0..(WIRE_CHUNK + 5)).map(|i| (i % 97) as u8).collect();
        let b: Vec<u8> = (0..(WIRE_CHUNK * 2)).map(|i| (i % 131) as u8).collect();
        let (ha, hb) = (sha256_hex(&a), sha256_hex(&b));

        receiver.accept_offer(1, &ha, a.len() as u64).unwrap();
        receiver.accept_offer(2, &hb, b.len() as u64).unwrap();

        // Interleave the two transfers frame by frame.
        let mut fa = a.chunks(WIRE_CHUNK);
        let mut fb = b.chunks(WIRE_CHUNK);
        loop {
            let (na, nb) = (fa.next(), fb.next());
            if na.is_none() && nb.is_none() {
                break;
            }
            if let Some(slice) = na {
                let f = chunk_frame(1, slice);
                assert_eq!(receiver.accept(decode(&f).unwrap()), Delivered::More);
            }
            if let Some(slice) = nb {
                let f = chunk_frame(2, slice);
                assert_eq!(receiver.accept(decode(&f).unwrap()), Delivered::More);
            }
        }
        let end = end_frame(2);
        assert_eq!(
            receiver.accept(decode(&end).unwrap()),
            Delivered::Installed {
                transfer_id: 2,
                hash: hb.clone()
            }
        );
        let end = end_frame(1);
        assert_eq!(
            receiver.accept(decode(&end).unwrap()),
            Delivered::Installed {
                transfer_id: 1,
                hash: ha.clone()
            }
        );
        assert_eq!(store.get(&ha).unwrap(), a);
        assert_eq!(store.get(&hb).unwrap(), b);
    }

    #[test]
    fn content_that_does_not_match_the_announced_hash_installs_nothing() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store.clone());
        let honest = vec![1u8; WIRE_CHUNK + 3];
        let hash = sha256_hex(&honest);
        let mut tampered = honest.clone();
        tampered[WIRE_CHUNK] ^= 0xFF;

        receiver
            .accept_offer(1, &hash, tampered.len() as u64)
            .unwrap();
        for slice in tampered.chunks(WIRE_CHUNK) {
            let f = chunk_frame(1, slice);
            receiver.accept(decode(&f).unwrap());
        }
        let end = end_frame(1);
        match receiver.accept(decode(&end).unwrap()) {
            Delivered::Failed { transfer_id, .. } => assert_eq!(transfer_id, 1),
            other => panic!("tampered content was not refused: {other:?}"),
        }
        assert!(!store.has(&hash));
        assert_eq!(scratch.parts(), 0);
    }

    #[test]
    fn a_truncated_transfer_installs_nothing() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store.clone());
        let bytes = vec![4u8; WIRE_CHUNK * 2];
        let hash = sha256_hex(&bytes);

        receiver.accept_offer(1, &hash, bytes.len() as u64).unwrap();
        let f = chunk_frame(1, &bytes[..WIRE_CHUNK]);
        receiver.accept(decode(&f).unwrap());
        let end = end_frame(1);
        match receiver.accept(decode(&end).unwrap()) {
            Delivered::Failed { reason, .. } => assert!(reason.contains("never arrived")),
            other => panic!("a truncated transfer was accepted: {other:?}"),
        }
        assert!(!store.has(&hash));
        assert_eq!(scratch.parts(), 0);
    }

    #[test]
    fn a_sender_cannot_write_more_than_it_announced() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store);
        let hash = sha256_hex(b"small");
        receiver.accept_offer(1, &hash, 5).unwrap();
        let f = chunk_frame(1, &[0u8; 4096]);
        match receiver.accept(decode(&f).unwrap()) {
            Delivered::Failed { reason, .. } => assert!(reason.contains("announced size")),
            other => panic!("an overlong transfer was accepted: {other:?}"),
        }
        assert_eq!(receiver.open_count(), 0);
        assert_eq!(scratch.parts(), 0);
    }

    /// An interrupted transfer installs nothing, and keeps exactly the prefix
    /// it verified so the next attempt does not pay for it again.
    #[test]
    fn an_interrupted_connection_is_resumed_rather_than_restarted() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes: Vec<u8> = (0..(WIRE_CHUNK * 3)).map(|i| (i % 241) as u8).collect();
        let hash = sha256_hex(&bytes);
        {
            let mut receiver = BlobReceiver::new(store.clone());
            receiver.accept_offer(1, &hash, bytes.len() as u64).unwrap();
            let f = chunk_frame(1, &bytes[..WIRE_CHUNK]);
            receiver.accept(decode(&f).unwrap());
            // Dropping the receiver is what a lost connection does.
        }
        assert!(!store.has(&hash), "a partial transfer must not install");
        assert_eq!(scratch.parts(), 0, "no anonymous temporary");
        assert_eq!(scratch.partials(), vec![WIRE_CHUNK as u64]);

        // A fresh connection: new receiver, new transfer id, same content.
        let mut receiver = BlobReceiver::new(store.clone());
        let from_offset = receiver.accept_offer(7, &hash, bytes.len() as u64).unwrap();
        assert_eq!(from_offset, WIRE_CHUNK as u64, "resumed, not restarted");
        for slice in bytes[WIRE_CHUNK..].chunks(WIRE_CHUNK) {
            let f = chunk_frame(7, slice);
            assert_eq!(receiver.accept(decode(&f).unwrap()), Delivered::More);
        }
        let end = end_frame(7);
        assert_eq!(
            receiver.accept(decode(&end).unwrap()),
            Delivered::Installed {
                transfer_id: 7,
                hash: hash.clone()
            }
        );
        assert_eq!(store.get(&hash).unwrap(), bytes);
        assert!(scratch.partials().is_empty(), "the partial became the blob");
    }

    /// A resumed transfer must still be verified over its whole length: the
    /// prefix the last connection left is re-hashed, so a peer that resumes
    /// with the wrong tail installs nothing.
    #[test]
    fn a_resumed_transfer_that_ends_wrong_installs_nothing() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let bytes = vec![3u8; WIRE_CHUNK * 2];
        let hash = sha256_hex(&bytes);
        {
            let mut receiver = BlobReceiver::new(store.clone());
            receiver.accept_offer(1, &hash, bytes.len() as u64).unwrap();
            let f = chunk_frame(1, &bytes[..WIRE_CHUNK]);
            receiver.accept(decode(&f).unwrap());
        }
        let mut receiver = BlobReceiver::new(store.clone());
        assert_eq!(
            receiver.accept_offer(2, &hash, bytes.len() as u64).unwrap(),
            WIRE_CHUNK as u64
        );
        let f = chunk_frame(2, &vec![0xFFu8; WIRE_CHUNK]);
        receiver.accept(decode(&f).unwrap());
        let end = end_frame(2);
        match receiver.accept(decode(&end).unwrap()) {
            Delivered::Failed { transfer_id, .. } => assert_eq!(transfer_id, 2),
            other => panic!("a mismatched resume was accepted: {other:?}"),
        }
        assert!(!store.has(&hash));
        // The bad prefix goes too, or every later attempt would fail the same
        // way.
        assert!(scratch.partials().is_empty());
    }

    /// A partial longer than the content now on offer cannot be a prefix of
    /// it, so it is thrown away rather than resumed from.
    #[test]
    fn a_partial_longer_than_the_offer_restarts_from_zero() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let short = vec![5u8; 32];
        let hash = sha256_hex(&short);
        {
            let mut receiver = BlobReceiver::new(store.clone());
            receiver.accept_offer(1, &hash, 4096).unwrap();
            let f = chunk_frame(1, &[5u8; 4096]);
            receiver.accept(decode(&f).unwrap());
        }
        let mut receiver = BlobReceiver::new(store.clone());
        assert_eq!(
            receiver.accept_offer(2, &hash, short.len() as u64).unwrap(),
            0
        );
        assert_eq!(
            send_open(&mut receiver, 2, &short),
            Delivered::Installed {
                transfer_id: 2,
                hash: hash.clone()
            }
        );
        assert_eq!(store.get(&hash).unwrap(), short);
    }

    #[test]
    fn frames_for_an_unknown_transfer_are_reported_not_written() {
        let scratch = Scratch::new();
        let mut receiver = BlobReceiver::new(scratch.store());
        let f = chunk_frame(42, b"orphan");
        assert_eq!(
            receiver.accept(decode(&f).unwrap()),
            Delivered::Stray { transfer_id: 42 }
        );
        let end = end_frame(42);
        assert_eq!(
            receiver.accept(decode(&end).unwrap()),
            Delivered::Stray { transfer_id: 42 }
        );
        assert_eq!(scratch.parts(), 0);
    }

    #[test]
    fn an_aborted_transfer_is_reported_and_installs_nothing() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store.clone());
        let bytes = vec![2u8; 64];
        let hash = sha256_hex(&bytes);
        receiver.accept_offer(3, &hash, bytes.len() as u64).unwrap();
        let f = chunk_frame(3, &bytes[..32]);
        receiver.accept(decode(&f).unwrap());
        let abort = abort_frame(3);
        match receiver.accept(decode(&abort).unwrap()) {
            Delivered::Failed { transfer_id, .. } => assert_eq!(transfer_id, 3),
            other => panic!("an abort was not reported: {other:?}"),
        }
        assert!(!store.has(&hash));
        assert_eq!(scratch.parts(), 0);
    }

    #[test]
    fn the_number_of_open_transfers_is_bounded() {
        let scratch = Scratch::new();
        let mut receiver = BlobReceiver::new(scratch.store());
        let hash = sha256_hex(b"x");
        for id in 0..MAX_OPEN_TRANSFERS as u64 {
            receiver.accept_offer(id, &hash, 1).unwrap();
        }
        assert!(receiver.accept_offer(999, &hash, 1).is_err());
    }

    #[test]
    fn re_offering_an_open_transfer_id_is_refused() {
        let scratch = Scratch::new();
        let mut receiver = BlobReceiver::new(scratch.store());
        let hash = sha256_hex(b"x");
        receiver.accept_offer(1, &hash, 1).unwrap();
        assert!(receiver.accept_offer(1, &hash, 1).is_err());
    }

    #[test]
    fn a_malformed_hash_is_refused_before_a_writer_exists() {
        let scratch = Scratch::new();
        let mut receiver = BlobReceiver::new(scratch.store());
        assert!(receiver.accept_offer(1, "not-a-hash", 1).is_err());
        assert!(receiver.accept_offer(1, &"z".repeat(64), 1).is_err());
        assert_eq!(receiver.open_count(), 0);
        assert_eq!(scratch.parts(), 0);
    }

    #[test]
    fn an_empty_blob_transfers_as_an_end_marker_alone() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let mut receiver = BlobReceiver::new(store.clone());
        let hash = sha256_hex(b"");
        assert_eq!(
            send(&mut receiver, 1, b""),
            Delivered::Installed {
                transfer_id: 1,
                hash: hash.clone()
            }
        );
        assert_eq!(store.get(&hash).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn transfer_ids_never_repeat() {
        let mut ids = TransferIds::default();
        let first = ids.next_id();
        assert_ne!(first, 0, "zero stays available as a never-used id");
        assert_eq!(ids.next_id(), first + 1);
    }
}
