//! Chronicle-feeder binary entry point.
//!
//! Responsibilities (and only these):
//!   1. Load env vars and crash loudly if required ones are missing.
//!   2. Install rustls + tracing.
//!   3. Sanity-check the `AUDIO_FILE` (WARN only, never fails startup
//!      unless the file is outright absent — see [`chronicle_feeder::audio`]).
//!   4. Construct the serenity client with songbird registered.
//!   5. Bind the axum control server.
//!   6. Run both concurrently; exit on either's termination.
//!
//! Everything else — the state machine, error rendering, control handlers
//! — lives in the library crate (`src/lib.rs`).
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use serenity::all::{Client, Context, EventHandler, GatewayIntents, Ready};
use serenity::async_trait;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::serenity::register_from_config;
use songbird::Config as SongbirdConfig;
use tracing::{error, info, info_span, Instrument};

use chronicle_feeder::{AppState, check_audio_file, require_audio_file_exists, router};

struct Handler {
    state: Arc<AppState>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let span = info_span!("voice", phase = "ready");
        async move {
            info!(
                feeder = %self.state.name,
                user = %ready.user.name,
                user_id = %ready.user.id,
                "bot_ready"
            );
            *self.state.self_user_id.lock().await = Some(ready.user.id.get());
            let Some(manager) = songbird::get(&ctx).await else {
                // register_from_config was called, so this should never
                // happen — but if it does the feeder is useless, log and
                // carry on (the process will stay up but /join will 500
                // with NotReady, which is the right signal to the harness).
                error!("songbird manager missing from TypeMap at ready time");
                return;
            };
            *self.state.songbird.lock().await = Some(manager);
            info!(feeder = %self.state.name, "songbird_attached");
        }
        .instrument(span)
        .await
    }
}

/// Load and validate env vars. Panics with a clear message on any failure
/// the caller needs to notice.
struct EnvConfig {
    token: String,
    name: String,
    audio_file: PathBuf,
    control_port: u16,
    control_bind: IpAddr,
}

impl EnvConfig {
    fn from_env() -> Self {
        let token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
        let name = std::env::var("FEEDER_NAME").unwrap_or_else(|_| "feeder".to_string());
        let audio_file = PathBuf::from(
            std::env::var("AUDIO_FILE")
                .expect("AUDIO_FILE not set — path to OGG Opus file to play"),
        );
        require_audio_file_exists(&audio_file);
        let control_port: u16 = std::env::var("CONTROL_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8003);
        // CONTROL_BIND defaults to loopback. In Docker the compose file
        // sets it to 0.0.0.0; host safety is enforced by the port mapping
        // (127.0.0.1:<port>:<port>), not the in-container bind address.
        let control_bind: IpAddr = std::env::var("CONTROL_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(IpAddr::from([127, 0, 0, 1]));
        Self {
            token,
            name,
            audio_file,
            control_port,
            control_bind,
        }
    }
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,serenity=warn,songbird=warn")
            }),
        )
        .init();

    let cfg = EnvConfig::from_env();

    // Classify the audio file up-front. WARN-only — songbird will still
    // attempt playback even for non-ideal inputs, it just won't use the
    // passthrough fast path. See `audio::check_audio_file` for rationale.
    check_audio_file(&cfg.audio_file);

    let state = Arc::new(AppState::new(cfg.name.clone(), cfg.audio_file));

    let intents = GatewayIntents::GUILD_VOICE_STATES;
    // Decode mode is set to `Decode` so songbird still works as a fallback
    // when the input isn't OGG Opus @ 48kHz. For the passthrough path it's
    // a no-op; songbird short-circuits before reaching the decoder.
    let songbird_config =
        SongbirdConfig::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()));

    let handler = Handler {
        state: state.clone(),
    };
    let client_builder = Client::builder(&cfg.token, intents).event_handler(handler);
    let mut client = register_from_config(client_builder, songbird_config)
        .await
        .expect("failed to build serenity client");

    let addr = SocketAddr::new(cfg.control_bind, cfg.control_port);
    info!(feeder = %cfg.name, %addr, "control_server_listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind control port failed");

    let app = router(state);

    // Run serenity and axum concurrently. If either exits, the process
    // dies so the container restarts — keeps the test harness honest.
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
