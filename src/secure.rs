// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end encryption for the Weave application protocol.
//!
//! Every remote Weave connection — Cloudflare Quick Tunnel or LAN — carries the
//! JSON protocol inside a Noise session established with
//! `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`. The pre-shared key is derived from
//! the session secret, which therefore never travels over the network in any
//! form: possession is proven by completing the handshake, not by presenting a
//! bearer token that Cloudflare terminates TLS in front of.
//!
//! Weave does not implement the Noise state machine. `snow` owns the ephemeral
//! key exchange, key derivation, AEAD, nonce counters, transcript hashing and
//! transport keys. This module is the framing and lifecycle around it.
//!
//! `snow` 0.10.0 has not received a formal, published third-party security
//! audit. It is a widely used implementation of a specified protocol, and its
//! primitives come from separately reviewed crates (`curve25519-dalek`,
//! `chacha20poly1305`, `blake2`), but "no audit" is the accurate statement and
//! is documented in `docs/SECURITY.md` rather than glossed over here.
//!
//! What is deliberately *not* attempted: hiding traffic metadata. Sizes, timing
//! and volume stay observable, and no padding or traffic shaping is added.

use crate::error::{network, protocol, Result, WeaveError};
use hkdf::Hkdf;
use sha2::Sha256;
use snow::{Builder, HandshakeState, TransportState};
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// Suite and limits
// ---------------------------------------------------------------------------

/// The Noise protocol name, exactly as it is hashed into the handshake.
///
/// `NNpsk0` is the pattern for two peers that already share a strong symmetric
/// key and have no public-key identities: the PSK authenticates both sides, the
/// ephemeral-ephemeral exchange supplies forward secrecy, and no static keys
/// have to be distributed or managed by anyone.
pub const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Version of the encrypted framing itself, mixed into the handshake prologue.
/// Two peers disagreeing on it cannot complete a handshake.
pub const TRANSPORT_VERSION: u16 = 1;

/// The Noise specification (revision 34) caps a single message at 65535 bytes.
pub const MAX_NOISE_MESSAGE: usize = 65535;
/// ChaChaPoly authentication tag.
const TAG: usize = 16;
/// One plaintext byte carries the continuation flag.
const FLAG: usize = 1;
/// Largest slice of an application message that fits in one Noise message.
pub const MAX_CHUNK: usize = MAX_NOISE_MESSAGE - TAG - FLAG;

/// Generous bound for a handshake message; both are 48 bytes in this pattern.
pub const MAX_HANDSHAKE_MESSAGE: usize = 1024;
/// A peer that cannot finish the handshake in this long is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Unauthenticated connections allowed to be mid-handshake at the same time.
pub const MAX_PENDING_HANDSHAKES: usize = 32;

/// WebSocket frame bound. One Noise message per frame, plus slack.
pub const MAX_WS_FRAME: usize = MAX_NOISE_MESSAGE + 64;

const CONTINUES: u8 = 1;
const FINAL: u8 = 0;

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------

/// HKDF salt. Changing this string invalidates every previously derived PSK,
/// which is the point of naming the version in it.
const PSK_SALT: &[u8] = b"weave-noise-psk-v1";
const PSK_INFO: &[u8] = b"weave noise pre-shared key";
const PROLOGUE_LABEL: &[u8] = b"weave-noise-v1";

/// The Noise pre-shared key for one session.
///
/// The session secret is the root of trust and is used for nothing else on the
/// wire, so it is passed through HKDF-SHA256 with an explicit salt and info
/// rather than being handed to the handshake directly. The session id is bound
/// in, so a PSK derived for one session is meaningless in another even if the
/// same secret were somehow reused.
pub fn derive_psk(session_secret: &str, session_id: Uuid) -> Zeroizing<[u8; 32]> {
    let mut info = Vec::with_capacity(PSK_INFO.len() + 18);
    info.extend_from_slice(PSK_INFO);
    info.extend_from_slice(&TRANSPORT_VERSION.to_be_bytes());
    info.extend_from_slice(session_id.as_bytes());

    let hkdf = Hkdf::<Sha256>::new(Some(PSK_SALT), session_secret.as_bytes());
    let mut psk = Zeroizing::new([0u8; 32]);
    hkdf.expand(&info, psk.as_mut())
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    psk
}

