// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// A host page: the package, over Cesium ion's terrain and imagery through the
// catalog proxy that serves this page (`tuile-catalog-proxy`). The proxy
// holds the token; this page never sees one. It is the one place that names
// the service; the package does not.
//
// Or, with `?terrain=<layer.json URL>`, over a terrain and an imagery template
// named by address, with no catalog at all (see examples/heightfield-serve):
//   ?terrain=…/layer.json&imagery=…/{z}/{x}/{y}.png&scheme=geographic&maxLevel=15
//   &lon=…&lat=…&altitude=…[&heading=…&pitch=…]

import { createViewer, UnsupportedError } from "../../packages/tuile-viewer/index.js";

const PHOTOGRAPHY = "© Microsoft and its data suppliers";
const params = new URLSearchParams(location.search);
const backend = params.get("backend") ?? "auto";

/** The sources by address the query names, or none. */
function addressed() {
  const terrain = params.get("terrain");
  if (!terrain) return undefined;
  const template = params.get("imagery");
  if (!template) throw new Error("?terrain= needs ?imagery=<template> beside it");
  const layer = {
    key: "template",
    name: "Template",
    attribution: `Tiles from ${new URL(template.replaceAll(/\{[xyz]\}/g, "0"), location.href).host}`,
    template,
    scheme: params.get("scheme") ?? "webmercator",
  };
  if (params.has("maxLevel")) layer.maxLevel = Number(params.get("maxLevel"));
  return { terrain: { url: terrain }, imagery: [layer] };
}

/** The view the query names, or none. */
function view() {
  if (!params.has("lon") || !params.has("lat")) return undefined;
  const number = (name, fallback) => (params.has(name) ? Number(params.get(name)) : fallback);
  return {
    lon: number("lon"),
    lat: number("lat"),
    altitude: number("altitude", 3000),
    heading: number("heading", 0),
    pitch: number("pitch", 90),
  };
}

try {
  // For the console, and for a test driving the page. Everything a person
  // sees — the controls, the menu, the caption, the alerts — is the viewer's.
  globalThis.viewer = await createViewer(document.querySelector("#globe"), {
    name: "Tuile",
    backend,
    view: view(),
    sources: addressed() ?? {
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
