// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Daemon assembly: `weave host`, `weave join`, `weave resume`.
//!
//! The engines are synchronous state machines on dedicated threads; this module
//! owns the async side (WebSocket server and client, local IPC, tunnel process,
//! watcher plumbing) and connects the two.

use crate::blobs::BlobStore;
use crate::client::{ClientEngine, ClientHandle, ClientInput, IpcCall};
use crate::error::{repository, session as session_err, unsupported, Result};
use crate::gitx;
use crate::host::{HostEngine, HostHandle, HostInput};
use crate::ipc::{IpcCommand, IpcRequest, IpcResponse};
use crate::model::*;
use crate::proto::{ClientEnvelope, HostEnvelope, SessionInfo};
use crate::secure::{
    FrameClass, Initiator, Responder, SecureChannel, HANDSHAKE_TIMEOUT, MAX_PENDING_HANDSHAKES,
};
use crate::session::*;
use crate::store_client::ClientStore;
use crate::store_host::HostStore;
use crate::transport::{
    blob_pump, default_outbound, secret_matches, DataFrame, Frame, Outbound, OutboundRx, PumpJob,
    MAX_FRAME, MAX_INFLIGHT_DATA_FRAMES, WS_PATH,
};
use crate::watch;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;
use zeroize::Zeroizing;

/// The session secret in the only form that ever leaves this module: a derived
/// Noise pre-shared key. Cloned by reference so the key material exists once.
#[derive(Clone)]
struct PeerKey {
    psk: Arc<Zeroizing<[u8; 32]>>,
    session_id: Uuid,
}

impl PeerKey {
    fn derive(secret: &str, session_id: Uuid) -> PeerKey {
        PeerKey {
            psk: Arc::new(crate::secure::derive_psk(secret, session_id)),
            session_id,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HostOptions {
    pub lan: bool,
    pub local_only: bool,
}

#[derive(Debug, Clone)]
pub struct JoinOptions {
    pub invite: String,
    /// Set when the caller already ran the join preflight. The CLI does, before
    /// prompting: refusing a broken repository is worth doing *before* asking
    /// somebody to paste a session secret.
    pub preflighted: bool,
}

/// Everything the IPC server needs to serve daemon-level commands.
struct DaemonControl {
    paths: Paths,
    role: Role,
    ws_port: u16,
    shutdown: mpsc::Sender<ShutdownKind>,
    client: ClientHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownKind {
    Stop,
    Leave,
}

// ---------------------------------------------------------------------------
// weave host
// ---------------------------------------------------------------------------

pub fn run_host(start_dir: &Path, opts: HostOptions) -> Result<()> {
    // The preflight `weave doctor` would run, minus everything that is merely
    // informative. Nobody should have to remember to run `weave doctor` before
    // their first session, and a successful start prints nothing extra.
    crate::doctor::ensure_ready(
        start_dir,
        if opts.local_only || opts.lan {
            crate::doctor::Intent::HostLocal
        } else {
            crate::doctor::Intent::HostRemote
        },
    )?;

    let paths = Paths::discover(start_dir)?;
    paths.ensure()?;
    let existing = load_session_record(&paths)?;
    let resuming = existing
        .as_ref()
        .map(|r| r.role == Role::Host)
        .unwrap_or(false);

    if !resuming {
        verify_new_host_repository(&paths)?;
    } else {
        verify_supported_repository(&paths)?;
    }

    let lock = DaemonLock::acquire(&paths)?;
    let identity = load_or_create_identity()?;
    let git_id = git_identity(&paths.repo_root)?;
    let display_name = identity
        .display_name
        .clone()
        .unwrap_or_else(|| git_id.name.clone());

    let branch = gitx::current_branch(&paths.repo_root)?.ok_or_else(|| {
        repository("Weave needs a checked-out branch (HEAD is detached).")
            .with_detail("Run `git switch <branch>` and retry.")
    })?;
    let head = gitx::head_oid(&paths.repo_root)?.ok_or_else(|| {
        repository("This repository has no commits yet.").with_detail(
            "Weave publishes on top of an existing commit. Make an initial commit and retry.",
        )
    })?;

    let mut host_store = HostStore::open(&paths.host_db())?;
    let blobs = BlobStore::open(paths.blobs())?;

    let (session, secret) = match (&existing, resuming) {
        (Some(record), true) => {
            let session = host_store
                .session()?
                .unwrap_or_else(|| record.session.clone());
            (session, record.secret.clone())
        }
        _ => {
            // A fresh session: r0 is the host working tree at creation. Any
            // canonical state left by a previous, ended session is discarded so
            // revision numbering starts from zero.
            host_store.reset()?;
            // The scan streams every file into the blob store as it hashes it,
            // so the base manifest is durable content-addressed state without
            // the repository ever being held in memory.
            let scan = crate::scan::scan_repository(
                &paths.repo_root,
                &BTreeMap::new(),
                &blobs,
                &mut crate::scan::ScanCache::new(),
            )?;
            report_rejected(&scan.rejected);
            host_store.install_base_manifest(&scan.entries)?;
            let session = SessionInfo {
                session_id: Uuid::new_v4(),
                repo_name: paths.repo_name(),
                branch: branch.clone(),
                base_commit: head.clone(),
                host_actor_id: identity.actor_id,
                host_display_name: display_name.clone(),
                created_at_ms: crate::util::now_ms(),
            };
            host_store.set_session(&session)?;
            (session, new_session_secret())
        }
    };

    // The publication that local Git state should currently reflect.
    let expected_head = host_store
        .latest_publication()?
        .map(|p| p.descriptor.commit_oid)
        .unwrap_or_else(|| session.base_commit.clone());
    if head != expected_head {
        return Err(
            repository("Git state changed outside Weave.").with_detail(format!(
            "Expected Git commit:\n{}\n\nCurrent Git commit:\n{}\n\nRestore the expected state \
             before resuming the session.",
            crate::util::short_oid(&expected_head),
            crate::util::short_oid(&head)
        )),
        );
    }

    let remote_name = gitx::upstream_of(&paths.repo_root, &branch)?
        .and_then(|upstream| upstream.split('/').next().map(|s| s.to_string()));

    let host_engine = HostEngine::new(
        paths.clone(),
        host_store,
        BlobStore::open(paths.blobs())?,
        session.clone(),
        expected_head.clone(),
        branch.clone(),
        git_id.name.clone(),
        git_id.email.clone(),
        remote_name,
    );
    let (host_handle, _host_thread) = host_engine.spawn();

    let mut client_store = ClientStore::open(&paths.client_db())?;
    if client_store
        .session()?
        .map(|s| s.session_id != session.session_id)
        .unwrap_or(false)
    {
        client_store.reset()?;
    }
    client_store.set_actor_id(&identity.actor_id)?;
    client_store.set_role(Role::Host)?;
    client_store.set_session(&session)?;
    let first_run = !client_store.has_manifest()?;

    let mut client_engine = ClientEngine::new(
        paths.clone(),
        client_store,
        blobs,
        identity.actor_id,
        display_name.clone(),
        git_id.name.clone(),
        git_id.email.clone(),
        Role::Host,
        session.clone(),
        branch.clone(),
        expected_head,
    );
    if first_run {
        client_engine.seed_materialized_from_disk()?;
    }
    let repaired = client_engine.repair_publications()?;
    for line in repaired {
        println!("Recovered: {line}");
    }
    let (client_handle, _client_thread) = client_engine.spawn();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| session_err(format!("Could not start the Weave runtime: {e}")))?;

    let result = runtime.block_on(host_async(
        paths.clone(),
        session,
        secret,
        opts,
        host_handle,
        client_handle,
        display_name,
        BlobStore::open(paths.blobs())?,
    ));

    drop(lock);
    let _ = clear_runtime(&paths);
    result
}

#[allow(clippy::too_many_arguments)]
async fn host_async(
    paths: Paths,
    session: SessionInfo,
    secret: SessionSecret,
    opts: HostOptions,
    host: HostHandle,
    client: ClientHandle,
    display_name: String,
    blobs: BlobStore,
) -> Result<()> {
    // The coordinator binds to loopback, or to all interfaces in LAN mode.
    let bind = if opts.lan { "0.0.0.0:0" } else { "127.0.0.1:0" };
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| session_err(format!("Could not bind the Weave coordinator: {e}")))?;
    let ws_port = listener
        .local_addr()
        .map_err(|e| session_err(format!("Could not read the coordinator address: {e}")))?
        .port();

    let ws_state = WsState {
        key: PeerKey::derive(&secret, session.session_id),
        host: host.clone(),
        next_conn: Arc::new(AtomicU64::new(1)),
        handshakes: Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES)),
        blobs,
    };
    let app = Router::new()
        .route(WS_PATH, get(ws_handler))
        .route("/", get(|| async { "weave" }))
        .with_state(ws_state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("coordinator server stopped: {e}");
        }
    });

    // The host participates through an in-process pair carrying identical
    // frames (specification section 5).
    spawn_loopback(host.clone(), client.clone());

    let (mode, endpoint) = if opts.local_only {
        (TransportMode::Local, None)
    } else if opts.lan {
        let addr = format!("{}:{ws_port}", local_ip());
        (TransportMode::Lan, Some(format!("ws://{addr}{WS_PATH}")))
    } else {
        let tunnel = crate::tunnel::start(ws_port).await?;
        let url = tunnel.websocket_url();
        TUNNEL.set(tunnel).await;
        (TransportMode::Tunnel, Some(url))
    };

    let record = SessionRecord {
        role: Role::Host,
        session: session.clone(),
        secret: secret.clone(),
        endpoint: endpoint.clone(),
        mode,
        created_at_ms: crate::util::now_ms(),
    };
    save_session_record(&paths, &record)?;

    print_host_banner(&paths, &session, &record, ws_port, &display_name)?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<ShutdownKind>(4);
    let control = Arc::new(DaemonControl {
        paths: paths.clone(),
        role: Role::Host,
        ws_port,
        shutdown: shutdown_tx,
        client: client.clone(),
    });
    start_ipc(control.clone(), Role::Host, session.session_id).await?;
    start_watcher(&paths, client.clone())?;

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nStopping Weave session.");
        }
        kind = shutdown_rx.recv() => {
            match kind {
                Some(ShutdownKind::Leave) => {
                    clear_session_record(&paths)?;
                    println!("Weave session ended.");
                }
                _ => println!("Weave session stopped."),
            }
        }
    }

    host.send(HostInput::Shutdown);
    client.send(ClientInput::Shutdown);
    TUNNEL.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// weave join
