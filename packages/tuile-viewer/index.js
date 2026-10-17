// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The globe viewer as an ES module: `createViewer(container, options)`.
//
// The viewer itself is Rust compiled to WebAssembly — the same application,
// renderer and controls as the desktop's — drawing with WebGPU, or WebGL2
// where the browser has no WebGPU. Its geometry server runs in a worker this
// module starts. This file only normalises the options, places the canvas and
// the attribution, and turns the wasm module's events into a typed API.

import init, { startViewer } from "./wasm/tuile_viewer.js";

/** No graphics backend this viewer can draw with: neither WebGPU nor WebGL2. */
export class UnsupportedError extends Error {
  constructor(message) {
    super(message);
    this.name = "UnsupportedError";
  }
}

/** The viewer could not start: wrong options, or a session that failed. */
export class StartError extends Error {
  constructor(message) {
    super(message);
    this.name = "StartError";
  }
}

const EVENTS = ["ready", "loading", "error", "viewChanged", "attributionChanged"];

/** The viewer of the moment: one per page, and a new one replaces it. */
let current = null;

let loaded;
/** Loads and instantiates the wasm module once per page. */
function load() {
  loaded ??= init();
  return loaded;
}

/** Whether this browser offers a WebGL2 context at all. */
function hasWebgl2() {
  try {
    return !!document.createElement("canvas").getContext("webgl2");
  } catch {
    return false;
  }
}

/** `auto` becomes `handheld` on a touch-first, small screen. */
function resolveProfile(profile) {
  if (profile === "desktop" || profile === "handheld") return profile;
  const coarse = globalThis.matchMedia?.("(pointer: coarse)").matches ?? false;
  const small = Math.min(screen.width, screen.height) < 900;
  return coarse && small ? "handheld" : "desktop";
}

/** What the browser can draw with, checked before anything is loaded. */
function checkSupport(backend) {
  const webgpu = "gpu" in navigator;
  const webgl2 = hasWebgl2();
  if (backend === "webgpu" && !webgpu) {
    return "this browser has no WebGPU, and the viewer was asked to use it";
  }
  if (backend === "webgl2" && !webgl2) {
    return "this browser has no WebGL2, and the viewer was asked to use it";
  }
  if (!webgpu && !webgl2) {
    return "this browser has neither WebGPU nor WebGL2";
  }
  return null;
}

function message(host, text) {
  const note = document.createElement("div");
  note.textContent = text;
  note.style.cssText =
    "position:absolute;inset:0;display:flex;align-items:center;justify-content:center;" +
    "padding:16px;text-align:center;font:14px system-ui,sans-serif;color:#ddd;background:#000";
  host.append(note);
  return note;
}

/**
 * Creates a viewer in `container`: an element the viewer fills with a canvas
 * of its own, or a canvas to draw into.
 *
 * Resolves once the viewer draws; rejects with `UnsupportedError` when the
 * browser cannot, and with `StartError` when the options are wrong.
 */
