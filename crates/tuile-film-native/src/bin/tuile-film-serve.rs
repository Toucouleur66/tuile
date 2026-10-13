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
//! in the store, with its [`SegmentDone`]; a pack not there yet is waited for
//! up to `pack_within_seconds`. `GET /` says the service is up.
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

use tuile_film_native::serve;
use tuile_film_native::Error;

async fn run() -> Result<(), Error> {
    let service = Arc::new(serve::service_from_env().await?);
    eprintln!("tuile-film-serve: {:?}", service.limits);
    serve::serve(serve::router(service, None)).await
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