// ---------------------------------------------------------------------------

pub fn run_join(start_dir: &Path, opts: JoinOptions) -> Result<()> {
    // `weave join` connects to somebody else's tunnel; it never launches
    // cloudflared, so the preflight does not ask for one.
    if !opts.preflighted {
        crate::doctor::ensure_ready(start_dir, crate::doctor::Intent::Join)?;
    }

    let payload = decode_invite(&opts.invite)?;
    let paths = Paths::discover(start_dir)?;
    paths.ensure()?;
    verify_supported_repository(&paths)?;

    let branch = gitx::current_branch(&paths.repo_root)?.ok_or_else(|| {
        repository("Weave needs a checked-out branch (HEAD is detached).")
            .with_detail("Run `git switch <branch>` and retry.")
    })?;
    if branch != payload.branch {
        return Err(repository("Cannot join Weave session.").with_detail(format!(
            "Session branch:\n{}\n\nYour branch:\n{branch}\n\nCheck out the session branch and retry.",
            payload.branch
        )));
    }
    let head = gitx::head_oid(&paths.repo_root)?.unwrap_or_default();

    let existing = load_session_record(&paths)?;
    let rejoining = existing
        .as_ref()
        .map(|r| r.session.session_id == payload.session_id)
        .unwrap_or(false);

    if !rejoining {
        verify_clean_working_tree(&paths)?;
        if head != payload.base_commit {
            return Err(
                repository("Cannot join Weave session.").with_detail(format!(
                "Session base:\n{}\n\nYour current Git commit:\n{}\n\nCheckout the expected base \
                 commit and retry.",
                crate::util::short_oid(&payload.base_commit),
                crate::util::short_oid(&head)
            )),
            );
        }
    }

    let lock = DaemonLock::acquire(&paths)?;
    let identity = load_or_create_identity()?;
    let git_id = git_identity(&paths.repo_root)?;
    let display_name = identity
        .display_name
        .clone()
        .unwrap_or_else(|| git_id.name.clone());

    let session = SessionInfo {
        session_id: payload.session_id,
        repo_name: payload.repo_name.clone(),
        branch: payload.branch.clone(),
        base_commit: payload.base_commit.clone(),
        host_actor_id: Uuid::nil(),
        host_display_name: "host".into(),
        created_at_ms: crate::util::now_ms(),
    };

    let mut client_store = ClientStore::open(&paths.client_db())?;
    // Joining a different session must not inherit the previous session's
    // replica, outbox or publication journal.
    if client_store
        .session()?
        .map(|s| s.session_id != payload.session_id)
        .unwrap_or(false)
    {
        client_store.reset()?;
    }
    client_store.set_actor_id(&identity.actor_id)?;
    client_store.set_role(Role::Participant)?;
    let session = client_store
        .session()?
        .filter(|s| s.session_id == payload.session_id)
        .unwrap_or(session);
    client_store.set_session(&session)?;
    let first_run = !client_store.has_manifest()?;
    let blobs = BlobStore::open(paths.blobs())?;

    let expected_head = client_store
        .latest_journal_publication()?
        .map(|p| p.descriptor.commit_oid)
        .unwrap_or_else(|| payload.base_commit.clone());

    let mut client_engine = ClientEngine::new(
        paths.clone(),
        client_store,
        blobs,
        identity.actor_id,
        display_name.clone(),
        git_id.name.clone(),
        git_id.email.clone(),
        Role::Participant,
        session.clone(),
        branch.clone(),
        expected_head,
    );
    if first_run {
        client_engine.seed_materialized_from_disk()?;
    }
    let repaired = client_engine.repair_publications()?;
    for line in repaired {
        println!("Recovered: {line}");
    }
    let (client_handle, _thread) = client_engine.spawn();

    let record = SessionRecord {
        role: Role::Participant,
        session: session.clone(),
        secret: payload.secret.clone(),
        endpoint: Some(payload.url.clone()),
        mode: if payload.url.starts_with("wss://") {
            TransportMode::Tunnel
        } else {
            TransportMode::Lan
        },
        created_at_ms: crate::util::now_ms(),
    };
    save_session_record(&paths, &record)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| session_err(format!("Could not start the Weave runtime: {e}")))?;
    let result = runtime.block_on(participant_async(
        paths.clone(),
        session,
        payload.url,
        payload.secret,
        client_handle,
        display_name,
    ));

    drop(lock);
    let _ = clear_runtime(&paths);
    result
}

