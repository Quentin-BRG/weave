// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error classes for Weave (specification section 157).
//!
//! Every user-facing failure is one of these classes. Each class maps to a
//! stable exit code so that agents driving the CLI can branch on the outcome
//! without parsing prose.

use std::fmt;

/// Logical error categories required by the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ErrorClass {
    UsageError,
    RepositoryError,
    SessionError,
    NetworkError,
    ProtocolError,
    ConflictError,
    GitError,
    IntegrityError,
    UnsupportedError,
    PersistenceError,
}

impl ErrorClass {
    /// Stable process exit code for this class.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorClass::UsageError => 2,
            ErrorClass::RepositoryError => 3,
            ErrorClass::SessionError => 4,
            ErrorClass::NetworkError => 5,
            ErrorClass::ProtocolError => 6,
            ErrorClass::ConflictError => 7,
            ErrorClass::GitError => 8,
            ErrorClass::IntegrityError => 9,
            ErrorClass::UnsupportedError => 10,
            ErrorClass::PersistenceError => 11,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::UsageError => "UsageError",
            ErrorClass::RepositoryError => "RepositoryError",
            ErrorClass::SessionError => "SessionError",
            ErrorClass::NetworkError => "NetworkError",
            ErrorClass::ProtocolError => "ProtocolError",
            ErrorClass::ConflictError => "ConflictError",
            ErrorClass::GitError => "GitError",
            ErrorClass::IntegrityError => "IntegrityError",
            ErrorClass::UnsupportedError => "UnsupportedError",
            ErrorClass::PersistenceError => "PersistenceError",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Weave error: a class, a headline, and optional actionable detail.
///
/// Specification section 156 requires errors to be actionable, so `detail`
/// carries the "what to do about it" text rather than an opaque code.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WeaveError {
    pub class: ErrorClass,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl WeaveError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        WeaveError {
            class,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn exit_code(&self) -> i32 {
        self.class.exit_code()
    }
}

impl fmt::Display for WeaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, "\n\n{detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WeaveError {}

pub type Result<T> = std::result::Result<T, WeaveError>;

macro_rules! ctor {
    ($name:ident, $class:ident) => {
        pub fn $name(message: impl Into<String>) -> WeaveError {
            WeaveError::new(ErrorClass::$class, message)
        }
    };
}

ctor!(usage, UsageError);
ctor!(repository, RepositoryError);
ctor!(session, SessionError);
ctor!(network, NetworkError);
ctor!(protocol, ProtocolError);
ctor!(conflict, ConflictError);
ctor!(git, GitError);
ctor!(integrity, IntegrityError);
ctor!(unsupported, UnsupportedError);
ctor!(persistence, PersistenceError);

impl From<rusqlite::Error> for WeaveError {
    fn from(e: rusqlite::Error) -> Self {
        persistence(format!("Weave storage error: {e}"))
    }
}

impl From<serde_json::Error> for WeaveError {
    fn from(e: serde_json::Error) -> Self {
        protocol(format!("Malformed Weave message: {e}"))
    }
}

impl From<std::io::Error> for WeaveError {
    fn from(e: std::io::Error) -> Self {
        persistence(format!("Filesystem error: {e}"))
    }
}
