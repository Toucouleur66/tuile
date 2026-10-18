// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// A host page: the package, over Cesium ion's terrain and imagery through the
// catalog proxy that serves this page (`tuile-catalog-proxy`). The proxy
// holds the token; this page never sees one. It is the one place that names
// the service; the package does not.

import { createViewer, UnsupportedError } from "../../packages/tuile-viewer/index.js";

const PHOTOGRAPHY = "© Microsoft and its data suppliers";
const backend = new URLSearchParams(location.search).get("backend") ?? "auto";

try {
  // For the console, and for a test driving the page. Everything a person
  // sees — the controls, the menu, the caption, the alerts — is the viewer's.
  globalThis.viewer = await createViewer(document.querySelector("#globe"), {
    name: "Tuile",
    backend,
    sources: {
      catalog: { url: "/catalog/" },
      terrain: { asset: 1 },
      imagery: [
        { key: "aerial", name: "Aerial", asset: 2, attribution: PHOTOGRAPHY },
        { key: "labels", name: "Aerial with labels", asset: 3, attribution: PHOTOGRAPHY },
        {
          key: "satellite",
          name: "Satellite",
          asset: 3954,
          attribution: "Contains modified Copernicus Sentinel data",
        },
      ],
    },
  });
} catch (e) {
  // The viewer has said why in the page; the console gets it too.
  if (!(e instanceof UnsupportedError)) throw e;
  console.warn(e.message);
}
