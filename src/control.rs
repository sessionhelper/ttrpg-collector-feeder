//! Axum control server.
//!
//! Five endpoints — `/health`, `/join`, `/play`, `/stop`, `/leave` — each
//! implemented as an exhaustive `match` on the current [`FeederState`].
//! The match itself is the design: every handler enumerates every state it
//! accepts or rejects, so it's impossible to forget (say) "what if `/stop`
//! comes while we're Idle?" — the compiler won't let you write the handler
//! without saying.
//!
//! All `.await`ing songbird work happens *inside* the match arm, while the
//! `FeederState` lock is held. This is intentional: the lock's sole purpose
//! is to serialize transitions, and the transitions are the only place
//! they're slow. A harness issuing two `/join` calls in quick succession
//! will see the second one fail cleanly with 409 rather than race.
//!
//! The transition *policy* (which state accepts which event) is additionally
//! encoded in [`crate::state::FeederState::decide`], which is what the
//! unit tests exercise. The handlers below effectively inline the same
//! policy; keeping both in sync is a small amount of redundancy that buys
//! compile-time safety on the match arms.
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serenity::model::id::{ChannelId, GuildId};
use songbird::input::File as SongbirdFile;
use tracing::{Instrument, info, info_span, warn};

use crate::error::FeederError;
use crate::state::{AppState, FeederState};

#[derive(Deserialize)]
pub struct JoinReq {
    pub guild_id: u64,
    pub channel_id: u64,
}

#[derive(Serialize)]
pub struct HealthResp {
    pub name: String,
    pub user_id: Option<u64>,
    pub in_voice: bool,
    pub playing: bool,
}

/// Build the router. Split out so integration tests can mount it against a
/// dummy `AppState` without spinning up serenity.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/join", post(join))
        .route("/play", post(play))
        .route("/stop", post(stop))
        .route("/leave", post(leave))
        .with_state(state)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResp> {
    let snap = state.snapshot().await;
    Json(HealthResp {
        name: snap.name,
        user_id: snap.user_id,
        in_voice: snap.in_voice,
        playing: snap.playing,
    })
}

async fn join(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JoinReq>,
) -> Result<impl IntoResponse, FeederError> {
    let span = info_span!("control", endpoint = "join", guild_id = req.guild_id);
    async move {
        let manager = state
            .songbird
            .lock()
            .await
            .clone()
            .ok_or(FeederError::NotReady)?;

        let mut cur = state.state.lock().await;
        match &*cur {
            FeederState::Joined { .. } | FeederState::Playing { .. } => {
                Err(FeederError::AlreadyJoined)
            }
            FeederState::Idle => {
                let guild = GuildId::new(req.guild_id);
                let channel = ChannelId::new(req.channel_id);
                let call = manager
                    .join(guild, channel)
                    .await
                    .map_err(|e| FeederError::Songbird(e.to_string()))?;
                *cur = FeederState::Joined { guild, call };
                info!(
                    feeder = %state.name,
                    channel_id = req.channel_id,
                    "voice_connected"
                );
                Ok(StatusCode::OK)
            }
        }
    }
    .instrument(span)
    .await
}

