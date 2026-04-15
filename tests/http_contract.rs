//! End-to-end test of the axum router without a live Discord/songbird.
//!
//! The router is mounted against an `AppState` that never has its
//! `songbird` field populated — i.e. we simulate the short window between
//! process boot and `ready`. This is enough to exercise:
//!
//!   * `GET /health` → 200 with the right JSON shape
//!   * `POST /play` before `/join` → 400 `not in voice channel`
//!     (when `AUDIO_FILE` exists)
//!   * `POST /play` with a missing `AUDIO_FILE` → 404
//!   * `POST /stop` in Idle → 200 (no-op)
//!   * `POST /leave` in Idle → 500 (NotReady — songbird never attached)
//!
//! `POST /join` and the Joined/Playing transitions need a real songbird
//! manager; they're covered by the dev-compose E2E harness, not here.
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use chronicle_feeder::{AppState, router};
use tower::util::ServiceExt;

fn state_with_no_audio_file() -> Arc<AppState> {
    Arc::new(AppState::new(
        "moe".into(),
        PathBuf::from("/does/not/exist.ogg"),
    ))
}

fn state_with_audio_file(path: PathBuf) -> Arc<AppState> {
    Arc::new(AppState::new("moe".into(), path))
}

/// Write a tiny fake audio file so the file-existence check passes; the
/// content doesn't matter for these tests since we never reach songbird.
fn write_dummy_audio() -> PathBuf {
    let p = std::env::temp_dir().join("chronicle_feeder_dummy.ogg");
    std::fs::write(&p, b"OggS").expect("write dummy audio file");
    p
}

#[tokio::test]
async fn health_returns_idle_shape() {
    let state = state_with_no_audio_file();
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body_bytes = body::to_bytes(res.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(v["name"], "moe");
    assert_eq!(v["in_voice"], false);
    assert_eq!(v["playing"], false);
    assert!(v["user_id"].is_null());
}

#[tokio::test]
async fn play_without_audio_file_returns_404() {
    let state = state_with_no_audio_file();
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/play")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body_bytes = body::to_bytes(res.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .starts_with("audio file missing:")
    );
}

#[tokio::test]
async fn play_in_idle_with_file_returns_400_not_in_voice() {
    let audio = write_dummy_audio();
    let state = state_with_audio_file(audio.clone());
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/play")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body_bytes = body::to_bytes(res.into_body(), 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(v["error"], "not in voice channel");
    let _ = std::fs::remove_file(audio);
}

#[tokio::test]
async fn stop_in_idle_is_noop_200() {
    let state = state_with_no_audio_file();
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn leave_in_idle_is_noop_200_even_without_songbird() {
    // Spec: `/leave` while Idle is a no-op. The handler short-circuits on
    // Idle *before* touching the songbird manager, so this works even in
    // the pre-ready window.
    let state = state_with_no_audio_file();
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/leave")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn join_without_songbird_returns_not_ready() {
    let state = state_with_no_audio_file();
    let app = router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/join")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"guild_id":1,"channel_id":2}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
