//! The feeder's state machine.
//!
//! ```text
//! Idle  ──/join──▶  Joined  ──/play──▶  Playing
//!   ▲                  │                    │
//!   │                  │                    │
//!   └───/leave─────────┴────/stop (or EOF)──┘
//! ```
//!
//! Transitions are exhaustive `match`es on [`FeederState`]. Data that
//! belongs to a specific state (the `Call` handle, the current `TrackHandle`,
//! the guild id) lives *inside* the variant, not in a soup of
//! `Option<Arc<Mutex<_>>>` fields alongside the state.
//!
//! The outer lock is a `tokio::sync::Mutex` because every transition awaits
//! at least once (`Songbird::join`, `Call::play_input`, `Songbird::leave`).
//! Holding a std mutex across those awaits would not compile; holding a
//! tokio mutex across them is fine and preserves the invariant that only
//! one transition runs at a time. Individual control calls complete in
//! well under a second, so lock contention between simultaneous test
//! scripts is not a concern.
use std::path::PathBuf;
use std::sync::Arc;

use serenity::model::id::GuildId;
use songbird::tracks::TrackHandle;
use songbird::{Call, Songbird};
use tokio::sync::Mutex as TokioMutex;

/// Data that persists for the lifetime of the process, independent of state:
/// the feeder's name, its audio file path, its serenity user id, and the
/// Songbird manager handle (set once on the `ready` event).
///
/// The state machine itself is behind a `TokioMutex<FeederState>`.
pub struct AppState {
    pub name: String,
    pub audio_file: PathBuf,
    /// Feeder's own Discord user id. Populated in the `ready` handler.
    pub self_user_id: TokioMutex<Option<u64>>,
    /// Songbird manager. Populated in the `ready` handler (`songbird::get`).
    pub songbird: TokioMutex<Option<Arc<Songbird>>>,
    /// The state machine. All valid operations go through a transition on
    /// this field; see the handler implementations in `control`.
    pub state: TokioMutex<FeederState>,
}

impl AppState {
    pub fn new(name: String, audio_file: PathBuf) -> Self {
        Self {
            name,
            audio_file,
            self_user_id: TokioMutex::new(None),
            songbird: TokioMutex::new(None),
            state: TokioMutex::new(FeederState::Idle),
        }
    }

    /// Snapshot the feeder's current status for `GET /health`. Cheap — one
    /// lock acquisition per field. Does not transition state.
    pub async fn snapshot(&self) -> HealthSnapshot {
        let (in_voice, playing) = {
            let st = self.state.lock().await;
            (st.is_in_voice(), st.is_playing())
        };
        HealthSnapshot {
            name: self.name.clone(),
            user_id: *self.self_user_id.lock().await,
            in_voice,
            playing,
        }
    }
}

/// Read-only snapshot of the feeder's state for the /health endpoint.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub name: String,
    pub user_id: Option<u64>,
    pub in_voice: bool,
    pub playing: bool,
}

/// The state machine. Variants own the handles that are only meaningful in
/// that state — there is no way to get a `TrackHandle` out of `Idle`, by
/// construction.
///
/// Default is `Idle` for convenient `std::mem::take` transitions.
#[derive(Default)]
pub enum FeederState {
    #[default]
    Idle,
    Joined {
        guild: GuildId,
        call: Arc<TokioMutex<Call>>,
    },
    Playing {
        guild: GuildId,
        call: Arc<TokioMutex<Call>>,
        track: TrackHandle,
    },
}

impl FeederState {
    pub fn is_in_voice(&self) -> bool {
        matches!(self, Self::Joined { .. } | Self::Playing { .. })
    }

    pub fn is_playing(&self) -> bool {
        matches!(self, Self::Playing { .. })
    }

    /// Short label for logs / tests.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Joined { .. } => "joined",
            Self::Playing { .. } => "playing",
        }
    }
}

/// What an endpoint decided to do with the state, independent of side
/// effects. The actual side effects (starting a track, leaving the call)
/// run in the handler after the decision.
///
/// This exists so the transition *policy* — which states accept which
/// events, which return which error — can be unit-tested without spinning
/// up a real songbird `Call`.
#[derive(Debug)]
pub enum TransitionDecision {
    /// The event is valid in the current state. Handler proceeds with side
    /// effects and then writes the target state.
    Proceed,
    /// The event is a no-op in the current state per spec (e.g. /stop when
    /// idle). Handler returns 200 without side effects.
    Noop,
    /// The event is invalid in the current state. Handler returns the
    /// mapped error.
    Reject(crate::error::FeederError),
}

/// Control events from the HTTP layer. Used for pure transition tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Join,
    Play,
    Stop,
    Leave,
}