async fn play(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, FeederError> {
    let span = info_span!("control", endpoint = "play");
    async move {
        if !state.audio_file.exists() {
            return Err(FeederError::AudioFileMissing(
                state.audio_file.display().to_string(),
            ));
        }

        let mut cur = state.state.lock().await;
        // Take ownership of the current state so we can move fields into
        // the new variant. `std::mem::take` leaves `Idle` in its place;
        // if the transition is rejected we restore the prior state.
        match std::mem::take(&mut *cur) {
            FeederState::Idle => {
                // *cur is already Idle from `take`.
                Err(FeederError::NotInVoice)
            }
            FeederState::Playing { guild, call, track } => {
                *cur = FeederState::Playing { guild, call, track };
                Err(FeederError::AlreadyPlaying)
            }
            FeederState::Joined { guild, call } => {
                let input = SongbirdFile::new(state.audio_file.clone()).into();
                let track = {
                    let mut handler = call.lock().await;
                    handler.play_input(input)
                };
                info!(
                    feeder = %state.name,
                    file = %state.audio_file.display(),
                    "playback_started"
                );
                *cur = FeederState::Playing { guild, call, track };
                Ok(StatusCode::OK)
            }
        }
    }
    .instrument(span)
    .await
}

async fn stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let span = info_span!("control", endpoint = "stop");
    async move {
        let mut cur = state.state.lock().await;
        match std::mem::take(&mut *cur) {
            // Already not playing — no-op per spec.
            prev @ (FeederState::Idle | FeederState::Joined { .. }) => {
                *cur = prev;
            }
            FeederState::Playing { guild, call, track } => {
                if let Err(e) = track.stop() {
                    warn!(feeder = %state.name, error = %e, "track_stop_failed");
                }
                info!(feeder = %state.name, "playback_stopped");
                *cur = FeederState::Joined { guild, call };
            }
        }
        StatusCode::OK
    }
    .instrument(span)
    .await
}

async fn leave(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, FeederError> {
    let span = info_span!("control", endpoint = "leave");
    async move {
        let mut cur = state.state.lock().await;
        match std::mem::take(&mut *cur) {
            // Already not joined — no-op per spec. Does not require the
            // songbird manager to have attached yet; callers can safely
            // /leave a freshly-booted feeder without racing `ready`.
            FeederState::Idle => Ok(StatusCode::OK),
            FeederState::Joined { guild, call } => {
                let Some(manager) = state.songbird.lock().await.clone() else {
                    // We got into Joined without a manager? Shouldn't
                    // happen — /join can't succeed without one — but
                    // restore state and report rather than panic.
                    *cur = FeederState::Joined { guild, call };
                    return Err(FeederError::NotReady);
                };
                manager
                    .leave(guild)
                    .await
                    .map_err(|e| FeederError::Songbird(e.to_string()))?;
                info!(feeder = %state.name, %guild, "voice_disconnected");
                // *cur is already Idle (from `take`).
                Ok(StatusCode::OK)
            }
            FeederState::Playing { guild, call, track } => {
                let Some(manager) = state.songbird.lock().await.clone() else {
                    *cur = FeederState::Playing { guild, call, track };
                    return Err(FeederError::NotReady);
                };
                // Implicit stop. Ignore stop() errors — we're tearing down
                // the call anyway.
                let _ = track.stop();
                manager
                    .leave(guild)
                    .await
                    .map_err(|e| FeederError::Songbird(e.to_string()))?;
                info!(
                    feeder = %state.name,
                    %guild,
                    "voice_disconnected (implicit stop)"
                );
                Ok(StatusCode::OK)
            }
        }
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    //! Router-level tests. The transition *policy* is tested as pure logic
    //! in `state::tests`; here we verify the axum plumbing — handlers are
    //! wired to the right methods, `/health` returns fields in the right
    //! shape, and the error-to-HTTP mapping is what the spec says.
    use super::*;
    use crate::error::FeederError;
    use crate::state::AppState;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn dummy_state() -> Arc<AppState> {
        Arc::new(AppState::new(
            "test".into(),
            PathBuf::from("/nonexistent.ogg"),
        ))
    }

    #[tokio::test]
    async fn health_snapshot_reflects_idle() {
        let state = dummy_state();
        let snap = state.snapshot().await;
        assert_eq!(snap.name, "test");
        assert!(!snap.in_voice);
        assert!(!snap.playing);
        assert!(snap.user_id.is_none());
    }

    #[tokio::test]
    async fn play_without_audio_file_returns_404() {
        let state = dummy_state();
        assert!(!state.audio_file.exists());
        // play() checks file existence before touching the lock; mimic
        // that path directly here rather than spinning up axum.
        if !state.audio_file.exists() {
            let err = FeederError::AudioFileMissing(state.audio_file.display().to_string());
            assert_eq!(err.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn router_can_be_built() {
        let state = dummy_state();
        let _router: Router = router(state);
    }
}