async fn participant_async(
    paths: Paths,
    session: SessionInfo,
    url: String,
    secret: SessionSecret,
    client: ClientHandle,
    display_name: String,
) -> Result<()> {
    println!("Weave — {}", paths.repo_name());
    println!();
    println!("Role: participant");
    println!("You:  {display_name}");
    println!("Branch: {}", session.branch);
    println!();
    println!("Connecting to the Weave host...");
    println!();
    println!("Run `weave status` from another terminal at any time.");
    println!("Press Ctrl-C to leave the live session.");
    println!();

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<ShutdownKind>(4);
    let control = Arc::new(DaemonControl {
        paths: paths.clone(),
        role: Role::Participant,
        ws_port: 0,
        shutdown: shutdown_tx,
        client: client.clone(),
    });
    start_ipc(control.clone(), Role::Participant, session.session_id).await?;
    start_watcher(&paths, client.clone())?;

    let key = PeerKey::derive(&secret, session.session_id);
    drop(secret);
    let connection = tokio::spawn(supervise_connection(
        url,
        key,
        client.clone(),
        BlobStore::open(paths.blobs())?,
    ));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("\nLeaving the Weave session."),
        kind = shutdown_rx.recv() => {
            if kind == Some(ShutdownKind::Leave) {
                clear_session_record(&paths)?;
                println!("Left the Weave session.");
            } else {
                println!("Weave stopped.");
            }
        }
    }

    connection.abort();
    client.send(ClientInput::Shutdown);
    Ok(())
}

// ---------------------------------------------------------------------------
// weave resume
// ---------------------------------------------------------------------------

