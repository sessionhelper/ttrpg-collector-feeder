//! Feeder bot — part of the E2E test harness for ttrpg-collector.
//!
//! A minimal Discord bot that joins a voice channel and plays a pre-recorded
//! WAV file on demand. Used in E2E tests to simulate real participants talking
//! into a voice channel while the collector records them.
//!
//! Dev-only. Four identical containers ("moe", "larry", "curly", "gygax") run
//! against the dev stack; see `infra/dev-compose.yml` in `sessionhelper-hub`.
//!
//! Control surface: a tiny axum server bound to 127.0.0.1 exposes /health,
//! /join, /play, /stop, /leave. An external test runner drives these to
//! orchestrate multi-bot scenarios.
//!
//! Env:
//!   DISCORD_TOKEN  — bot token
//!   FEEDER_NAME    — short name for logs (e.g. "moe")
//!   AUDIO_FILE     — absolute path to the WAV to play on /play
//!   CONTROL_PORT   — loopback port for the control server (default 8003)

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serenity::all::*;
use serenity::async_trait;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::input::File as SongbirdFile;
use songbird::serenity::register_from_config;
use songbird::tracks::TrackHandle;
use songbird::{Call, Config as SongbirdConfig, Songbird};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Shared state between the serenity client and the axum control server.
/// The serenity client populates `songbird` + `self_user_id` on ready; the
/// axum routes read them to drive voice operations.
struct AppState {
    name: String,
    audio_file: PathBuf,
    songbird: Mutex<Option<Arc<Songbird>>>,
    self_user_id: Mutex<Option<u64>>,
    // Current Call handle and most recent TrackHandle, so /stop + /leave can
    // tear down cleanly. We hold the call Arc (not a lock on it) so we can
    // re-acquire the songbird mutex on each control call.
    current_call: Mutex<Option<Arc<tokio::sync::Mutex<Call>>>>,
    current_track: Mutex<Option<TrackHandle>>,
    current_guild: Mutex<Option<GuildId>>,
    /// Looping silence track, started on /join and held until /leave.
    /// Keeps the bot in the "speaking" state so other participants'
    /// songbird instances see our SSRC in VoiceTick events and complete
    /// their DAVE handshakes. Without this, bot-only voice channels
    /// have no voice traffic, and the collector's DAVE check times out.
    silence_track: Mutex<Option<TrackHandle>>,
    /// Path to a generated 1-second silence WAV. Created once at startup,
    /// reused on every /join. Lives in /tmp because the feeder container
    /// has a writable /tmp even though the root FS is read-only.
    silence_wav: PathBuf,
}

impl AppState {
    fn new(name: String, audio_file: PathBuf) -> Self {
        let silence_wav = PathBuf::from("/tmp/silence.wav");
        generate_silence_wav(&silence_wav).expect("failed to generate silence WAV");
        Self {
            name,
            audio_file,
            songbird: Mutex::new(None),
            self_user_id: Mutex::new(None),
            current_call: Mutex::new(None),
            current_track: Mutex::new(None),
            current_guild: Mutex::new(None),
            silence_track: Mutex::new(None),
            silence_wav,
        }
    }
}

/// Write a 1-second silent WAV to disk. 48kHz, stereo, 16-bit PCM — matches
/// Discord's native voice format so songbird doesn't need to resample.
fn generate_silence_wav(path: &std::path::Path) -> std::io::Result<()> {
    let sample_rate: u32 = 48_000;
    let channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let num_samples = sample_rate; // 1 second
    let data_bytes = num_samples * (channels as u32) * ((bits_per_sample / 8) as u32);
    let byte_rate = sample_rate * (channels as u32) * ((bits_per_sample / 8) as u32);
    let block_align = channels * (bits_per_sample / 8);

    let mut f = std::fs::File::create(path)?;
    // RIFF header
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    // fmt chunk
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // chunk size
    f.write_all(&1u16.to_le_bytes())?; // PCM format
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits_per_sample.to_le_bytes())?;
    // data chunk
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    // 192000 bytes of silence (zeros)
    let zeros = vec![0u8; data_bytes as usize];
    f.write_all(&zeros)?;
    Ok(())
}

struct Handler {
    state: Arc<AppState>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(
            feeder = %self.state.name,
            user = %ready.user.name,
            user_id = %ready.user.id,
            "bot_ready"
        );
        *self.state.self_user_id.lock().await = Some(ready.user.id.get());
        // Pull the Songbird manager out of the TypeMap once at ready time.
        let manager = songbird::get(&ctx)
            .await
            .expect("songbird not registered on client");
        *self.state.songbird.lock().await = Some(manager);
    }
}

#[derive(Deserialize)]
struct JoinReq {
    guild_id: u64,
    channel_id: u64,
}

