// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A participant's half of the blob plane.
//!
//! Included by `client.rs` as a private module; it holds the transfer
//! bookkeeping that would otherwise crowd the replica state machine.
//!
//! Two directions, one shape. Uploads run before the control message that
//! depends on them, so the host never has to park an operation waiting for
//! content. Downloads run behind the control message that revealed the gap, so
//! a broadcast stays small whatever the file it announces.
//!
//! Concurrency is bounded here rather than at the host, because the receiver is
//! the side that owns the partial file, and therefore the side that knows how
//! many transfers it can have open at once. Both bounds sit below
//! [`blobwire::MAX_OPEN_TRANSFERS`] so a peer never has to refuse a transfer
//! this side considers legitimate.

use super::*;

const MAX_CONCURRENT_DOWNLOADS: usize = 8;
const MAX_CONCURRENT_UPLOADS: usize = 8;

/// A control message held back until the content it names is durable at the
/// host.
struct Deferred {
    /// Identifies the work this message belongs to, so a retry of the same
    /// operation replaces its predecessor instead of queueing beside it.
    key: Uuid,
    needs: Vec<String>,
    message: ClientMessage,
}

/// What the engine should do as a result of one blob-plane event.
///
/// Returned rather than performed so this type never needs a handle on the
/// engine that owns it.
#[derive(Default)]
pub(crate) struct Emitted {
    pub messages: Vec<ClientMessage>,
    pub jobs: Vec<PumpJob>,
    /// Blobs that became available locally during this event.
    pub installed: Vec<String>,
    /// Content the host refused, with its reason.
    pub refused: Vec<(String, String)>,
}

impl Emitted {
    fn merge(&mut self, other: Emitted) {
        self.messages.extend(other.messages);
        self.jobs.extend(other.jobs);
        self.installed.extend(other.installed);
        self.refused.extend(other.refused);
    }

    fn message(message: ClientMessage) -> Emitted {
        Emitted {
            messages: vec![message],
            ..Emitted::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
            && self.jobs.is_empty()
            && self.installed.is_empty()
            && self.refused.is_empty()
    }
}

pub(crate) struct BlobTraffic {
    blobs: BlobStore,
    ids: TransferIds,
    receiver: BlobReceiver,

    /// Hashes to fetch, beyond those already asked for.
    wanted: VecDeque<String>,
    queued: HashSet<String>,
    /// Hashes asked for and not yet resolved one way or the other.
    requested: HashSet<String>,

    /// Hashes to upload, beyond those already offered.
    to_upload: VecDeque<String>,
    to_upload_set: HashSet<String>,
    /// Offers made and not yet answered: transfer id -> hash.
    uploading: HashMap<u64, String>,
    /// Hashes the host has confirmed it holds, on this connection.
    at_host: HashSet<String>,

    deferred: Vec<Deferred>,
}

impl BlobTraffic {
    pub fn new(blobs: BlobStore) -> BlobTraffic {
        BlobTraffic {
            receiver: BlobReceiver::new(blobs.clone()),
            blobs,
            ids: TransferIds::default(),
            wanted: VecDeque::new(),
            queued: HashSet::new(),
            requested: HashSet::new(),
            to_upload: VecDeque::new(),
            to_upload_set: HashSet::new(),
            uploading: HashMap::new(),
            at_host: HashSet::new(),
            deferred: Vec::new(),
        }
    }

    /// Start again on a fresh connection.
    ///
    /// Transfer ids mean nothing across sockets and a partial file no sender
    /// will finish is only a way to install the wrong bytes later, so every
    /// transfer restarts. Nothing is forgotten: what was in flight goes back
    /// into the queue it came from, and deferred messages re-offer their
    /// content. The host answers `Have` at once for anything it already holds,
    /// so a reconnect costs one round trip per blob, not one retransmission.
    pub fn reconnected(&mut self) -> Emitted {
        self.receiver.clear();
        for hash in std::mem::take(&mut self.requested) {
            self.enqueue_want(hash);
        }
        for (_, hash) in std::mem::take(&mut self.uploading) {
            self.enqueue_upload(hash);
        }
        self.at_host.clear();
        let needed: Vec<String> = self
            .deferred
            .iter()
            .flat_map(|d| d.needs.iter().cloned())
            .collect();
        for hash in needed {
            self.enqueue_upload(hash);
        }
        let mut out = self.pump_wants();
        out.merge(self.pump_uploads());
        out
    }