/// Handshake prologue: context both peers must already agree on.
///
/// Hashed into the transcript, so a mismatch on suite, framing version or
/// session id fails the handshake instead of producing a subtly wrong session.
fn prologue(session_id: Uuid) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(PROLOGUE_LABEL);
    out.push(b'|');
    out.extend_from_slice(NOISE_PATTERN.as_bytes());
    out.push(b'|');
    out.extend_from_slice(&TRANSPORT_VERSION.to_be_bytes());
    out.extend_from_slice(session_id.as_bytes());
    out
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Handshake and AEAD failures never quote the underlying cryptographic error
/// or any buffer, so nothing derived from key material can reach a log.
fn rejected() -> WeaveError {
    network("The Weave host did not accept the encrypted handshake.").with_detail(
        "The session secret in this invite does not match the host's session, the host is \
         running an incompatible Weave version, or the connection was tampered with. Ask the \
         host for a fresh invite.",
    )
}

fn handshake_failed() -> WeaveError {
    network("The Weave encrypted handshake failed.")
}

fn frame_rejected() -> WeaveError {
    protocol("An encrypted Weave frame failed authentication.").with_detail(
        "The frame was altered, truncated or replayed in transit. Weave dropped the \
         connection rather than acting on unauthenticated data.",
    )
}

fn misconfigured(what: &str) -> WeaveError {
    protocol(format!(
        "Could not initialise the Weave secure transport: {what}"
    ))
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// `Builder` borrows the prologue and the PSK, so each constructor below owns
/// them locally and consumes the builder before they go out of scope.
fn new_builder<'a>(psk: &'a [u8; 32], prologue: &'a [u8]) -> Result<Builder<'a>> {
    let params = NOISE_PATTERN
        .parse()
        .map_err(|_| misconfigured("unsupported Noise pattern"))?;
    Builder::new(params)
        .prologue(prologue)
        .map_err(|_| misconfigured("prologue rejected"))?
        .psk(0, psk)
        .map_err(|_| misconfigured("pre-shared key rejected"))
}

/// The joining side of a Weave connection.
pub struct Initiator {
    state: HandshakeState,
}

impl Initiator {
    pub fn new(psk: &[u8; 32], session_id: Uuid) -> Result<Initiator> {
        let prologue = prologue(session_id);
        let state = new_builder(psk, &prologue)?
            .build_initiator()
            .map_err(|_| misconfigured("initiator state"))?;
        Ok(Initiator { state })
    }

    /// `-> psk, e`
    pub fn first_message(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; MAX_HANDSHAKE_MESSAGE];
        let n = self
            .state
            .write_message(&[], &mut buf)
            .map_err(|_| handshake_failed())?;
        buf.truncate(n);
        Ok(buf)
    }

    /// `<- e, ee`, then transport mode.
    pub fn finish(mut self, response: &[u8]) -> Result<SecureChannel> {
        if response.is_empty() || response.len() > MAX_HANDSHAKE_MESSAGE {
            return Err(rejected());
        }
        let mut buf = vec![0u8; MAX_HANDSHAKE_MESSAGE];
        self.state
            .read_message(response, &mut buf)
            .map_err(|_| rejected())?;
        if !self.state.is_handshake_finished() {
            return Err(rejected());
        }
        let transport = self
            .state
            .into_transport_mode()
            .map_err(|_| handshake_failed())?;
        Ok(SecureChannel::new(transport))
    }
}

/// The hosting side of a Weave connection.
pub struct Responder {
    state: HandshakeState,
}

