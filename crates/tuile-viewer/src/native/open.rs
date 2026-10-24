// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The host's sources, with those the command line named by address in
//! their place (`--terrain-url`, `--imagery-template`): see [`crate::open`].
//!
//! Whatever the host is, a terrain at a `layer.json` URL and an imagery
//! template are fetched over the native HTTP stack with no token. When both
//! are given the host is not asked for its sources at all, so no credential
//! is looked for.

use std::sync::Arc;

use tuile_core::raster::{ImageryProvider, TemplateProvider};
use tuile_native_fetchers::NativeHttp;
use tuile_terrain::{HttpTerrainSource, LayerJson, TerrainSource};

use crate::embed::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};
use crate::open::{template_tiling, TEMPLATE_IMAGERY, URL_TERRAIN};
use crate::start::Addresses;

/// `inner`, with what `addresses` names in place of its own.
pub(crate) struct Addressed {
    inner: Arc<dyn ViewerHost>,
    addresses: Addresses,
}

impl Addressed {
    /// `inner` itself when the command line named nothing.
    pub(crate) fn wrap(inner: Arc<dyn ViewerHost>, addresses: Addresses) -> Arc<dyn ViewerHost> {
        if addresses.are_nothing() {
            inner
        } else {
            Arc::new(Self { inner, addresses })
        }
    }
}

/// The layer an imagery template is shown as.
fn template_layer(template: &str) -> ImageryChoice {
    // Whose pictures: the server's, which is all the command line says.
    let origin = url::Url::parse(
        &template
            .replace("{z}", "0")
            .replace("{x}", "0")
            .replace("{y}", "0"),
    )
    .ok()
    .and_then(|u| {
        u.host_str().map(|h| match u.port() {
            Some(port) => format!("{h}:{port}"),
            None => h.to_owned(),
        })
    })
    .unwrap_or_default();
    ImageryChoice::from_template(
        "template",
        "Template",
        TEMPLATE_IMAGERY,
        &format!("Tiles from {origin}"),
        template,
    )
}

#[async_trait::async_trait]
impl ViewerHost for Addressed {
    fn identity(&self) -> Identity {
        self.inner.identity()
    }

    fn terrain(&self) -> TerrainChoice {
        match &self.addresses.terrain {
            Some(url) => TerrainChoice::at_url(url.as_str()),
            None => self.inner.terrain(),
        }
    }

    fn imagery(&self) -> Vec<ImageryChoice> {
        match &self.addresses.imagery {
            Some(template) => vec![template_layer(&template.template)],
            None => self.inner.imagery(),
        }
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        let inner = if self.addresses.are_everything() {
            None
        } else {
            Some(self.inner.connect().await?)
        };
        let http = Arc::new(NativeHttp::shared().await.map_err(|e| e.to_string())?);
        Ok(Arc::new(AddressedSources {
            inner,
            http,
            addresses: self.addresses.clone(),
        }))
    }
}

/// The sources: the addressed ones by their numbers, the rest the host's.
struct AddressedSources {
    inner: Option<Arc<dyn Sources>>,
    http: Arc<NativeHttp>,
    addresses: Addresses,
}

impl AddressedSources {
    fn inner(&self) -> Result<&Arc<dyn Sources>, String> {
        self.inner
            .as_ref()
            .ok_or_else(|| "every source was named on the command line".to_owned())
    }
}

#[async_trait::async_trait]
impl Sources for AddressedSources {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        match &self.addresses.terrain {
            Some(url) if asset == URL_TERRAIN => {
                let source = HttpTerrainSource::open(Arc::clone(&self.http), url.clone())
                    .await
                    .map_err(|e| format!("the terrain at {url}: {e}"))?;
                Ok((source.layer().clone(), Arc::new(source)))
            }
            _ => self.inner()?.terrain(asset).await,
        }
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        match &self.addresses.imagery {
            Some(t) if asset == TEMPLATE_IMAGERY => Ok(Arc::new(TemplateProvider::new(
                t.template.clone(),
                Arc::clone(&self.http),
                template_tiling(t.geographic, t.max_level),
            ))),
            _ => self.inner()?.imagery(asset).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::start::Template;

    /// A host whose sources must never be asked for.
    struct Untouchable;

    #[async_trait::async_trait]
    impl ViewerHost for Untouchable {
        fn identity(&self) -> Identity {
            Identity::default()
        }
        fn terrain(&self) -> TerrainChoice {
            TerrainChoice {
                asset: 1,
                cache: "host".into(),
            }
        }
        fn imagery(&self) -> Vec<ImageryChoice> {
            vec![ImageryChoice::new("aerial", "Aerial", 2, "©")]
        }
        async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
            Err("the host was asked for its credentials".into())
        }
    }

    fn template() -> Template {
        Template {
            template: "http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png".into(),
            geographic: true,
            max_level: Some(15),
        }
    }

    /// Named alone, an address replaces only its own kind; named together,
    /// the host is not asked for anything.
    #[test]
    fn addresses_replace_the_host_sources_they_name() {
        let host: Arc<dyn ViewerHost> = Arc::new(Untouchable);
        let url = url::Url::parse("http://127.0.0.1:8092/terrain/layer.json").expect("a URL");
        let same = Addressed::wrap(Arc::clone(&host), Addresses::default());
        assert!(Arc::ptr_eq(&same, &host), "nothing named, the host itself");

        let terrain_only = Addressed::wrap(
            Arc::clone(&host),
            Addresses {
                terrain: Some(url.clone()),
                imagery: None,
            },
        );
        assert_eq!(terrain_only.terrain().asset, URL_TERRAIN);
        assert_eq!(terrain_only.imagery()[0].key, "aerial");

        let both = Addressed::wrap(
            host,
            Addresses {
                terrain: Some(url),
                imagery: Some(template()),
            },
        );
        let layers = both.imagery();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].asset, TEMPLATE_IMAGERY);
        assert_eq!(layers[0].attribution, "Tiles from 127.0.0.1:8092");
        assert!(layers[0].cache.starts_with("imagery-template-"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let sources = runtime
            .block_on(both.connect())
            .expect("no credential is looked for");
        let imagery = runtime
            .block_on(sources.imagery(TEMPLATE_IMAGERY))
            .expect("the template");
        assert_eq!(imagery.tiling_scheme(), template_tiling(true, Some(15)));
        // A number the command line did not name goes to the host — which
        // there is none of here.
        assert!(runtime.block_on(sources.imagery(2)).is_err());
    }
}
