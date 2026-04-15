//! Chronicle-feeder: a dev-only Discord bot that joins a voice channel and
//! plays a pre-encoded OGG Opus file on demand. Four identical containers
//! (moe / larry / curly / gygax) run in the dev compose stack and are driven
//! by external test scripts through a loopback HTTP control API.
//!
//! The module is deliberately small:
//!
//! * [`state`]   — the `FeederState` state machine (Idle → Joined → Playing).
//! * [`error`]   — the `FeederError` enum with `IntoResponse` mapping.
//! * [`control`] — axum handlers, one per endpoint.
//! * [`audio`]   — startup-time OGG Opus sanity check.
//!
//! Everything async-aware lives behind a single `tokio::sync::Mutex<FeederState>`
//! on [`state::AppState`]; handlers take the lock, `match` on the current
//! state, perform at most one `.await`-bearing transition, then drop the
//! guard. There are no background tasks in this process other than the
//! serenity gateway loop and axum request tasks.

pub mod audio;
pub mod control;
pub mod error;
pub mod state;

pub use audio::{AudioFormat, check_audio_file, require_audio_file_exists};
pub use control::router;
pub use error::FeederError;
pub use state::{AppState, FeederState};
