// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a page's tiles come from: the catalog it named, and the URL
//! templates it listed — answered by asset number, as every host's are.
//!
//! The catalog is the connector's ([`tuile_cesium_ion::IonSources`]), over
//! the browser's `fetch`, asked through the proxy the page names — which holds
//! the catalog's token, so that the page never does (see
//! `tuile_cesium_ion::proxy`). A template layer is answered here, by the
//! number [`SourcesSpec::choices`] gave it.

use std::sync::Arc;

use async_trait::async_trait;
use tuile_cesium_ion::IonSources;
use tuile_core::raster::{ImageryProvider, TemplateProvider};
use tuile_planetary::Sources;
use tuile_terrain::{LayerJson, TerrainSource};
use tuile_web::{WebFetcher, WebIonHttp};

use super::options::SourcesSpec;

/// The page's sources, in the worker.
pub(crate) struct PageSources {
    catalog: IonSources<WebIonHttp, WebFetcher>,
    spec: SourcesSpec,
}

impl PageSources {
    /// Over `spec`, through the catalog proxy it names, with no token.
    pub(crate) fn new(spec: SourcesSpec) -> Result<Self, String> {
        let catalog = IonSources::new(Arc::new(WebIonHttp), Arc::new(WebFetcher), "")
            .with_api_base(spec.catalog_base()?);
        Ok(Self { catalog, spec })
    }
}

#[async_trait(?Send)]
impl Sources for PageSources {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        self.catalog.terrain(asset).await
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        let Some(layer) = self.spec.template_of(asset) else {
            return self.catalog.imagery(asset).await;
        };
        let template = layer.template.clone().unwrap_or_default();
        Ok(Arc::new(TemplateProvider::new(
            template,
            Arc::new(WebFetcher),
            layer.tiling(),
        )))
    }
}
