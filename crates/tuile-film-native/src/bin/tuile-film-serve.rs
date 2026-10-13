// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render as a service: segments of film asked over HTTP.
//!
//! ```text
//! tuile-film-serve
//! curl -X POST localhost:8080/segments -d '{"pack": "…", "frames": [1, 450], "out": "…/1-450.mp4"}'
//! ```
//!
//! `POST /segments` takes a [`SegmentOrder`] and answers when the segment is
//! in the store, with its [`SegmentDone`]; it may be asked before its pack is
//! baked, and reads ahead what the bake announces meanwhile. `GET /` says the
//! service is up.
//!
//! The stores are the environment's, as for `tuile-film-produce`:
//!
//! ```text
//! TUILE_STORE_ENDPOINT, TUILE_STORE_BUCKET,
//! TUILE_STORE_ACCESS_KEY_ID, TUILE_STORE_SECRET_ACCESS_KEY   the run store, a bucket
//! TUILE_STORE_DIR                                            …or a directory, instead
//! TUILE_TILES_REMOTE (+ _HEADER, _SECRET) or TUILE_TILES_BUCKET   the tile store
//! PORT                                                       8080 unless said
//! TUILE_SERVE_CACHE                                          where caches are kept
//! TUILE_SERVE_CACHE_GIB, TUILE_SERVE_GENERATION_MINUTES      when they are let go
//! TUILE_SERVE_RENDERS                                        renders at once on the card
//! ```

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use tuile_farm::ObjectRunStore;
use tuile_film_native::serve::{Limits, SegmentDone, SegmentOrder, Service};
use tuile_film_native::setup::tile_store;
use tuile_film_native::Error;

fn number(name: &str) -> Result<Option<u64>, Error> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(Some(v.parse().map_err(|e| format!("{name}={v}: {e}"))?)),
        _ => Ok(None),
    }
}

fn limits() -> Result<Limits, Error> {
    let default = Limits::default();
    Ok(Limits {
        cache_bytes: number("TUILE_SERVE_CACHE_GIB")?.map_or(default.cache_bytes, |gib| gib << 30),
        generation_age: number("TUILE_SERVE_GENERATION_MINUTES")?
            .map_or(default.generation_age, |m| Duration::from_secs(m * 60)),
        renders_at_once: number("TUILE_SERVE_RENDERS")?
            .map_or(default.renders_at_once, |n| n as usize),
    })
}

async fn segment(
    State(service): State<Arc<Service>>,
    Json(order): Json<SegmentOrder>,
) -> Result<Json<SegmentDone>, (StatusCode, String)> {
    eprintln!(
        "segment {} ← {} {}:{}",
        order.out, order.pack, order.frames.0, order.frames.1
    );
    match service.segment(&order).await {
        Ok(done) => {
            eprintln!(
                "segment {} done: {} frames, waited {:.0} s, rendered in {:.0} s, {} tiles read ahead",
                done.out, done.frames, done.waited_seconds, done.render_seconds, done.announced_tiles
            );
            Ok(Json(done))
        }
        Err(e) => {
            eprintln!("segment {} failed: {e}", order.out);
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

async fn run() -> Result<(), Error> {
    let runs = Arc::new(ObjectRunStore::from_env()?);
    let cache = std::env::var("TUILE_SERVE_CACHE")
        .map(Into::into)
        .unwrap_or_else(|_| std::env::temp_dir().join("tuile-film-serve"));
    let tiles = tile_store(&cache).await?;
    let limits = limits()?;
    let service = Arc::new(Service::new(runs, tiles.objects, cache, limits));
    let app = Router::new()
        .route("/", get(|| async { "tuile-film-serve" }))
        .route("/segments", post(segment))
        .with_state(service);
    let port = number("PORT")?.unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port as u16)).await?;
    eprintln!("tuile-film-serve: on :{port}, {limits:?}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("tuile-film-serve: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let ended = runtime.block_on(run());
    runtime.shutdown_background();
    match ended {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tuile-film-serve: {e}");
            ExitCode::FAILURE
        }
    }
}