    // -------------------------------------------------------------- downloads

    /// Note that this content has to reach the working tree.
    pub fn want<I: IntoIterator<Item = String>>(&mut self, hashes: I) -> Emitted {
        for hash in hashes {
            self.enqueue_want(hash);
        }
        self.pump_wants()
    }

    fn enqueue_want(&mut self, hash: String) {
        if self.blobs.has(&hash) || self.queued.contains(&hash) || self.requested.contains(&hash) {
            return;
        }
        self.queued.insert(hash.clone());
        self.wanted.push_back(hash);
    }

    fn pump_wants(&mut self) -> Emitted {
        let mut batch = Vec::new();
        while self.requested.len() + batch.len() < MAX_CONCURRENT_DOWNLOADS {
            let Some(hash) = self.wanted.pop_front() else {
                break;
            };
            self.queued.remove(&hash);
            // It may have arrived by another route since it was queued.
            if self.blobs.has(&hash) {
                continue;
            }
            self.requested.insert(hash.clone());
            batch.push(hash);
        }
        if batch.is_empty() {
            return Emitted::default();
        }
        Emitted::message(ClientMessage::RequestBlobs { hashes: batch })
    }

    /// True while content the working tree needs has still to arrive.
    pub fn waiting_for_content(&self) -> bool {
        !self.wanted.is_empty() || !self.requested.is_empty()
    }

    // ---------------------------------------------------------------- uploads

    /// Send `message` once every hash in `needs` is durable at the host.
    ///
    /// The message goes out immediately when the host already holds the
    /// content, which is the common case for a repeated edit or a revert.
    pub fn send_when_uploaded(
        &mut self,
        key: Uuid,
        needs: Vec<String>,
        message: ClientMessage,
    ) -> Emitted {
        self.deferred.retain(|d| d.key != key);
        if needs.iter().all(|hash| self.at_host.contains(hash)) {
            return Emitted::message(message);
        }
        for hash in &needs {
            self.enqueue_upload(hash.clone());
        }
        self.deferred.push(Deferred {
            key,
            needs,
            message,
        });
        self.pump_uploads()
    }

    fn enqueue_upload(&mut self, hash: String) {
        if self.at_host.contains(&hash)
            || self.to_upload_set.contains(&hash)
            || self.uploading.values().any(|h| *h == hash)
        {
            return;
        }
        self.to_upload_set.insert(hash.clone());
        self.to_upload.push_back(hash);
    }

    fn pump_uploads(&mut self) -> Emitted {
        let mut out = Emitted::default();
        while self.uploading.len() < MAX_CONCURRENT_UPLOADS {
            let Some(hash) = self.to_upload.pop_front() else {
                break;
            };
            self.to_upload_set.remove(&hash);
            let size = match self.blobs.size_of(&hash) {
                Ok(size) => size,
                Err(e) => {
                    // Content this replica captured is no longer on disk. The
                    // message that needs it can never be honoured, so drop it
                    // rather than leave the caller waiting forever; the outbox
                    // rebuilds the operation from the working tree.
                    out.refused.push((hash.clone(), e.message));
                    self.drop_deferred_needing(&hash);
                    continue;
                }
            };
            let transfer_id = self.ids.next_id();
            self.uploading.insert(transfer_id, hash.clone());
            out.messages.push(ClientMessage::Blob {
                blob: BlobControl::Offer {
                    transfer_id,
                    hash,
                    size,
                },
            });
        }
        out
    }

    fn confirm_at_host(&mut self, transfer_id: u64, hash: String) -> Emitted {
        self.uploading.remove(&transfer_id);
        self.at_host.insert(hash);
        let mut out = self.drain_deferred();
        out.merge(self.pump_uploads());
        out
    }

    fn drain_deferred(&mut self) -> Emitted {
        let mut out = Emitted::default();
        let mut still_waiting = Vec::new();
        for deferred in std::mem::take(&mut self.deferred) {
            if deferred.needs.iter().all(|h| self.at_host.contains(h)) {
                out.messages.push(deferred.message);
            } else {
                still_waiting.push(deferred);
            }
        }
        self.deferred = still_waiting;
        out
    }

