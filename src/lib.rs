// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Weave — a lightweight real-time collaboration layer above Git.
//!
//! Weave lets several people (and their agents) work simultaneously on
//! independent local copies of the same Git repository while one authoritative
//! host coordinator maintains a single shared live state. Git keeps its
//! ordinary role: durable history, branch identity, remotes, portability.
//!
//! The design priorities, in order, are: no lost edits; one authoritative
//! canonical state; deterministic global revision ordering; durable
//! acknowledgement semantics; explicit rather than silent conflict; no
//! corruption of the ordinary Git repository.

pub mod blobs;
pub mod bootstrap;
pub mod cli;
pub mod client;
pub mod daemon;
pub mod db;
pub mod doctor;
pub mod error;
pub mod gitx;
pub mod host;
pub mod install;
pub mod ipc;
pub mod model;
pub mod path;
pub mod proto;
pub mod reconcile;
pub mod recover;
pub mod scan;
pub mod secure;
pub mod session;
pub mod store_client;
pub mod store_host;
pub mod transport;
pub mod tunnel;
pub mod util;
pub mod watch;

pub use error::{ErrorClass, Result, WeaveError};
