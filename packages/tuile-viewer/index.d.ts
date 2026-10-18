// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

/** Where the eye is and how it looks: degrees and metres. */
export interface View {
  /** Longitude of the eye, degrees, east positive. */
  lon: number;
  /** Latitude of the eye, degrees, north positive. */
  lat: number;
  /** Height of the eye above the ellipsoid, metres (1 to 1e8). */
  altitude: number;
  /** Compass heading, degrees clockwise from north. 0 unless said. */
  heading?: number;
  /** Angle below the horizon, degrees; 90 looks straight down, and is the default. */
  pitch?: number;
}

/** One imagery layer: from the catalog by asset number, or from a URL template. */
export interface ImageryLayer {
  /** Lower case, no spaces: what the layer is called in code. */
  key: string;
  /** What a person reads. */
  name: string;
  /** Whose pictures these are; kept on screen while the layer is. */
  attribution: string;
  /** The catalog's asset number. Give this or `template`. */
  asset?: number;
  /** `https://…/{z}/{x}/{y}.png`. Give this or `asset`. */
  template?: string;
  /** The grid a template's tiles are cut on. `webmercator` unless said. */
  scheme?: "webmercator" | "geographic";
  /** The deepest level a template serves. */
  maxLevel?: number;
}

/** Where terrain and imagery come from. */
export interface Sources {
  /**
   * The catalog that resolves asset numbers to tile endpoints: `url` is the
   * address of the catalog proxy, which holds the catalog's token on the server
   * (`tuile-catalog-proxy`). Relative to the page unless absolute. The page
   * never holds a token.
   */
  catalog: { url: string };
  /** The terrain: an asset of the catalog. */
  terrain: { asset: number };
  /** The layers, in the order a person should meet them; the first is shown first. */
  imagery: ImageryLayer[];
}

export interface ViewerOptions {
  sources: Sources;
  /** The host application's name, used in messages. */
  name?: string;
  /** The numbers the session runs with. `auto` unless said. */
  profile?: "auto" | "desktop" | "handheld";
  /** The graphics backend. `auto`: WebGPU where there is one, else WebGL2. */
  backend?: "auto" | "webgpu" | "webgl2";
  /**
   * Where the eye opens. Without it, where the last viewer of this `name` on
   * this site was left (see `rememberView`), else straight down on western
   * Europe from 2 000 km.
   */
  view?: View;
  /** Whether the on-screen compass, zoom controls and menu are drawn. `true` unless said. */
  ui?: boolean;
  /**
   * The bottom corner the caption keeps to: whose pictures are on screen, and
   * the last thing the viewer said. `bottom-right` unless said.
   */
  attribution?: "bottom-right" | "bottom-left";
  /** Whether a viewer that cannot draw says so in the container. `true` unless said. */
  showUnsupportedMessage?: boolean;
  /**
   * Whether a viewer created without `view` opens where the last one was left,
   * kept in the site's local storage. `true` unless said.
   */
  rememberView?: boolean;
  /**
   * The most Web Workers that decode imagery and terrain beside the geometry
   * server's: one per core the browser reports, less one, up to this. `0`
   * decodes on the geometry server's own worker. `4` unless said.
   */
  decodeWorkers?: number;
}

export interface ViewerEvents {
  /** The viewer draws, through this backend. */
  ready: { backend: string };
  /** The loading indicator came or went, or its progress moved. */
  loading: { shown: boolean; progress: number | null };
  /**
   * Something went wrong that the person should hear about. It is also shown over
   * the view; a session that cannot start is tried again.
   */
  error: { title: string; message: string };
  /** The view, a few times a second at most while it moves. */
  viewChanged: Required<View>;
  /** The layer on screen changed, and with it whose pictures these are. */
  attributionChanged: { layer: string; name: string; attribution: string };
}

export interface Viewer {
  /** What the viewer draws through. */
  readonly backend: "webgpu" | "webgl2" | undefined;
  /** Puts the eye there at once. */
  setView(view: View): void;
  /** Glides there: up, across and down, over `duration` seconds (2.5 unless said). */
  flyTo(view: View, options?: { duration?: number }): void;
  /** The view the last frame was drawn from; `undefined` before the first. */
  getView(): Required<View> | undefined;
  /** Calls `listener` on each `event`; returns the function that stops it. */
  on<K extends keyof ViewerEvents>(event: K, listener: (detail: ViewerEvents[K]) => void): () => void;
  /** Draws a frame now. The canvas size is observed, so a layout change needs nothing. */
  resize(): void;
  /** Stops drawing and holds the loads, as a hidden tab does. */
  pause(): void;
  /** Draws and loads again. */
  resume(): void;
  /** Releases the GPU, the worker and every listener. The viewer is unusable after. */
  destroy(): void;
  /**
   * Reserved for the control API of the graphics budget: budgets, GPU
   * figures, frame readback, scripted moves. Not available yet.
   */
  readonly debug?: never;
}

/** Neither WebGPU nor WebGL2 is available, or the one asked for is not. */
export class UnsupportedError extends Error {
  readonly name: "UnsupportedError";
}

/** The options are wrong. */
export class StartError extends Error {
  readonly name: "StartError";
}

/**
 * Creates a viewer in `container` — an element it fills with a canvas, or a
 * canvas to draw into — and resolves once it draws.
 */
export function createViewer(container: HTMLElement, options: ViewerOptions): Promise<Viewer>;