pub fn run_resume(start_dir: &Path) -> Result<()> {
    let paths = Paths::discover(start_dir)?;
    let record = load_session_record(&paths)?.ok_or_else(|| {
        session_err("There is no Weave session to resume in this repository.")
            .with_detail("Start one with `weave host`, or join one with `weave join`.")
    })?;
    match record.role {
        Role::Host => {
            let opts = match record.mode {
                TransportMode::Lan => HostOptions {
                    lan: true,
                    local_only: false,
                },
                TransportMode::Local => HostOptions {
                    lan: false,
                    local_only: true,
                },
                TransportMode::Tunnel => HostOptions {
                    lan: false,
                    local_only: false,
                },
            };
            run_host(start_dir, opts)
        }
        Role::Participant => {
            let endpoint = record.endpoint.clone().ok_or_else(|| {
                session_err("This participant session has no recorded host endpoint.")
                    .with_detail("Ask the host for a fresh invite and run `weave join`.")
            })?;
            let invite = encode_invite(&InvitePayload {
                protocol_version: PROTOCOL_VERSION,
                url: endpoint,
                session_id: record.session.session_id,
                secret: record.secret.clone(),
                base_commit: record.session.base_commit.clone(),
                branch: record.session.branch.clone(),
                repo_name: record.session.repo_name.clone(),
            })?;
            run_join(
                start_dir,
                JoinOptions {
                    invite,
                    preflighted: false,
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WsState {
    key: PeerKey,
    host: HostHandle,
    next_conn: Arc<AtomicU64>,
    /// Bounds how many unauthenticated peers can sit mid-handshake at once, so
    /// an anonymous caller cannot turn the public endpoint into an unbounded
    /// memory and task sink.
    handshakes: Arc<Semaphore>,
    /// Read by each connection's transfer pump. The store is shared by every
    /// connection and by the engines; it is content-addressed and append-only,
    /// so concurrent readers need no coordination.
    blobs: BlobStore,
}

/// The upgrade itself carries no authentication.
///
/// It cannot: proving possession of the session secret now means completing a
/// Noise handshake, and that needs a bidirectional channel. Nothing about the
/// session is disclosed by the upgrade succeeding — the endpoint answers
/// identically to a peer holding the secret and to one that is guessing.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<WsState>) -> Response {
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| serve_socket(socket, state))
}

async fn serve_socket(socket: WebSocket, state: WsState) {
    let Ok(permit) = state.handshakes.clone().try_acquire_owned() else {
        tracing::warn!("refusing a Weave connection: too many handshakes in flight");
        return;
    };

    let (mut sink, mut stream) = socket.split();
    // The timeout covers the whole exchange, not just the read: a peer that
    // sends its message and then stops reading would otherwise hold a handshake
    // permit for as long as it liked once the socket stopped draining.
    let handshake = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        host_handshake(&state.key, &mut sink, &mut stream),
    )
    .await
    .unwrap_or_else(|_| Err(crate::error::network("The Weave handshake timed out.")));
    let channel = match handshake {
        Ok(channel) => Arc::new(channel),
        Err(e) => {
            // Deliberately terse and generic: a peer that fails the handshake
            // learns only that it failed.
            tracing::warn!("a peer failed the Weave handshake: {}", e.message);
            let _ = sink.close().await;
            return;
        }
    };
    drop(permit);

    // Only now does this connection exist as far as the session is concerned.
    let conn_id = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let (out, rx, queued) = default_outbound();
    let (pump, jobs) = blob_pump();
    state.host.send(HostInput::Connected {
        conn_id,
        out: out.clone(),
        pump,
        is_local: false,
    });

    let writer = tokio::spawn(encrypt_to_sink(rx, queued, sink, channel.clone()));
    let pumping = tokio::spawn(run_pump(jobs, state.blobs.clone(), out.clone()));
    // Inbound data is bounded here rather than in the engine's channel; see
    // [`MAX_INFLIGHT_DATA_FRAMES`].
    let slots = Arc::new(Semaphore::new(MAX_INFLIGHT_DATA_FRAMES));

    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Binary(bytes) => match channel.decrypt(&bytes) {
                // A partial application message; more frames follow.
                Ok(None) => {}
                Ok(Some((FrameClass::Control, plaintext))) => {
                    if !dispatch_client_message(&plaintext, conn_id, &state.host, &out) {
                        break;
                    }
                }
                // Blob traffic. Waiting for a permit here is the whole
                // backpressure story on this side: a peer that outruns the
                // engine's installs stalls in TCP, not in this process.
                Ok(Some((FrameClass::Data, payload))) => {
                    let Ok(permit) = slots.clone().acquire_owned().await else {
                        break;
                    };
                    state.host.send(HostInput::Data {
                        conn_id,
                        frame: DataFrame::new(payload, permit),
                    });
                }
                Err(e) => {
                    tracing::warn!("dropping a Weave connection: {}", e.message);
                    break;
                }
            },
            // Nothing is accepted outside the encrypted channel.
            Message::Text(_) => {
                tracing::warn!("dropping a Weave connection that sent an unencrypted frame");
                break;
            }
            Message::Close(_) => break,
            _ => {}
        }
        if out.is_closed() {
            break;
        }
    }

    out.close();
    writer.abort();
    pumping.abort();
    state.host.send(HostInput::Disconnected { conn_id });
}

// ---------------------------------------------------------------------------
// Blob transfer pump
// ---------------------------------------------------------------------------

/// Stream requested blobs to one peer, one at a time, forever.
///
/// Serial on purpose: interleaving transfers on one connection would not make
/// the link any faster, and it would multiply the number of `.part` files the
/// receiver holds open. Control frames still overtake these at every frame
/// boundary, so a transfer in progress never makes the session look frozen.
async fn run_pump(mut jobs: mpsc::UnboundedReceiver<PumpJob>, blobs: BlobStore, out: Outbound) {
    while let Some(job) = jobs.recv().await {
        if let Err(e) = stream_blob(&blobs, &out, &job).await {
            tracing::warn!(
                "blob transfer {} ({}) failed: {}",
                job.transfer_id,
                crate::util::short_oid(&job.hash),
                e.message
            );
            // In-band, so it cannot overtake the chunks it cancels. The peer
            // discards its partial write and asks again.
            out.send_data(crate::blobwire::abort_frame(job.transfer_id))
                .await;
        }
        if out.is_closed() {
            return;
        }
    }
}

/// Send one blob as chunk frames followed by an end frame.
///
/// The file is read in `WIRE_CHUNK` slices and never held whole, at either end
/// of the connection or in between.
async fn stream_blob(blobs: &BlobStore, out: &Outbound, job: &PumpJob) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let path = blobs.path_of(&job.hash)?;
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| crate::error::persistence(format!("Could not read a Weave blob: {e}")))?;
    if job.from_offset > 0 {
        file.seek(std::io::SeekFrom::Start(job.from_offset))
            .await
            .map_err(|e| {
                crate::error::persistence(format!("Could not resume a Weave blob: {e}"))
            })?;
    }

    let mut buffer = vec![0u8; crate::blobwire::WIRE_CHUNK];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|e| crate::error::persistence(format!("Could not read a Weave blob: {e}")))?;
        if read == 0 {
            break;
        }
        if !out
            .send_data(crate::blobwire::chunk_frame(
                job.transfer_id,
                &buffer[..read],
            ))
            .await
        {
            return Err(crate::error::network("The connection closed mid-transfer."));
        }
    }
    if !out
        .send_data(crate::blobwire::end_frame(job.transfer_id))
        .await
    {
        return Err(crate::error::network("The connection closed mid-transfer."));
    }
    Ok(())
}

/// Read the initiator's message, reply, and return the established channel.
///
/// Until this returns `Ok`, the caller has told the host engine nothing and has
/// sent nothing but a fixed-size handshake reply. The caller bounds this in
/// time; there is no internal timeout to keep in step with that one.
async fn host_handshake(
    key: &PeerKey,
    sink: &mut SplitSink<WebSocket, Message>,
    stream: &mut SplitStream<WebSocket>,
) -> Result<SecureChannel> {
    let first = next_binary_frame(stream).await?;

    let responder = Responder::new(&key.psk, key.session_id)?;
    let (reply, channel) = responder.respond(&first)?;
    sink.send(Message::Binary(reply.into()))
        .await
        .map_err(|e| crate::error::network(format!("Could not send the Weave handshake: {e}")))?;
    Ok(channel)
}

async fn next_binary_frame(stream: &mut SplitStream<WebSocket>) -> Result<Vec<u8>> {
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Binary(bytes)) => return Ok(bytes.into()),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
            Ok(_) => {
                return Err(crate::error::network(
                    "The peer did not begin with a Weave handshake.",
                ))
            }
            Err(e) => {
                return Err(crate::error::network(format!(
                    "The connection ended during the Weave handshake: {e}"
                )))
            }
        }
    }
    Err(crate::error::network(
        "The connection closed before the Weave handshake completed.",
    ))
}