impl Responder {
    pub fn new(psk: &[u8; 32], session_id: Uuid) -> Result<Responder> {
        let prologue = prologue(session_id);
        let state = new_builder(psk, &prologue)?
            .build_responder()
            .map_err(|_| misconfigured("responder state"))?;
        Ok(Responder { state })
    }

    /// Consume the initiator's message and produce the reply plus the channel.
    ///
    /// A peer that does not hold the session secret cannot get past this call,
    /// which is why the caller must not disclose anything about the session
    /// before it returns.
    pub fn respond(mut self, first: &[u8]) -> Result<(Vec<u8>, SecureChannel)> {
        if first.is_empty() || first.len() > MAX_HANDSHAKE_MESSAGE {
            return Err(handshake_failed());
        }
        let mut scratch = vec![0u8; MAX_HANDSHAKE_MESSAGE];
        self.state
            .read_message(first, &mut scratch)
            .map_err(|_| handshake_failed())?;

        let mut reply = vec![0u8; MAX_HANDSHAKE_MESSAGE];
        let n = self
            .state
            .write_message(&[], &mut reply)
            .map_err(|_| handshake_failed())?;
        reply.truncate(n);

        if !self.state.is_handshake_finished() {
            return Err(handshake_failed());
        }
        let transport = self
            .state
            .into_transport_mode()
            .map_err(|_| handshake_failed())?;
        Ok((reply, SecureChannel::new(transport)))
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

struct Inner {
    state: TransportState,
    /// Chunks of a partially received application message.
    pending: Vec<u8>,
    /// Chunks accumulated into `pending`. Bounded alongside the byte count so a
    /// peer cannot stream continuation frames forever without ever finishing a
    /// message: the byte bound alone does not stop an endless run of chunks
    /// that carry no payload.
    chunks: usize,
}

/// An established Noise session.
///
/// Deliberately implements neither `Debug` nor `Clone`: it holds live transport
/// keys, so any attempt to format it into a log is a compile error.
///
/// Shared by the socket's reader and writer tasks, so both directions go
/// through one lock. Noise keeps an independent nonce counter per direction;
/// the lock only serialises access to the state object, it does not couple the
/// two streams.
pub struct SecureChannel {
    inner: Mutex<Inner>,
    /// Largest application message this channel will reassemble.
    max_message: usize,
}

impl SecureChannel {
    fn new(state: TransportState) -> SecureChannel {
        SecureChannel {
            inner: Mutex::new(Inner {
                state,
                pending: Vec::new(),
                chunks: 0,
            }),
            max_message: crate::model::MAX_PROTOCOL_MESSAGE,
        }
    }

    /// Most chunks a single application message may be split into.
    ///
    /// Exactly what a maximum-size message needs, plus one for the terminating
    /// chunk when the payload divides evenly. Anything beyond that is a peer
    /// sending chunks that cannot belong to a legal message.
    fn max_chunks(&self) -> usize {
        self.max_message.div_ceil(MAX_CHUNK) + 1
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        // A poisoned lock means a panic happened mid-update, so the nonce
        // counters can no longer be trusted. Failing the connection forces a
        // fresh handshake rather than continuing on suspect state.
        self.inner
            .lock()
            .map_err(|_| protocol("The Weave secure transport entered an unusable state."))
    }

    /// Encrypt one application message into one or more Noise messages.
    ///
    /// Messages larger than a Noise message are split; the continuation flag
    /// travels *inside* the ciphertext, so an intermediary cannot forge, drop
    /// or reorder a chunk — Noise's per-direction counter makes any such edit
    /// fail authentication on the next frame.
    ///
    /// The lock is held for the whole message rather than per chunk, and that is
    /// a correctness requirement, not convenience. Two concurrent calls that
    /// each took the lock per chunk would interleave validly encrypted chunks
    /// from different messages; the peer would authenticate every frame and
    /// reassemble the mixture into one corrupt message.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<Vec<u8>>> {
        if plaintext.len() > self.max_message {
            return Err(protocol("Weave message is too large to send."));
        }
        let mut guard = self.lock()?;
        let mut frames = Vec::with_capacity(plaintext.len() / MAX_CHUNK + 1);
        let mut offset = 0usize;
        loop {
            let end = (offset + MAX_CHUNK).min(plaintext.len());
            let chunk = &plaintext[offset..end];
            let more = end < plaintext.len();

            let mut framed = Vec::with_capacity(chunk.len() + FLAG);
            framed.push(if more { CONTINUES } else { FINAL });
            framed.extend_from_slice(chunk);

            let mut out = vec![0u8; framed.len() + TAG];
            let n = guard
                .state
                .write_message(&framed, &mut out)
                .map_err(|_| protocol("Could not encrypt a Weave message."))?;
            out.truncate(n);
            frames.push(out);

            offset = end;
            if !more {
                break;
            }
        }
        Ok(frames)
    }

    /// Decrypt one Noise message, returning a complete application message when
    /// the last chunk of one arrives.
    ///
    /// Any failure here is terminal for the connection: the caller must drop it
    /// rather than resynchronise, because a rejected frame has already consumed
    /// a nonce.
    pub fn decrypt(&self, frame: &[u8]) -> Result<Option<Vec<u8>>> {
        if frame.is_empty() || frame.len() > MAX_NOISE_MESSAGE {
            return Err(frame_rejected());
        }
        let mut guard = self.lock()?;
        let mut out = vec![0u8; frame.len()];
        let n = guard
            .state
            .read_message(frame, &mut out)
            .map_err(|_| frame_rejected())?;
        if n < FLAG {
            return Err(frame_rejected());
        }
        let flag = out[0];
        let body = &out[FLAG..n];

        // An authenticated peer is still only a peer holding the session
        // secret, so reassembly is bounded on both axes before anything is
        // appended. Bytes: a message can never grow past the protocol limit.
        // Chunks: without this, a peer could stream empty continuation frames
        // indefinitely — each one authentic, none of them growing `pending`,
        // the message never completing.
        let max_chunks = self.max_chunks();
        let guard = &mut *guard;
        if guard.pending.len() + body.len() > self.max_message || guard.chunks >= max_chunks {
            guard.reset_reassembly();
            return Err(protocol("Weave message exceeded the protocol size limit."));
        }
        // Reserve exactly, so the buffer's capacity is bounded by the protocol
        // limit too. `extend_from_slice` alone grows geometrically and would let
        // a maximum-size message transiently hold roughly twice that in memory.
        guard.pending.reserve_exact(body.len());
        guard.pending.extend_from_slice(body);
        guard.chunks += 1;

        match flag {
            FINAL => {
                guard.chunks = 0;
                Ok(Some(std::mem::take(&mut guard.pending)))
            }
            CONTINUES => Ok(None),
            _ => {
                guard.reset_reassembly();
                Err(frame_rejected())
            }
        }
    }

    /// Shrink the reassembly limit. Test-only: the real limit is the protocol
    /// message size, and exercising it directly would mean moving tens of
    /// megabytes through the AEAD in a unit test.
    #[cfg(test)]
    fn limit_to(&mut self, max_message: usize) {
        self.max_message = max_message;
    }

    /// Encrypt one chunk with a caller-chosen continuation flag, bypassing the
    /// chunking loop. Test-only: it is the only way to produce the frames a
    /// hostile peer would send — bad flag bytes, endless continuations — which
    /// `encrypt` will never emit.
    #[cfg(test)]
    fn encrypt_raw(&self, flag: u8, body: &[u8]) -> Result<Vec<u8>> {
        let mut guard = self.lock()?;
        let mut framed = Vec::with_capacity(body.len() + FLAG);
        framed.push(flag);
        framed.extend_from_slice(body);
        let mut out = vec![0u8; framed.len() + TAG];
        let n = guard
            .state
            .write_message(&framed, &mut out)
            .map_err(|_| protocol("Could not encrypt a Weave message."))?;
        out.truncate(n);
        Ok(out)
    }
}

impl Inner {
    /// Drop a partial message and its accounting together, so the two can never
    /// disagree about how much has been accumulated.
    fn reset_reassembly(&mut self) {
        self.pending = Vec::new();
        self.chunks = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(secret: &str, session: Uuid) -> (SecureChannel, SecureChannel) {
        connect(secret, session, secret, session).expect("handshake")
    }

    fn connect(
        initiator_secret: &str,
        initiator_session: Uuid,
        responder_secret: &str,
        responder_session: Uuid,
    ) -> Result<(SecureChannel, SecureChannel)> {
        let ipsk = derive_psk(initiator_secret, initiator_session);
        let rpsk = derive_psk(responder_secret, responder_session);
        let mut initiator = Initiator::new(&ipsk, initiator_session)?;
        let responder = Responder::new(&rpsk, responder_session)?;
        let first = initiator.first_message()?;
        let (reply, host) = responder.respond(&first)?;
        let client = initiator.finish(&reply)?;
        Ok((client, host))
    }

    #[test]
    fn matching_secrets_establish_a_channel_and_round_trip() {
        let session = Uuid::new_v4();
        let (client, host) = pair("a-session-secret", session);
        let frames = client.encrypt(b"{\"hello\":true}").unwrap();
        assert_eq!(frames.len(), 1);
        let got = host.decrypt(&frames[0]).unwrap().unwrap();
        assert_eq!(got, b"{\"hello\":true}");
    }

    #[test]
    fn the_secret_never_appears_in_the_handshake_or_the_ciphertext() {
        let session = Uuid::new_v4();
        let secret = "SENTINEL_SESSION_SECRET_VALUE_0123456789";
        let psk = derive_psk(secret, session);
        let mut initiator = Initiator::new(&psk, session).unwrap();
        let responder = Responder::new(&psk, session).unwrap();
        let first = initiator.first_message().unwrap();
        let (reply, host) = responder.respond(&first).unwrap();
        let client = initiator.finish(&reply).unwrap();

        let frames = client.encrypt(b"SENTINEL_PAYLOAD_CONTENT").unwrap();
        let mut wire = Vec::new();
        wire.extend_from_slice(&first);
        wire.extend_from_slice(&reply);
        for f in &frames {
            wire.extend_from_slice(f);
        }
        assert!(!contains(&wire, secret.as_bytes()));
        assert!(!contains(&wire, psk.as_ref()));
        assert!(!contains(&wire, b"SENTINEL_PAYLOAD_CONTENT"));
        // ... and the same bytes really do decrypt, so the scan is not vacuous.
        assert_eq!(
            host.decrypt(&frames[0]).unwrap().unwrap(),
            b"SENTINEL_PAYLOAD_CONTENT"
        );
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn a_wrong_secret_cannot_complete_the_handshake() {
        let session = Uuid::new_v4();
        match connect("right-secret", session, "wrong-secret", session) {
            Ok(_) => panic!("a wrong secret established a channel"),
            Err(e) => assert_eq!(e.class, crate::error::ErrorClass::NetworkError),
        }
    }

    #[test]
    fn a_secret_for_another_session_cannot_complete_the_handshake() {
        // Same secret string, different session id: the derived PSKs differ.
        assert!(connect("shared", Uuid::new_v4(), "shared", Uuid::new_v4()).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let mut frames = client.encrypt(b"the quick brown fox").unwrap();
        let last = frames[0].len() - 1;
        frames[0][last] ^= 0x01;
        assert!(host.decrypt(&frames[0]).is_err());

        // And flipping a byte of the ciphertext body, not just the tag.
        let (client, host) = pair("secret", session);
        let mut frames = client.encrypt(b"the quick brown fox").unwrap();
        frames[0][2] ^= 0x80;
        assert!(host.decrypt(&frames[0]).is_err());
    }

    #[test]
    fn truncated_and_malformed_frames_fail_safely() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let frames = client.encrypt(b"a payload worth truncating").unwrap();
        let short = &frames[0][..frames[0].len() - 4];
        assert!(host.decrypt(short).is_err());
        assert!(host.decrypt(&[]).is_err());
        assert!(host.decrypt(&[0u8; 3]).is_err());
        assert!(host.decrypt(&vec![0u8; MAX_NOISE_MESSAGE + 1]).is_err());
    }

    #[test]
    fn traffic_captured_from_one_connection_is_invalid_on_a_fresh_one() {
        let session = Uuid::new_v4();
        let (client, _host) = pair("secret", session);
        let captured = client.encrypt(b"replay me").unwrap();

        // A brand new connection with the same secret: fresh ephemeral keys
        // mean fresh transport keys, so the captured frame is meaningless.
        let (_client2, host2) = pair("secret", session);
        assert!(host2.decrypt(&captured[0]).is_err());
    }

    #[test]
    fn a_frame_cannot_be_replayed_within_its_own_connection() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let frames = client.encrypt(b"once").unwrap();
        assert!(host.decrypt(&frames[0]).unwrap().is_some());
        // The receiving nonce has advanced; the same bytes no longer verify.
        assert!(host.decrypt(&frames[0]).is_err());
    }

    #[test]
    fn frames_cannot_be_reordered() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        // The first frame is deliberately withheld, not delivered out of order.
        let _first = client.encrypt(b"first").unwrap();
        let second = client.encrypt(b"second").unwrap();
        assert!(host.decrypt(&second[0]).is_err());
    }