impl FeederState {
    /// Pure transition policy: given a state and an event, decide whether
    /// the event is valid. No I/O, no mutation.
    pub fn decide(&self, event: Event) -> TransitionDecision {
        use crate::error::FeederError as E;
        match (self, event) {
            // /join
            (Self::Idle, Event::Join) => TransitionDecision::Proceed,
            (Self::Joined { .. } | Self::Playing { .. }, Event::Join) => {
                TransitionDecision::Reject(E::AlreadyJoined)
            }
            // /play
            (Self::Idle, Event::Play) => TransitionDecision::Reject(E::NotInVoice),
            (Self::Joined { .. }, Event::Play) => TransitionDecision::Proceed,
            (Self::Playing { .. }, Event::Play) => TransitionDecision::Reject(E::AlreadyPlaying),
            // /stop
            (Self::Idle | Self::Joined { .. }, Event::Stop) => TransitionDecision::Noop,
            (Self::Playing { .. }, Event::Stop) => TransitionDecision::Proceed,
            // /leave
            (Self::Idle, Event::Leave) => TransitionDecision::Noop,
            (Self::Joined { .. } | Self::Playing { .. }, Event::Leave) => {
                TransitionDecision::Proceed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the pure transition policy. These don't touch songbird;
    //! they only verify that the `match` arms in `decide` encode the
    //! behaviour the spec requires. The handlers in `control.rs` run the
    //! same policy plus side effects — the side effects (songbird calls)
    //! are exercised by integration tests under the real dev stack, not
    //! here.
    use super::*;
    use crate::error::FeederError;
    use serenity::model::id::GuildId;
    use songbird::Call;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    fn idle() -> FeederState {
        FeederState::Idle
    }

    fn joined() -> FeederState {
        // standalone() builds a Call without any gateway connection — it's
        // unusable for real voice ops, but holds the type for state
        // assertions.
        let guild = GuildId::new(1);
        let call = Arc::new(TokioMutex::new(Call::standalone(
            guild,
            serenity::model::id::UserId::new(1),
        )));
        FeederState::Joined { guild, call }
    }

    /// Build a Playing state by asking a standalone Call to play a dummy
    /// track. The track never actually plays audio (no voice connection),
    /// but `play_input` on a standalone Call does return a valid
    /// TrackHandle — enough to exercise the Playing variant's match arms.
    async fn playing() -> FeederState {
        use songbird::input::File as SongbirdFile;
        let guild = GuildId::new(1);
        let call = Arc::new(TokioMutex::new(Call::standalone(
            guild,
            serenity::model::id::UserId::new(1),
        )));
        // Path doesn't need to exist — songbird's File input is lazy; the
        // handle is produced immediately.
        let track = {
            let mut handler = call.lock().await;
            handler.play_input(SongbirdFile::new("/nonexistent.ogg").into())
        };
        FeederState::Playing { guild, call, track }
    }

    // `Call::standalone` internally constructs a songbird `Driver`, which
    // spawns tasks into the current tokio runtime. Hence every test that
    // touches `joined()` must run under `#[tokio::test]`; `#[test]` alone
    // panics with "there is no reactor running".

    #[test]
    fn join_from_idle_proceeds() {
        assert!(matches!(
            idle().decide(Event::Join),
            TransitionDecision::Proceed
        ));
    }

    #[tokio::test]
    async fn join_from_joined_rejects_as_already_joined() {
        let d = joined().decide(Event::Join);
        assert!(matches!(
            d,
            TransitionDecision::Reject(FeederError::AlreadyJoined)
        ));
    }

    #[test]
    fn play_from_idle_rejects_as_not_in_voice() {
        let d = idle().decide(Event::Play);
        assert!(matches!(
            d,
            TransitionDecision::Reject(FeederError::NotInVoice)
        ));
    }

    #[tokio::test]
    async fn play_from_joined_proceeds() {
        assert!(matches!(
            joined().decide(Event::Play),
            TransitionDecision::Proceed
        ));
    }

    #[tokio::test]
    async fn double_play_from_playing_rejects_as_already_playing() {
        let d = playing().await.decide(Event::Play);
        assert!(matches!(
            d,
            TransitionDecision::Reject(FeederError::AlreadyPlaying)
        ));
    }

    #[tokio::test]
    async fn join_from_playing_rejects_as_already_joined() {
        let d = playing().await.decide(Event::Join);
        assert!(matches!(
            d,
            TransitionDecision::Reject(FeederError::AlreadyJoined)
        ));
    }

    #[tokio::test]
    async fn stop_from_playing_proceeds() {
        let d = playing().await.decide(Event::Stop);
        assert!(matches!(d, TransitionDecision::Proceed));
    }

    #[tokio::test]
    async fn leave_from_playing_proceeds() {
        let d = playing().await.decide(Event::Leave);
        assert!(matches!(d, TransitionDecision::Proceed));
    }

    #[tokio::test]
    async fn is_in_voice_and_playing_in_playing_state() {
        let p = playing().await;
        assert!(p.is_in_voice());
        assert!(p.is_playing());
        assert_eq!(p.name(), "playing");
    }

    #[tokio::test]
    async fn stop_is_noop_in_idle_and_joined() {
        assert!(matches!(
            idle().decide(Event::Stop),
            TransitionDecision::Noop
        ));
        assert!(matches!(
            joined().decide(Event::Stop),
            TransitionDecision::Noop
        ));
    }

    #[test]
    fn leave_is_noop_in_idle() {
        assert!(matches!(
            idle().decide(Event::Leave),
            TransitionDecision::Noop
        ));
    }

    #[tokio::test]
    async fn leave_from_joined_proceeds() {
        assert!(matches!(
            joined().decide(Event::Leave),
            TransitionDecision::Proceed
        ));
    }

    #[tokio::test]
    async fn is_in_voice_matches_variant() {
        assert!(!idle().is_in_voice());
        assert!(joined().is_in_voice());
    }

    #[test]
    fn is_playing_matches_idle() {
        assert!(!idle().is_playing());
    }

    #[tokio::test]
    async fn is_playing_false_in_joined() {
        assert!(!joined().is_playing());
    }

    #[tokio::test]
    async fn state_names() {
        assert_eq!(idle().name(), "idle");
        assert_eq!(joined().name(), "joined");
    }
}
