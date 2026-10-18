// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer, over the public connectors — the host every platform's build
//! runs: `main.rs` on a desktop and a phone, `android_main` on Android (named, not
//! linked: like `DEVELOPMENT_CATALOG` below, it exists only in an Android build).
//!
//! The application itself — window, camera, controls, steering, scripting — is
//! the `tuile-viewer` library. This crate is the one decision a host makes:
//! **where terrain and imagery come from.** Here that is Cesium ion, with Bing
//! imagery behind it, over a native HTTP transport; the choice is made in this
//! file and nowhere deeper. Another host answers the same three questions with
//! its own service and reuses everything else — see
//! `docs/16-embedding-the-viewer.md`.
//!
//! ```text
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer -- \
//!     --lon 6.86 --lat 45.83 --altitude 6000 --heading 120 --pitch 25
//! ```
//!
//! The access token is read from the environment, or — for a session started
//! from an icon — from one file the person owns:
//! `~/Library/Application Support/Tuile/token` on macOS.
//!
//! **On Android there is no token at all.** The catalog is asked through the
//! catalog proxy (`tuile-catalog-proxy`), which holds the token on a server:
//! an APK carries nothing that would let anyone who unpacks it use the
//! account. The proxy's address is `TUILE_CATALOG_URL` — a launch setting,
//! else the value it had when the library was built, else
//! `DEVELOPMENT_CATALOG`, which `scripts/try-viewer.sh android` serves.

use std::sync::Arc;

use tuile_cesium_ion::IonSources;
use tuile_native_fetchers::{default_cache_dir, CacheConfig, NativeHttp};
use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};

/// The environment variable the access token is read from — on every
/// platform but Android, which holds none.
const TOKEN_VARIABLE: &str = "CESIUM_ION_TOKEN";

/// The ion asset of the terrain: Cesium World Terrain.
const WORLD_TERRAIN: i64 = 1;

/// The ion assets of the three imagery layers.
const AERIAL: i64 = 2;
const AERIAL_WITH_LABELS: i64 = 3;
const SATELLITE_MOSAIC: i64 = 3954;

/// Whose pictures the layers are: shown beside the layer's name for as long
/// as it is on screen.
const PHOTOGRAPHY: &str = "© Microsoft and its data suppliers";
const MOSAIC: &str = "Contains modified Copernicus Sentinel data";

/// The catalog proxy a development build asks, on the developer's machine:
/// `adb reverse tcp:8091 tcp:8091` makes it the device's own loopback, for an
/// emulator and a phone on a cable alike.
#[cfg(target_os = "android")]
pub const DEVELOPMENT_CATALOG: &str = "http://127.0.0.1:8091/catalog/";

/// The public host: the project's own application over ion.
pub struct Public;

#[async_trait::async_trait]
impl ViewerHost for Public {
    fn identity(&self) -> Identity {
        tuile_viewer::renamed_by_environment(Identity {
            // The store this viewer has always written, so that a session
            // after this change starts as warm as the one before it.
            store: tuile_bing::cache_name("Aerial"),
            credentials: if cfg!(target_os = "android") {
                "No token on this device: the catalog is asked through a proxy that holds it \
                 (TUILE_CATALOG_URL)."
                    .to_owned()
            } else {
                format!(
                    "The access token is read from {TOKEN_VARIABLE}, or from the file `token` \
                     in the application's support directory."
                )
            },
            ..Identity::default()
        })
    }

    fn terrain(&self) -> TerrainChoice {
        TerrainChoice {
            asset: WORLD_TERRAIN,
            cache: "ion-cwt".into(),
        }
    }

    fn imagery(&self) -> Vec<ImageryChoice> {
        // The three the connector has. Neutral names: what a layer shows, not
        // who serves it — the attribution says that.
        vec![
            // Asset 2, under the namespace its tiles have always been stored
            // in, so a session after this change starts as warm as before.
            ImageryChoice::new("aerial", "Aerial", AERIAL, PHOTOGRAPHY)
                .cached_as(&tuile_bing::cache_namespace("Aerial")),
            // The same photography with roads and place names drawn over it.
            ImageryChoice::new(
                "labels",
                "Aerial with labels",
                AERIAL_WITH_LABELS,
                PHOTOGRAPHY,
            ),
            // One cloudless mosaic of the whole planet from a single
            // instrument: coarser, and the same colour everywhere. Served on
            // the geographic grid, where the two above are web-mercator.
            ImageryChoice::new("satellite", "Satellite", SATELLITE_MOSAIC, MOSAIC),
        ]
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        // One pooled, cached native transport drives both ion and Bing — with
        // a phone's smaller cache on a phone, where the viewer's own tile
        // store already holds the tiles.
        let http = if cfg!(any(target_os = "ios", target_os = "android")) {
            NativeHttp::with_config(CacheConfig::handheld(default_cache_dir())).await
        } else {
            NativeHttp::shared().await
        };
        let http = Arc::new(http.map_err(|e| e.to_string())?);
        #[cfg(target_os = "android")]
        {
            let catalog = catalog_url()?;
            tracing::info!(%catalog, "catalog proxy");
            Ok(Arc::new(
                IonSources::new(Arc::clone(&http), http, "").with_api_base(catalog),
            ))
        }
        #[cfg(not(target_os = "android"))]
        {
            let token = tuile_viewer::credential(TOKEN_VARIABLE).map_err(|e| e.to_string())?;
            Ok(Arc::new(IonSources::new(Arc::clone(&http), http, token)))
        }
    }
}

/// The catalog proxy's address, ending in `/` so the catalog's paths are
/// read under it.
#[cfg(target_os = "android")]
fn catalog_url() -> Result<url::Url, String> {
    let given = std::env::var("TUILE_CATALOG_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| option_env!("TUILE_CATALOG_URL").map(str::to_owned))
        .unwrap_or_else(|| DEVELOPMENT_CATALOG.to_owned());
    let mut url = url::Url::parse(given.trim())
        .map_err(|e| format!("the catalog proxy's address {given:?}: {e}"))?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

/// The entry the system calls once the activity has started: on a thread of
/// its own, which becomes the application's main thread.
///
/// Exported unmangled because that is how `android-activity` finds it in the
/// library the activity loads — the one item of this crate that is.
#[cfg(target_os = "android")]
#[allow(unsafe_code)]
#[no_mangle]
fn android_main(app: tuile_viewer::android::AndroidApp) {
    tuile_viewer::android::main(app, Public)
}