    #[test]
    fn a_message_larger_than_one_noise_message_is_chunked_and_reassembled() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let payload: Vec<u8> = (0..(MAX_CHUNK * 3 + 17)).map(|i| (i % 251) as u8).collect();
        let frames = client.encrypt(&payload).unwrap();
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|f| f.len() <= MAX_NOISE_MESSAGE));

        for frame in &frames[..3] {
            assert!(host.decrypt(frame).unwrap().is_none());
        }
        assert_eq!(host.decrypt(&frames[3]).unwrap().unwrap(), payload);
    }

    #[test]
    fn dropping_a_chunk_of_a_split_message_fails_rather_than_truncating_it() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let payload = vec![7u8; MAX_CHUNK * 2];
        let frames = client.encrypt(&payload).unwrap();
        assert_eq!(frames.len(), 2);
        // Deliver only the final chunk: the counter no longer matches.
        assert!(host.decrypt(&frames[1]).is_err());
    }

    #[test]
    fn reassembly_stops_at_the_size_limit_instead_of_growing() {
        let session = Uuid::new_v4();
        let (client, mut host) = pair("secret", session);
        // The sender keeps the real limit so it will happily produce a message
        // the receiver has decided is too large.
        host.limit_to(MAX_CHUNK * 2);
        let payload = vec![3u8; MAX_CHUNK * 3];
        let frames = client.encrypt(&payload).unwrap();

        assert!(host.decrypt(&frames[0]).unwrap().is_none());
        assert!(host.decrypt(&frames[1]).unwrap().is_none());
        {
            let guard = host.inner.lock().unwrap();
            assert_eq!(guard.pending.len(), MAX_CHUNK * 2);
            // Reserved exactly, so the buffer never overshoots the limit it is
            // being held to.
            assert!(guard.pending.capacity() <= MAX_CHUNK * 2);
        }

        assert!(host.decrypt(&frames[2]).is_err());
        let guard = host.inner.lock().unwrap();
        assert!(guard.pending.is_empty());
        assert_eq!(guard.chunks, 0);
    }

    #[test]
    fn an_endless_run_of_continuation_chunks_is_refused() {
        let session = Uuid::new_v4();
        let (client, mut host) = pair("secret", session);
        host.limit_to(MAX_CHUNK * 2);
        let limit = host.max_chunks();

        // Empty continuation chunks never grow `pending`, so only the chunk
        // bound can stop them.
        for _ in 0..limit {
            let frame = client.encrypt_raw(CONTINUES, b"").unwrap();
            assert!(host.decrypt(&frame).unwrap().is_none());
        }
        let frame = client.encrypt_raw(CONTINUES, b"").unwrap();
        assert!(host.decrypt(&frame).is_err());
        assert_eq!(host.inner.lock().unwrap().chunks, 0);
    }

    #[test]
    fn the_chunk_bound_still_admits_a_maximum_size_message() {
        let session = Uuid::new_v4();
        let (_client, host) = pair("secret", session);
        let needed = crate::model::MAX_PROTOCOL_MESSAGE.div_ceil(MAX_CHUNK);
        assert!(needed <= host.max_chunks());
    }

    #[test]
    fn an_unknown_continuation_flag_is_refused() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        // Authentic, correctly framed, and still not something `encrypt` can
        // ever produce.
        let frame = client.encrypt_raw(2, b"body").unwrap();
        assert!(host.decrypt(&frame).is_err());
        let guard = host.inner.lock().unwrap();
        assert!(guard.pending.is_empty());
        assert_eq!(guard.chunks, 0);
    }

    #[test]
    fn a_completed_message_resets_the_chunk_count() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        for _ in 0..(host.max_chunks() * 3) {
            let frames = client.encrypt(b"a whole small message").unwrap();
            assert_eq!(frames.len(), 1);
            assert!(host.decrypt(&frames[0]).unwrap().is_some());
        }
        assert_eq!(host.inner.lock().unwrap().chunks, 0);
    }

    #[test]
    fn an_empty_message_round_trips() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let frames = client.encrypt(b"").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(host.decrypt(&frames[0]).unwrap().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn both_directions_work_independently() {
        let session = Uuid::new_v4();
        let (client, host) = pair("secret", session);
        let up = client.encrypt(b"client to host").unwrap();
        let down = host.encrypt(b"host to client").unwrap();
        assert_eq!(host.decrypt(&up[0]).unwrap().unwrap(), b"client to host");
        assert_eq!(
            client.decrypt(&down[0]).unwrap().unwrap(),
            b"host to client"
        );
    }

    #[test]
    fn psk_derivation_is_deterministic_and_domain_separated() {
        let session = Uuid::new_v4();
        let a = derive_psk("secret", session);
        let b = derive_psk("secret", session);
        assert_eq!(a.as_ref(), b.as_ref());
        assert_ne!(a.as_ref(), derive_psk("secret", Uuid::new_v4()).as_ref());
        assert_ne!(a.as_ref(), derive_psk("secret2", session).as_ref());
        // The PSK is not the secret, nor a plain hash the secret could be read from.
        assert_ne!(a.as_ref() as &[u8], b"secret" as &[u8]);
    }

    #[test]
    fn handshake_messages_stay_within_the_bound() {
        let session = Uuid::new_v4();
        let psk = derive_psk("secret", session);
        let mut initiator = Initiator::new(&psk, session).unwrap();
        let first = initiator.first_message().unwrap();
        let (reply, _) = Responder::new(&psk, session)
            .unwrap()
            .respond(&first)
            .unwrap();
        assert!(first.len() <= MAX_HANDSHAKE_MESSAGE);
        assert!(reply.len() <= MAX_HANDSHAKE_MESSAGE);
    }

    #[test]
    fn garbage_handshake_input_is_rejected_without_panicking() {
        let session = Uuid::new_v4();
        let psk = derive_psk("secret", session);
        for junk in [
            vec![],
            vec![0u8; 1],
            vec![0u8; 48],
            vec![0xff; MAX_HANDSHAKE_MESSAGE],
            vec![0u8; MAX_HANDSHAKE_MESSAGE + 1],
        ] {
            let responder = Responder::new(&psk, session).unwrap();
            assert!(responder.respond(&junk).is_err());
        }
    }
}