#[derive(Serialize)]
struct HealthResp {
    name: String,
    user_id: Option<u64>,
    in_voice: bool,
    playing: bool,
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResp> {
    let user_id = *state.self_user_id.lock().await;
    let in_voice = state.current_call.lock().await.is_some();
    let playing = state.current_track.lock().await.is_some();
    Json(HealthResp {
        name: state.name.clone(),
        user_id,
        in_voice,
        playing,
    })
}

/// Turn any error into a 500 with a JSON message. The harness is local-only
/// so leaking error strings to the caller is fine and useful for debugging.
fn err500<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn join(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JoinReq>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let manager = state
        .songbird
        .lock()
        .await
        .clone()
        .ok_or_else(|| err500("songbird not ready"))?;
    let guild = GuildId::new(req.guild_id);
    let channel = ChannelId::new(req.channel_id);
    let call = manager.join(guild, channel).await.map_err(err500)?;
    *state.current_call.lock().await = Some(call.clone());
    *state.current_guild.lock().await = Some(guild);

    // Start a looping silence track immediately so the feeder is
    // "speaking" from the collector's perspective. Without this, the
    // collector's DAVE handshake times out in bot-only channels because
    // VoiceTick.speaking is empty when nobody is producing voice frames.
    let silence_input = SongbirdFile::new(state.silence_wav.clone()).into();
    let silence_handle = {
        let mut handler = call.lock().await;
        let handle = handler.play_input(silence_input);
        let _ = handle.enable_loop();
        handle
    };
    *state.silence_track.lock().await = Some(silence_handle);

    info!(feeder = %state.name, guild_id = req.guild_id, channel_id = req.channel_id, "joined_voice (silence loop active)");
    Ok(StatusCode::OK)
}

async fn play(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let call = state
        .current_call
        .lock()
        .await
        .clone()
        .ok_or_else(|| err500("not in a voice channel"))?;
    if !state.audio_file.exists() {
        return Err(err500(format!(
            "audio file missing: {}",
            state.audio_file.display()
        )));
    }
    let input = SongbirdFile::new(state.audio_file.clone()).into();
    let track = {
        let mut handler = call.lock().await;
        handler.play_input(input)
    };
    *state.current_track.lock().await = Some(track);
    info!(feeder = %state.name, file = %state.audio_file.display(), "playing");
    Ok(StatusCode::OK)
}

async fn stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if let Some(track) = state.current_track.lock().await.take()
        && let Err(e) = track.stop()
    {
        warn!(feeder = %state.name, error = %e, "track_stop_failed");
    }
    info!(feeder = %state.name, "stopped");
    StatusCode::OK
}

async fn leave(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Clear both tracks — the voice connection is about to go away.
    if let Some(silence) = state.silence_track.lock().await.take() {
        let _ = silence.stop();
    }
    *state.current_track.lock().await = None;
    let manager = state
        .songbird
        .lock()
        .await
        .clone()
        .ok_or_else(|| err500("songbird not ready"))?;
    let guild = state
        .current_guild
        .lock()
        .await
        .take()
        .ok_or_else(|| err500("not in a guild"))?;
    manager.leave(guild).await.map_err(err500)?;
    *state.current_call.lock().await = None;
    info!(feeder = %state.name, guild_id = %guild, "left_voice");
    Ok(StatusCode::OK)
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,serenity=warn,songbird=warn")
            }),
        )
        .init();

    let token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let name = std::env::var("FEEDER_NAME").unwrap_or_else(|_| "feeder".to_string());
    let audio_file = PathBuf::from(
        std::env::var("AUDIO_FILE").expect("AUDIO_FILE not set — path to WAV to play"),
    );
    if !audio_file.exists() {
        panic!("AUDIO_FILE does not exist: {}", audio_file.display());
    }
    let control_port: u16 = std::env::var("CONTROL_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8003);
    // CONTROL_BIND defaults to loopback for safe local dev runs. In Docker
    // the compose file sets it to 0.0.0.0 so the container-side listener is
    // reachable from the host — host safety is enforced by the port mapping
    // (127.0.0.1:<port>:<port>), not by the in-container bind address.
    let control_bind: std::net::IpAddr = std::env::var("CONTROL_BIND")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));

    let state = Arc::new(AppState::new(name.clone(), audio_file));

    let intents = GatewayIntents::GUILD_VOICE_STATES;
    let songbird_config =
        SongbirdConfig::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));

    let handler = Handler {
        state: state.clone(),
    };
    let client_builder = Client::builder(&token, intents).event_handler(handler);
    let mut client = register_from_config(client_builder, songbird_config)
        .await
        .expect("failed to build serenity client");

    let app = Router::new()
        .route("/health", get(health))
        .route("/join", post(join))
        .route("/play", post(play))
        .route("/stop", post(stop))
        .route("/leave", post(leave))
        .with_state(state.clone());

    let addr = SocketAddr::new(control_bind, control_port);
    info!(feeder = %name, %addr, "control_server_listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind control port failed");

    // Run serenity and axum concurrently. If either exits, the process dies
    // so the container restarts — keeps the test harness honest.
    tokio::select! {
        res = client.start() => {
            if let Err(e) = res {
                error!(error = %e, "serenity_client_exited");
            }
        }
        res = axum::serve(listener, app) => {
            if let Err(e) = res {
                error!(error = %e, "axum_server_exited");
            }
        }
    }
}