    fn drop_deferred_needing(&mut self, hash: &str) {
        self.deferred.retain(|d| !d.needs.iter().any(|h| h == hash));
    }

    // --------------------------------------------------------------- incoming

    /// One blob-plane control message from the host.
    pub fn on_control(&mut self, blob: BlobControl) -> Emitted {
        match blob {
            // The host is offering content this replica asked for.
            BlobControl::Offer {
                transfer_id,
                hash,
                size,
            } => {
                if self.blobs.has(&hash) {
                    let mut out = self.resolve_download(&hash);
                    out.messages.push(ClientMessage::Blob {
                        blob: BlobControl::Have { transfer_id, hash },
                    });
                    return out;
                }
                match self.receiver.accept_offer(transfer_id, &hash, size) {
                    Ok(from_offset) => Emitted::message(ClientMessage::Blob {
                        blob: BlobControl::Want {
                            transfer_id,
                            from_offset,
                        },
                    }),
                    Err(e) => {
                        // Refusing here loses nothing: the hash stays wanted and
                        // the next rescan asks again with a free slot.
                        let mut out = self.resolve_download(&hash);
                        self.enqueue_want(hash.clone());
                        out.messages.push(ClientMessage::Blob {
                            blob: BlobControl::Failed {
                                transfer_id,
                                hash,
                                reason: e.message,
                            },
                        });
                        out
                    }
                }
            }
            // The host is ready to receive something this replica offered.
            BlobControl::Want {
                transfer_id,
                from_offset,
            } => {
                let Some(hash) = self.uploading.get(&transfer_id).cloned() else {
                    return Emitted::default();
                };
                Emitted {
                    jobs: vec![PumpJob {
                        transfer_id,
                        hash,
                        from_offset,
                    }],
                    ..Emitted::default()
                }
            }
            BlobControl::Have { transfer_id, hash } | BlobControl::Done { transfer_id, hash } => {
                self.confirm_at_host(transfer_id, hash)
            }
            BlobControl::Failed {
                transfer_id,
                hash,
                reason,
            } => {
                self.uploading.remove(&transfer_id);
                self.drop_deferred_needing(&hash);
                let mut out = self.pump_uploads();
                out.refused.push((hash, reason));
                out
            }
            BlobControl::Unavailable { hash, reason } => {
                let mut out = self.resolve_download(&hash);
                out.refused.push((hash, reason));
                out
            }
        }
    }

    /// One frame off the blob plane.
    pub fn on_data(&mut self, frame: &DataFrame) -> Emitted {
        let incoming = match blobwire::decode(frame.bytes()) {
            Ok(incoming) => incoming,
            Err(e) => {
                tracing::warn!("malformed blob frame from the host: {}", e.message);
                return Emitted::default();
            }
        };
        match self.receiver.accept(incoming) {
            Delivered::More => Emitted::default(),
            Delivered::Installed { transfer_id, hash } => {
                let mut out = self.resolve_download(&hash);
                out.messages.push(ClientMessage::Blob {
                    blob: BlobControl::Done {
                        transfer_id,
                        hash: hash.clone(),
                    },
                });
                out.installed.push(hash);
                out
            }
            Delivered::Failed {
                transfer_id,
                hash,
                reason,
            } => {
                // Nothing was installed. Requeue: a corrupt or interrupted
                // transfer must be retried, not silently abandoned.
                let mut out = self.resolve_download(&hash);
                self.enqueue_want(hash.clone());
                out.merge(self.pump_wants());
                out.messages.push(ClientMessage::Blob {
                    blob: BlobControl::Failed {
                        transfer_id,
                        hash: hash.clone(),
                        reason: reason.clone(),
                    },
                });
                out.refused.push((hash, reason));
                out
            }
            Delivered::Stray { transfer_id } => {
                tracing::warn!("ignoring a stray blob frame from the host ({transfer_id})");
                Emitted::default()
            }
        }
    }

    /// Free the slot a download occupied and start the next one.
    fn resolve_download(&mut self, hash: &str) -> Emitted {
        if !self.requested.remove(hash) {
            return Emitted::default();
        }
        self.pump_wants()
    }
}