/// Decode one decrypted application message and hand it to the host engine.
/// Returns `false` when the connection must be dropped.
fn dispatch_client_message(
    plaintext: &[u8],
    conn_id: u64,
    host: &HostHandle,
    out: &crate::transport::Outbound,
) -> bool {
    match serde_json::from_slice::<ClientEnvelope>(plaintext) {
        Ok(envelope) => {
            if envelope.protocol_version != PROTOCOL_VERSION {
                out.send_host(crate::proto::HostMessage::Error {
                    request_id: None,
                    class: crate::error::ErrorClass::ProtocolError,
                    message: format!(
                        "Unsupported Weave protocol version {}.",
                        envelope.protocol_version
                    ),
                    detail: Some("Every participant must run a matching Weave version.".into()),
                });
                return false;
            }
            host.send(HostInput::Message {
                conn_id,
                message: envelope.message,
            });
            true
        }
        Err(e) => {
            // The frame authenticated, so this is a version or bug problem
            // rather than an attack; the peer gets a useful message.
            out.send_host(crate::proto::HostMessage::Error {
                request_id: None,
                class: crate::error::ErrorClass::ProtocolError,
                message: format!("Malformed Weave message: {e}"),
                detail: None,
            });
            true
        }
    }
}

/// Drain the outbound queues, encrypting each message into one or more frames.
///
/// Control is served at strict priority; see [`OutboundRx::next`]. A data
/// message is always exactly one Noise message, so the longest a control
/// message can wait here is one frame on the wire.
async fn encrypt_to_sink(
    mut rx: OutboundRx,
    queued: Arc<std::sync::atomic::AtomicUsize>,
    mut sink: SplitSink<WebSocket, Message>,
    channel: Arc<SecureChannel>,
) {
    while let Some(frame) = rx.next().await {
        let (class, plaintext) = release(frame, &queued);
        let frames = match channel.encrypt(class, &plaintext) {
            Ok(frames) => frames,
            Err(e) => {
                tracing::error!("could not encrypt a Weave message: {}", e.message);
                break;
            }
        };
        let mut ok = true;
        for frame in frames {
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                ok = false;
                break;
            }
        }
        if !ok {
            break;
        }
    }
    let _ = sink.close().await;
}

/// Turn a queued frame into a plaintext, giving the control queue its bytes
/// back at the moment the message stops being queued.
fn release(frame: Frame, queued: &Arc<std::sync::atomic::AtomicUsize>) -> (FrameClass, Vec<u8>) {
    match frame {
        Frame::Control(text) => {
            let len = text.len();
            queued.fetch_sub(len.min(queued.load(Ordering::Relaxed)), Ordering::Relaxed);
            (FrameClass::Control, text.into_bytes())
        }
        // Data is bounded by the queue's frame count, not by a byte total.
        Frame::Data(bytes) => (FrameClass::Data, bytes),
    }
}

// ---------------------------------------------------------------------------
// Loopback pair for the host's own participation
// ---------------------------------------------------------------------------

fn spawn_loopback(host: HostHandle, client: ClientHandle) {
    let conn_id = 0u64;
    let (host_out, mut host_rx, host_queued) = default_outbound();
    let (client_out, mut client_rx, client_queued) = default_outbound();
    let (host_pump, host_jobs) = blob_pump();
    let (client_pump, client_jobs) = blob_pump();

    host.send(HostInput::Connected {
        conn_id,
        out: host_out,
        pump: host_pump,
        is_local: true,
    });

    // Neither side can legitimately ask for a transfer here, so both pump
    // queues exist only to be drained and complained about.
    drain_loopback_pump(host_jobs, "host->client");
    drain_loopback_pump(client_jobs, "client->host");

    // The host and its local client share one blob store, so every hash either
    // side needs is already on disk and the data plane is never used here. A
    // data frame on the loopback is a bug, not a slow path.
    let client_for_host = client.clone();
    tokio::spawn(async move {
        while let Some(frame) = host_rx.next().await {
            match release(frame, &host_queued) {
                (FrameClass::Control, plaintext) => {
                    match serde_json::from_slice::<HostEnvelope>(&plaintext) {
                        Ok(envelope) => client_for_host.send(ClientInput::Host(envelope.message)),
                        Err(e) => tracing::error!("loopback decode (host->client): {e}"),
                    }
                }
                (FrameClass::Data, _) => {
                    tracing::error!("loopback carried a data frame (host->client)")
                }
            }
        }
    });

    let host_for_client = host.clone();
    tokio::spawn(async move {
        while let Some(frame) = client_rx.next().await {
            match release(frame, &client_queued) {
                (FrameClass::Control, plaintext) => {
                    match serde_json::from_slice::<ClientEnvelope>(&plaintext) {
                        Ok(envelope) => host_for_client.send(HostInput::Message {
                            conn_id,
                            message: envelope.message,
                        }),
                        Err(e) => tracing::error!("loopback decode (client->host): {e}"),
                    }
                }
                (FrameClass::Data, _) => {
                    tracing::error!("loopback carried a data frame (client->host)")
                }
            }
        }
    });

    client.send(ClientInput::Connected {
        out: client_out,
        pump: client_pump,
    });
}

