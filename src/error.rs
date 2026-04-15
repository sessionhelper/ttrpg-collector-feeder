//! User-facing errors from the control API. Exactly four kinds, each of
//! which maps to a fixed status code + JSON body. Keeping this enum small and
//! closed is deliberate: the control API has a narrow, well-defined surface
//! and errors need to be predictable for the test harness.
//!
//! The `IntoResponse` impl is the only thing that renders an error into an
//! HTTP response; handlers return `Result<_, FeederError>` and axum does the
//! rest.
//!
//! Internal, process-level errors (e.g. "songbird manager missing from
//! TypeMap") do not belong here — they should surface as `warn!`/`error!`
//! logs or startup panics, not as HTTP responses.
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

/// Every error the control API is allowed to return to the caller.
///
/// The variants encode both the `StatusCode` and the message body; the
/// `IntoResponse` impl is a simple match with no fallible logic.
#[derive(Debug, Error)]
pub enum FeederError {
    /// `/play` called before `/join`, or after the call handle was lost.
    #[error("not in voice channel")]
    NotInVoice,
    /// `/play` called while a track is already playing.
    #[error("already playing")]
    AlreadyPlaying,
    /// `/join` called while already in a voice channel.
    #[error("already joined; leave first")]
    AlreadyJoined,
    /// `AUDIO_FILE` does not exist on disk at `/play` time.
    #[error("audio file missing: {0}")]
    AudioFileMissing(String),
    /// Songbird's internal `join` / `leave` call rejected the request (e.g.
    /// gateway not yet connected, permission denied on the channel). Surfaces
    /// as 500 because the caller generally can't do anything better than log
    /// it — the control API is loopback-only and leaks the message body for
    /// ease of debugging.
    #[error("songbird error: {0}")]
    Songbird(String),
    /// Serenity has not called `ready` yet, so we have no `Songbird` manager.
    /// Rare — usually means the test runner started issuing calls too early.
    #[error("songbird not ready")]
    NotReady,
}

impl FeederError {
    /// Map the variant to its HTTP status. Kept as a method (not baked into
    /// `IntoResponse`) so tests can assert on the status without spinning up
    /// a router.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotInVoice => StatusCode::BAD_REQUEST,
            Self::AlreadyPlaying | Self::AlreadyJoined => StatusCode::CONFLICT,
            Self::AudioFileMissing(_) => StatusCode::NOT_FOUND,
            Self::Songbird(_) | Self::NotReady => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Serialize)]
struct ErrBody<'a> {
    error: &'a str,
}

impl IntoResponse for FeederError {
    fn into_response(self) -> Response {
        let status = self.status();
        let msg = self.to_string();
        (status, Json(ErrBody { error: &msg })).into_response()
    }
}
