// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The package from a bundler: what an application does with it. The token
// comes from the host's own endpoint, never from this source.

import { createViewer, UnsupportedError, type Viewer } from "@lapoule-dev/tuile-viewer";

async function token(): Promise<string> {
  const answer = await fetch("/api/catalog-token");
  if (!answer.ok) throw new Error(`no token: ${answer.status}`);
  return (await answer.text()).trim();
}

const container = document.querySelector<HTMLElement>("#globe");
if (!container) throw new Error("no #globe element");

try {
  const viewer: Viewer = await createViewer(container, {
    getToken: token,
    view: { lon: 6.86, lat: 45.83, altitude: 12_000, heading: 120, pitch: 35 },
    sources: {
      catalog: {},
      terrain: { asset: 1 },
      imagery: [{ key: "aerial", name: "Aerial", asset: 2, attribution: "© the imagery's owners" }],
    },
  });
  viewer.on("viewChanged", ({ lat, lon, altitude }) => {
    document.title = `${lat.toFixed(3)}, ${lon.toFixed(3)} · ${Math.round(altitude)} m`;
  });
  viewer.flyTo({ lon: 7.66, lat: 45.98, altitude: 9_000 }, { duration: 4 });
} catch (e) {
  if (e instanceof UnsupportedError) container.textContent = e.message;
  else throw e;
}