export async function createViewer(container, options) {
  if (!(container instanceof HTMLElement)) {
    throw new TypeError("createViewer: the container must be an element");
  }
  const o = options ?? {};
  const backend = o.backend ?? "auto";
  const hostElement = container instanceof HTMLCanvasElement ? container.parentElement : container;
  const unsupported = checkSupport(backend);
  if (unsupported) {
    if (o.showUnsupportedMessage !== false && hostElement) message(hostElement, unsupported);
    throw new UnsupportedError(unsupported);
  }
  await load();
  // Before the new canvas exists, so the two never share the page.
  current?.destroy();

  let canvas = container;
  let ownCanvas = false;
  if (!(container instanceof HTMLCanvasElement)) {
    canvas = document.createElement("canvas");
    canvas.style.cssText = "display:block;width:100%;height:100%;touch-action:none;outline:none";
    canvas.tabIndex = 0;
    if (getComputedStyle(container).position === "static") container.style.position = "relative";
    container.append(canvas);
    ownCanvas = true;
  }

  const placement = o.attribution ?? "bottom-right";
  let credit = null;
  if (placement !== "none" && hostElement) {
    credit = document.createElement("div");
    const side = placement === "bottom-left" ? "left" : "right";
    credit.style.cssText =
      `position:absolute;bottom:4px;${side}:6px;max-width:70%;pointer-events:none;` +
      "font:11px system-ui,sans-serif;color:#fff;text-shadow:0 0 3px #000;white-space:nowrap;" +
      "overflow:hidden;text-overflow:ellipsis";
    hostElement.append(credit);
  }

  const token = o.token ?? (o.getToken ? await o.getToken() : undefined);
  const normalized = {
    name: o.name ?? "Globe",
    sources: o.sources,
    profile: resolveProfile(o.profile ?? "auto"),
    deviceMemory: navigator.deviceMemory,
    backend,
    view: o.view,
    ui: o.ui ?? true,
  };

  const listeners = new Map(EVENTS.map((kind) => [kind, new Set()]));
  let settle;
  const drawing = new Promise((resolve, reject) => {
    settle = { resolve, reject };
  });
  let ready = false;
  const onEvent = (kind, detail) => {
    if (kind === "ready") {
      ready = true;
      settle.resolve();
    } else if (kind === "unsupported") {
      if (o.showUnsupportedMessage !== false && hostElement) message(hostElement, detail.message);
      settle.reject(new UnsupportedError(detail.message));
      return;
    } else if (kind === "error" && !ready) {
      settle.reject(new StartError(detail.message));
    } else if (kind === "attributionChanged" && credit) {
      credit.textContent = `${detail.name} · ${detail.attribution}`;
    }
    for (const listener of listeners.get(kind) ?? []) {
      try {
        listener(detail);
      } catch (e) {
        console.error(e);
      }
    }
  };

  // Written out here, literally, so a bundler sees the worker script and
  // ships it with everything it imports.
  const makeWorker = () => new Worker(new URL("./worker.js", import.meta.url), { type: "module" });

  let handle;
  try {
    handle = startViewer(canvas, normalized, makeWorker, token, onEvent);
  } catch (e) {
    if (ownCanvas) canvas.remove();
    credit?.remove();
    throw new StartError(e?.message ?? String(e));
  }
  const viewer = new Viewer(handle, listeners, () => {
    if (ownCanvas) canvas.remove();
    credit?.remove();
    if (current === viewer) current = null;
  });
  current = viewer;
  try {
    await drawing;
  } catch (e) {
    viewer.destroy();
    throw e;
  }
  return viewer;
}

/** A viewer on the page. */
class Viewer {
  #handle;
  #listeners;
  #cleanup;

  constructor(handle, listeners, cleanup) {
    this.#handle = handle;
    this.#listeners = listeners;
    this.#cleanup = cleanup;
  }

  /** `"webgpu"` or `"webgl2"`: what the viewer draws through. */
  get backend() {
    const b = this.#handle?.backend;
    return b === "webgpu" ? "webgpu" : b ? "webgl2" : undefined;
  }

  /** Puts the eye there at once. */
  setView(view) {
    this.#live().goTo(view.lon, view.lat, view.altitude, view.heading, view.pitch, undefined);
  }

  /** Glides there, over `duration` seconds (2.5 unless said). */
  flyTo(view, { duration = 2.5 } = {}) {
    this.#live().goTo(view.lon, view.lat, view.altitude, view.heading, view.pitch, duration);
  }

  /** The view the last frame was drawn from. */
  getView() {
    return this.#live().getView();
  }

  /** Calls `listener` on each `event`; returns the function that stops it. */
  on(event, listener) {
    const set = this.#listeners.get(event);
    if (!set) throw new TypeError(`unknown event ${JSON.stringify(event)}`);
    set.add(listener);
    return () => set.delete(listener);
  }

  /**
   * The canvas size is observed, so a layout change needs nothing; call this
   * after changing the container's size in a way no layout sees, and the next
   * frame takes the new size.
   */
  resize() {
    this.#live().redraw();
  }

  /** Stops drawing and holds the loads, as a hidden tab does. */
  pause() {
    this.#live().pause();
  }

  /** Draws and loads again. */
  resume() {
    this.#live().resume();
  }

  /** Releases the GPU, the worker and every listener. The viewer is unusable after. */
  destroy() {
    if (!this.#handle) return;
    this.#handle.destroy();
    this.#handle.free();
    this.#handle = null;
    for (const set of this.#listeners.values()) set.clear();
    this.#cleanup();
  }

  #live() {
    if (!this.#handle) throw new Error("the viewer has been destroyed");
    return this.#handle;
  }
}
