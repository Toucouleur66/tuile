// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The package from a bundler: what an application does with it. The catalog is
// the host's proxy (`tuile-catalog-proxy`), which holds the token: this page
// never has one.

import { createViewer, UnsupportedError, type Viewer } from "@lapoule-dev/tuile-viewer";

const container = document.querySelector<HTMLElement>("#globe");
if (!container) throw new Error("no #globe element");

try {
  const viewer: Viewer = await createViewer(container, {
    view: { lon: 6.86, lat: 45.83, altitude: 12_000, heading: 120, pitch: 35 },
    sources: {
      catalog: { url: "/catalog/" },
      terrain: { asset: 1 },
      imagery: [{ key: "aerial", name: "Aerial", asset: 2, attribution: "© the imagery's owners" }],
    },
  });
  viewer.on("viewChanged", ({ lat, lon, altitude }) => {
    document.title = `${lat.toFixed(3)}, ${lon.toFixed(3)} · ${Math.round(altitude)} m`;
  });
  viewer.flyTo({ lon: 7.66, lat: 45.98, altitude: 9_000 }, { duration: 4 });
} catch (e) {
  // The viewer has said why in the container.
  if (e instanceof UnsupportedError) console.warn(e.message);
  else throw e;
}
