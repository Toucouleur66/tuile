// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// A host page: the package, over Cesium ion's terrain and imagery. This page
// is the one place that names the service; the package does not.

import { createViewer, UnsupportedError } from "../../packages/tuile-viewer/index.js";

const status = document.querySelector("#status");
const say = (text) => (status.textContent = text);

/** The token: from the local development server when it has one, else typed. */
async function token() {
  try {
    const answer = await fetch("/dev-token", { cache: "no-store" });
    if (answer.ok) return (await answer.text()).trim();
  } catch {
    // No development server: ask.
  }
  const panel = document.querySelector("#token");
  panel.hidden = false;
  return new Promise((resolve) => {
    panel.querySelector("form").addEventListener("submit", (event) => {
      event.preventDefault();
      panel.hidden = true;
      resolve(panel.querySelector("input").value.trim());
    });
  });
}

const PHOTOGRAPHY = "© Microsoft and its data suppliers";
const backend = new URLSearchParams(location.search).get("backend") ?? "auto";

try {
  const viewer = await createViewer(document.querySelector("#globe"), {
    name: "Tuile",
    backend,
    getToken: token,
    sources: {
      catalog: {},
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
  say(`drawing with ${viewer.backend}`);
  viewer.on("loading", ({ shown, progress }) =>
    say(shown ? `loading${progress == null ? "" : ` ${Math.round(progress * 100)} %`}` : `drawing with ${viewer.backend}`),
  );
  viewer.on("error", ({ title, message }) => say(`${title}: ${message}`));
  // For the console, and for a test driving the page.
  globalThis.viewer = viewer;
} catch (e) {
  say(e instanceof UnsupportedError ? `cannot draw here: ${e.message}` : String(e.message ?? e));
  throw e;
}