/// Report, and discard, any transfer requested over the loopback.
fn drain_loopback_pump(mut jobs: mpsc::UnboundedReceiver<PumpJob>, direction: &'static str) {
    tokio::spawn(async move {
        while let Some(job) = jobs.recv().await {
            tracing::error!(
                "loopback requested blob {} ({direction})",
                crate::util::short_oid(&job.hash)
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Participant WebSocket client
// ---------------------------------------------------------------------------

/// Install the TLS crypto provider once per process.
///
/// `rustls` 0.23 refuses to pick a provider implicitly, and a `wss://` connect
/// panics without one. Weave pins `ring`, which needs no external toolchain on
/// any supported platform.
fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err means another component already installed one, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Keep the connection loop running.
///
/// `client_connection_loop` never returns on its own, so if its task ends, it
/// ended abnormally. Without this the participant would sit "offline" forever
/// with no path back, which is exactly the failure mode local editing must
/// survive. Restarting costs nothing and cannot lose queued work: the outbox is
/// durable and every operation is idempotent.
async fn supervise_connection(url: String, key: PeerKey, client: ClientHandle, blobs: BlobStore) {
    loop {
        let task = tokio::spawn(client_connection_loop(
            url.clone(),
            key.clone(),
            client.clone(),
            blobs.clone(),
        ));
        match task.await {
            Ok(()) => return,
            Err(e) if e.is_cancelled() => return,
            Err(e) => {
                tracing::error!("connection task stopped unexpectedly: {e}; restarting");
                client.send(ClientInput::Disconnected(
                    "connection task restarted".into(),
                ));
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

async fn client_connection_loop(url: String, key: PeerKey, client: ClientHandle, blobs: BlobStore) {
    ensure_crypto_provider();
    let mut backoff = 1u64;
    loop {
        match connect_once(&url, &key, &client, &blobs).await {
            Ok(reason) => {
                client.send(ClientInput::Disconnected(reason));
                backoff = 1;
            }
            Err(e) => {
                client.send(ClientInput::Disconnected(e.message.clone()));
                tracing::warn!("connection failed: {}", e.message);
                // Capped low on purpose: after `weave tunnel restart` the new
                // hostname needs a moment to resolve, and a coarse backoff
                // would add tens of seconds of avoidable downtime. One DNS
                // lookup every few seconds costs nothing at this scale.
                backoff = (backoff * 2).min(8);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
    }
}

async fn connect_once(
    url: &str,
    key: &PeerKey,
    client: &ClientHandle,
    blobs: &BlobStore,
) -> Result<String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    // No credential travels with the request. The session secret authenticates
    // the Noise handshake below and never leaves this machine.
    let request = url
        .into_client_request()
        .map_err(|e| crate::error::network(format!("Invalid Weave host URL: {e}")))?;
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false)
        .await
        .map_err(describe_connect_failure)?;

    let (mut sink, mut stream) = socket.split();

    // Handshake first: nothing about this repository is sent, and nothing the
    // host says is trusted, until it succeeds. The send is inside the timeout
    // too — an endpoint that accepts the upgrade and then stops reading must
    // not strand the connection task, or the participant sits offline with no
    // path back.
    let mut initiator = Initiator::new(&key.psk, key.session_id)?;
    let first = initiator.first_message()?;
    let exchange = async {
        sink.send(WsMessage::Binary(first.into()))
            .await
            .map_err(|e| {
                crate::error::network(format!("Could not start the Weave handshake: {e}"))
            })?;
        next_host_frame(&mut stream).await
    };
    let reply = match tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange).await {
        Ok(reply) => reply?,
        Err(_) => {
            let _ = sink.close().await;
            return Err(crate::error::network("The Weave handshake timed out."));
        }
    };
    let channel = Arc::new(initiator.finish(&reply)?);

    let (out, mut rx, queued) = default_outbound();
    let sender = channel.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.next().await {
            let (class, plaintext) = release(frame, &queued);
            let frames = match sender.encrypt(class, &plaintext) {
                Ok(frames) => frames,
                Err(e) => {
                    tracing::error!("could not encrypt a Weave message: {}", e.message);
                    break;
                }
            };
            let mut ok = true;
            for frame in frames {
                if sink.send(WsMessage::Binary(frame.into())).await.is_err() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let (pump, jobs) = blob_pump();
    let pumping = tokio::spawn(run_pump(jobs, blobs.clone(), out.clone()));
    let slots = Arc::new(Semaphore::new(MAX_INFLIGHT_DATA_FRAMES));

    client.send(ClientInput::Connected {
        out: out.clone(),
        pump,
    });

    while let Some(message) = stream.next().await {
        let message = match message {
            Ok(m) => m,
            Err(e) => {
                out.close();
                writer.abort();
                pumping.abort();
                return Ok(format!("connection lost: {e}"));
            }
        };
        match message {
            WsMessage::Binary(bytes) => match channel.decrypt(&bytes) {
                Ok(None) => {}
                Ok(Some((FrameClass::Control, plaintext))) => {
                    match serde_json::from_slice::<HostEnvelope>(&plaintext) {
                        Ok(envelope) => {
                            if envelope.protocol_version != PROTOCOL_VERSION {
                                out.close();
                                writer.abort();
                                pumping.abort();
                                return Ok(format!(
                                    "host speaks Weave protocol version {}",
                                    envelope.protocol_version
                                ));
                            }
                            client.send(ClientInput::Host(envelope.message));
                        }
                        Err(e) => tracing::error!("malformed host message: {e}"),
                    }
                }
                // Blob traffic; see the host side for why the permit is
                // acquired before the frame reaches the engine.
                Ok(Some((FrameClass::Data, payload))) => {
                    let Ok(permit) = slots.clone().acquire_owned().await else {
                        break;
                    };
                    client.send(ClientInput::Data(DataFrame::new(payload, permit)));
                }
                Err(e) => {
                    out.close();
                    writer.abort();
                    pumping.abort();
                    return Ok(e.message);
                }
            },
            // The host never speaks outside the encrypted channel.
            WsMessage::Text(_) => {
                out.close();
                writer.abort();
                pumping.abort();
                return Ok("the host sent an unencrypted frame".into());
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
        if out.is_closed() {
            break;
        }
    }
    out.close();
    writer.abort();
    pumping.abort();
    Ok("disconnected".into())
}

/// Await the host's handshake reply, ignoring keepalives.
async fn next_host_frame(
    stream: &mut SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) -> Result<Vec<u8>> {
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    while let Some(message) = stream.next().await {
        match message {
            Ok(WsMessage::Binary(bytes)) => return Ok(bytes.into()),
            Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => continue,
            Ok(_) => {
                return Err(crate::error::network(
                    "The Weave host did not answer with a handshake.",
                )
                .with_detail(
                    "The endpoint answered, but not as a Weave 2 host. Check the invite.",
                ))
            }
            Err(e) => {
                return Err(crate::error::network(format!(
                    "The connection ended during the Weave handshake: {e}"
                )))
            }
        }
    }
    Err(
        crate::error::network("The Weave host closed the connection during the handshake.")
            .with_detail(
            "The host rejects peers whose session secret does not match. Ask for a fresh invite.",
        ),
    )
}

/// Turn a WebSocket connect failure into something a person can act on.
///
/// A Weave 1.x host has no `/weave/v2` route, so it answers 404; the same host
/// answered 401 to a missing bearer token. Either way the answer is "upgrade",
/// never "retry without encryption".
fn describe_connect_failure(e: tokio_tungstenite::tungstenite::Error) -> crate::error::WeaveError {
    use tokio_tungstenite::tungstenite::Error as WsError;
    if let WsError::Http(response) = &e {
        let status = response.status().as_u16();
        if status == 404 || status == 401 {
            return crate::error::protocol(
                "This host is not running an end-to-end encrypted Weave session.",
            )
            .with_detail(
                "It answered as a Weave 1.x host, whose application protocol was not encrypted. \
                 Weave will not fall back to it. Both sides must run Weave 2 or newer.",
            );
        }
    }
    crate::error::network(format!("Could not reach the Weave host: {e}"))
}

// ---------------------------------------------------------------------------
// Local IPC server
// ---------------------------------------------------------------------------

async fn start_ipc(control: Arc<DaemonControl>, role: Role, session_id: Uuid) -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| {
            session_err(format!(
                "Could not bind the local Weave control endpoint: {e}"
            ))
        })?;
    let port = listener
        .local_addr()
        .map_err(|e| session_err(format!("Could not read the control endpoint address: {e}")))?
        .port();
    let token = new_local_token();

    write_runtime(
        &control.paths,
        &Runtime {
            pid: std::process::id(),
            port,
            token: token.clone(),
            role: role.as_str().to_string(),
            session_id,
            started_at_ms: crate::util::now_ms(),
        },
    )?;

    let token = Arc::new(token);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let control = control.clone();
            let token = token.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_ipc_connection(stream, control, token).await {
                    tracing::debug!("ipc connection ended: {e}");
                }
            });
        }
    });
    Ok(())
}

async fn serve_ipc_connection(
    stream: tokio::net::TcpStream,
    control: Arc<DaemonControl>,
    token: Arc<String>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<IpcRequest>(&line) {
            Ok(request) => {
                if !secret_matches(&token, &request.token) {
                    IpcResponse::error(&session_err("Invalid local Weave control token."))
                } else {
                    handle_ipc_command(&control, request.command).await
                }
            }
            Err(e) => IpcResponse::error(&crate::error::usage(format!(
                "Malformed local Weave request: {e}"
            ))),
        };
        let text = serde_json::to_string(&response).unwrap_or_else(|_| "{\"ok\":false}".into());
        write_half.write_all(text.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
    }
    Ok(())
}

async fn handle_ipc_command(control: &Arc<DaemonControl>, command: IpcCommand) -> IpcResponse {
    match command {
        IpcCommand::Stop => {
            let _ = control.shutdown.send(ShutdownKind::Stop).await;
            IpcResponse::ok(serde_json::json!({ "stopping": true }))
        }
        IpcCommand::Leave => {
            let _ = control.shutdown.send(ShutdownKind::Leave).await;
            IpcResponse::ok(serde_json::json!({ "leaving": true }))
        }
        IpcCommand::Invite => match invite_text(control) {
            Ok(value) => IpcResponse::ok(value),
            Err(e) => IpcResponse::error(&e),
        },
        IpcCommand::TunnelRestart => match restart_tunnel(control).await {
            Ok(value) => IpcResponse::ok(value),
            Err(e) => IpcResponse::error(&e),
        },
        other => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            control.client.send(ClientInput::Ipc(IpcCall {
                command: other,
                reply: tx,
            }));
            match tokio::time::timeout(std::time::Duration::from_secs(110), rx).await {
                Ok(Ok(response)) => response,
                Ok(Err(_)) => {
                    IpcResponse::error(&session_err("The Weave engine dropped the request."))
                }
                Err(_) => IpcResponse::error(
                    &crate::error::network("Timed out waiting for the Weave host.").with_detail(
                        "The host may be unreachable. Run `weave status` to check the connection.",
                    ),
                ),
            }
        }
    }
}

fn invite_text(control: &Arc<DaemonControl>) -> Result<serde_json::Value> {
    let record = load_session_record(&control.paths)?
        .ok_or_else(|| session_err("No Weave session record is present."))?;
    if record.role != Role::Host {
        return Err(crate::error::usage(
            "Only the host can produce a Weave invite.",
        ));
    }
    let Some(endpoint) = record.endpoint.clone() else {
        return Err(session_err("This session has no remote endpoint.").with_detail(
            "It was started with --local. Restart it with `weave host` or `weave host --lan` to \
             invite other participants.",
        ));
    };
    let invite = encode_invite(&InvitePayload {
        protocol_version: PROTOCOL_VERSION,
        url: endpoint.clone(),
        session_id: record.session.session_id,
        secret: record.secret.clone(),
        base_commit: record.session.base_commit.clone(),
        branch: record.session.branch.clone(),
        repo_name: record.session.repo_name.clone(),
    })?;
    Ok(serde_json::json!({
        "invite": invite,
        "endpoint": endpoint,
        "session_id": record.session.session_id,
        "branch": record.session.branch,
        "base_commit": record.session.base_commit,
    }))
}

/// Replace a dead Quick Tunnel without recreating the session
/// (specification section 62).
async fn restart_tunnel(control: &Arc<DaemonControl>) -> Result<serde_json::Value> {
    if control.role != Role::Host {
        return Err(crate::error::usage(
            "Only the host manages the Weave tunnel.",
        ));
    }
    let mut record = load_session_record(&control.paths)?
        .ok_or_else(|| session_err("No Weave session record is present."))?;
    if record.mode != TransportMode::Tunnel {
        return Err(
            crate::error::usage("This session is not using a Cloudflare Quick Tunnel.")
                .with_detail("Tunnel restart applies to sessions started with plain `weave host`."),
        );
    }
    TUNNEL.shutdown().await;
    let tunnel = crate::tunnel::start(control.ws_port).await?;
    let url = tunnel.websocket_url();
    TUNNEL.set(tunnel).await;

    record.endpoint = Some(url.clone());
    save_session_record(&control.paths, &record)?;
    let invite = encode_invite(&InvitePayload {
        protocol_version: PROTOCOL_VERSION,
        url: url.clone(),
        session_id: record.session.session_id,
        secret: record.secret.clone(),
        base_commit: record.session.base_commit.clone(),
        branch: record.session.branch.clone(),
        repo_name: record.session.repo_name.clone(),
    })?;
    Ok(serde_json::json!({
        "endpoint": url,
        "invite": invite,
        "session_id": record.session.session_id,
    }))
}

// ---------------------------------------------------------------------------
// Watcher plumbing
// ---------------------------------------------------------------------------

fn start_watcher(paths: &Paths, client: ClientHandle) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = watch::start(&paths.repo_root, tx)?;
    std::thread::Builder::new()
        .name("weave-watch-bridge".into())
        .spawn(move || {
            let _keep_alive = handle;
            while let Ok(event) = rx.recv() {
                client.send(ClientInput::Watch(event));
            }
        })
        .map_err(|e| session_err(format!("Could not start the watcher bridge: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Repository preconditions (specification sections 10, 12)
// ---------------------------------------------------------------------------

pub fn verify_supported_repository(paths: &Paths) -> Result<()> {
    let unsupported_features = gitx::detect_unsupported(&paths.repo_root)?;
    if !unsupported_features.is_empty() {
        let mut detail = String::new();
        for item in &unsupported_features {
            detail.push_str(&format!("- {}: {}\n", item.feature, item.detail));
        }
        detail.push_str(
            "\nWeave V1 deliberately refuses these features rather than corrupting them.",
        );
        return Err(
            unsupported("This repository uses features Weave V1 does not support.")
                .with_detail(detail),
        );
    }
    if let Some(operation) = gitx::operation_in_progress(&paths.repo_root)? {
        return Err(repository(format!("A Git {operation} is in progress."))
            .with_detail("Finish or abort the Git operation before using Weave."));
    }
    Ok(())
}

pub fn verify_clean_working_tree(paths: &Paths) -> Result<()> {
    let dirty = gitx::dirty_entries(&paths.repo_root)?;
    if !dirty.is_empty() {
        let preview: Vec<String> = dirty.iter().take(12).cloned().collect();
        return Err(
            repository("The working tree is not clean.").with_detail(format!(
                "{}{}\n\nCommit, stash or discard these changes before starting a Weave session.",
                preview.join("\n"),
                if dirty.len() > preview.len() {
                    format!("\n... and {} more", dirty.len() - preview.len())
                } else {
                    String::new()
                }
            )),
        );
    }
    Ok(())
}

fn verify_new_host_repository(paths: &Paths) -> Result<()> {
    verify_supported_repository(paths)?;
    if gitx::current_branch(&paths.repo_root)?.is_none() {
        return Err(repository("HEAD is detached.")
            .with_detail("Weave needs one checked-out branch. Run `git switch <branch>`."));
    }
    verify_clean_working_tree(paths)?;
    Ok(())
}

fn report_rejected(rejected: &[crate::scan::RejectedPath]) {
    if rejected.is_empty() {
        return;
    }
    eprintln!("Some paths cannot participate in this Weave session:");
    for item in rejected.iter().take(20) {
        eprintln!("  {} — {}", item.path, item.reason);
    }
    if rejected.len() > 20 {
        eprintln!("  ... and {} more", rejected.len() - 20);
    }
    eprintln!();
}

// ---------------------------------------------------------------------------
// Process-wide tunnel slot
// ---------------------------------------------------------------------------

struct TunnelSlot {
    inner: tokio::sync::Mutex<Option<crate::tunnel::Tunnel>>,
}

impl TunnelSlot {
    async fn set(&self, tunnel: crate::tunnel::Tunnel) {
        let mut guard = self.inner.lock().await;
        if let Some(old) = guard.take() {
            old.shutdown().await;
        }
        *guard = Some(tunnel);
    }

    async fn shutdown(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(old) = guard.take() {
            old.shutdown().await;
        }
    }
}

static TUNNEL: TunnelSlot = TunnelSlot {
    inner: tokio::sync::Mutex::const_new(None),
};

// ---------------------------------------------------------------------------
// Banner
// ---------------------------------------------------------------------------

fn print_host_banner(
    paths: &Paths,
    session: &SessionInfo,
    record: &SessionRecord,
    ws_port: u16,
    display_name: &str,
) -> Result<()> {
    println!("Weave — {}", paths.repo_name());
    println!();
    println!("Role: host");
    println!("You:  {display_name}");
    println!("Branch: {}", session.branch);
    println!(
        "Base commit: {}",
        crate::util::short_oid(&session.base_commit)
    );
    println!();
    match record.mode {
        TransportMode::Tunnel => {
            let invite = encode_invite(&InvitePayload {
                protocol_version: PROTOCOL_VERSION,
                url: record.endpoint.clone().unwrap_or_default(),
                session_id: session.session_id,
                secret: record.secret.clone(),
                base_commit: session.base_commit.clone(),
                branch: session.branch.clone(),
                repo_name: session.repo_name.clone(),
            })?;
            println!("Share this invite with participants:");
            println!();
            println!("{invite}");
            println!();
            println!("The invite grants full read/write access to this session. Send it over a");
            println!("channel you trust.");
        }
        TransportMode::Lan => {
            let invite = encode_invite(&InvitePayload {
                protocol_version: PROTOCOL_VERSION,
                url: record.endpoint.clone().unwrap_or_default(),
                session_id: session.session_id,
                secret: record.secret.clone(),
                base_commit: session.base_commit.clone(),
                branch: session.branch.clone(),
                repo_name: session.repo_name.clone(),
            })?;
            println!("Weave LAN session");
            println!();
            println!("Address:");
            println!("{}:{}", local_ip(), ws_port);
            println!();
            println!("Share this invite with participants on the same network:");
            println!();
            println!("{invite}");
        }
        TransportMode::Local => {
            println!("Local session: no remote endpoint is exposed.");
        }
    }
    println!();
    println!("Run `weave status` from another terminal at any time.");
    println!("Press Ctrl-C to stop hosting.");
    println!();
    Ok(())
}

/// Address advertised to participants in LAN mode.
///
/// `WEAVE_LAN_ADDRESS` overrides the detected interface, which matters when the
/// host is reachable under a different name than its default route suggests
/// (containers, NAT, several interfaces) and in the integration tests.
fn local_ip() -> String {
    if let Ok(address) = std::env::var("WEAVE_LAN_ADDRESS") {
        if !address.trim().is_empty() {
            return address.trim().to_string();
        }
    }
    detect_local_ip()
}

/// Best-effort local IPv4 address. No packets are sent.
fn detect_local_ip() -> String {
    match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => {
            if socket.connect("192.0.2.1:9").is_ok() {
                if let Ok(addr) = socket.local_addr() {
                    return addr.ip().to_string();
                }
            }
            "127.0.0.1".into()
        }
        Err(_) => "127.0.0.1".into(),
    }
}
