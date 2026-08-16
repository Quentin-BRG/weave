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
use crate::session::*;
use crate::store_client::ClientStore;
use crate::store_host::HostStore;
use crate::transport::{default_outbound, secret_matches, MAX_FRAME, WS_PATH};
use crate::watch;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub struct HostOptions {
    pub lan: bool,
    pub local_only: bool,
}

#[derive(Debug, Clone)]
pub struct JoinOptions {
    pub invite: String,
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
            let scan = crate::scan::scan_repository(&paths.repo_root, &BTreeMap::new(), true)?;
            report_rejected(&scan.rejected);
            let mut manifest = BTreeMap::new();
            for (path, entry) in &scan.entries {
                if let Some(bytes) = scan.contents.get(path) {
                    blobs.put(bytes)?;
                }
                manifest.insert(path.clone(), entry.clone());
            }
            host_store.install_base_manifest(&manifest)?;
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
    ));

    drop(lock);
    let _ = clear_runtime(&paths);
    result
}

#[allow(clippy::too_many_arguments)]
async fn host_async(
    paths: Paths,
    session: SessionInfo,
    secret: String,
    opts: HostOptions,
    host: HostHandle,
    client: ClientHandle,
    display_name: String,
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
        secret: Arc::new(secret.clone()),
        session_id: session.session_id,
        host: host.clone(),
        next_conn: Arc::new(AtomicU64::new(1)),
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
    secret: String,
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

    let connection = tokio::spawn(client_connection_loop(url, secret, client.clone()));

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
            run_join(start_dir, JoinOptions { invite })
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WsState {
    secret: Arc<String>,
    session_id: Uuid,
    host: HostHandle,
    next_conn: Arc<AtomicU64>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
    headers: HeaderMap,
) -> Response {
    // The public hostname is not authentication (specification section 58).
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::transport::bearer)
        .unwrap_or("");
    if !secret_matches(&state.secret, provided) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let _ = state.session_id;
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| serve_socket(socket, state))
}

async fn serve_socket(socket: WebSocket, state: WsState) {
    let conn_id = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let (out, mut rx, queued) = default_outbound();
    state.host.send(HostInput::Connected {
        conn_id,
        out: out.clone(),
        is_local: false,
    });

    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let len = text.len();
            let send = sink.send(Message::Text(text.into())).await;
            queued.fetch_sub(len.min(queued.load(Ordering::Relaxed)), Ordering::Relaxed);
            if send.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(Ok(message)) = stream.next().await {
        match message {
            Message::Text(text) => match serde_json::from_str::<ClientEnvelope>(text.as_str()) {
                Ok(envelope) => {
                    if envelope.protocol_version != PROTOCOL_VERSION {
                        out.send_host(crate::proto::HostMessage::Error {
                            request_id: None,
                            class: crate::error::ErrorClass::ProtocolError,
                            message: format!(
                                "Unsupported Weave protocol version {}.",
                                envelope.protocol_version
                            ),
                            detail: Some(
                                "Every participant must run a matching Weave version.".into(),
                            ),
                        });
                        break;
                    }
                    state.host.send(HostInput::Message {
                        conn_id,
                        message: envelope.message,
                    });
                }
                Err(e) => {
                    out.send_host(crate::proto::HostMessage::Error {
                        request_id: None,
                        class: crate::error::ErrorClass::ProtocolError,
                        message: format!("Malformed Weave message: {e}"),
                        detail: None,
                    });
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
        if out.is_closed() {
            break;
        }
    }

    out.close();
    writer.abort();
    state.host.send(HostInput::Disconnected { conn_id });
}

// ---------------------------------------------------------------------------
// Loopback pair for the host's own participation
// ---------------------------------------------------------------------------

fn spawn_loopback(host: HostHandle, client: ClientHandle) {
    let conn_id = 0u64;
    let (host_out, mut host_rx, host_queued) = default_outbound();
    let (client_out, mut client_rx, client_queued) = default_outbound();

    host.send(HostInput::Connected {
        conn_id,
        out: host_out,
        is_local: true,
    });

    let client_for_host = client.clone();
    tokio::spawn(async move {
        while let Some(text) = host_rx.recv().await {
            let len = text.len();
            host_queued.fetch_sub(
                len.min(host_queued.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            match serde_json::from_str::<HostEnvelope>(&text) {
                Ok(envelope) => client_for_host.send(ClientInput::Host(envelope.message)),
                Err(e) => tracing::error!("loopback decode (host->client): {e}"),
            }
        }
    });

    let host_for_client = host.clone();
    tokio::spawn(async move {
        while let Some(text) = client_rx.recv().await {
            let len = text.len();
            client_queued.fetch_sub(
                len.min(client_queued.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            match serde_json::from_str::<ClientEnvelope>(&text) {
                Ok(envelope) => host_for_client.send(HostInput::Message {
                    conn_id,
                    message: envelope.message,
                }),
                Err(e) => tracing::error!("loopback decode (client->host): {e}"),
            }
        }
    });

    client.send(ClientInput::Connected(client_out));
}

// ---------------------------------------------------------------------------
// Participant WebSocket client
// ---------------------------------------------------------------------------

async fn client_connection_loop(url: String, secret: String, client: ClientHandle) {
    let mut backoff = 1u64;
    loop {
        match connect_once(&url, &secret, &client).await {
            Ok(reason) => {
                client.send(ClientInput::Disconnected(reason));
                backoff = 1;
            }
            Err(e) => {
                client.send(ClientInput::Disconnected(e.message.clone()));
                tracing::warn!("connection failed: {}", e.message);
                backoff = (backoff * 2).min(30);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
    }
}

async fn connect_once(url: &str, secret: &str, client: &ClientHandle) -> Result<String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url
        .into_client_request()
        .map_err(|e| crate::error::network(format!("Invalid Weave host URL: {e}")))?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {secret}")
            .parse()
            .map_err(|_| crate::error::network("Invalid session secret encoding."))?,
    );
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME));
    let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false)
        .await
        .map_err(|e| crate::error::network(format!("Could not reach the Weave host: {e}")))?;

    let (mut sink, mut stream) = socket.split();
    let (out, mut rx, queued) = default_outbound();
    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let len = text.len();
            let sent = sink
                .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                .await;
            queued.fetch_sub(len.min(queued.load(Ordering::Relaxed)), Ordering::Relaxed);
            if sent.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    client.send(ClientInput::Connected(out.clone()));

    while let Some(message) = stream.next().await {
        let message = match message {
            Ok(m) => m,
            Err(e) => {
                out.close();
                writer.abort();
                return Ok(format!("connection lost: {e}"));
            }
        };
        match message {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                match serde_json::from_str::<HostEnvelope>(text.as_str()) {
                    Ok(envelope) => {
                        if envelope.protocol_version != PROTOCOL_VERSION {
                            out.close();
                            writer.abort();
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
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => {}
        }
        if out.is_closed() {
            break;
        }
    }
    out.close();
    writer.abort();
    Ok("disconnected".into())
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
