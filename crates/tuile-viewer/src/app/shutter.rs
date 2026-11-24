// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Snapshots: the frame drawn, saved as a picture with the view in its EXIF.
//!
//! *View › Save Snapshot*, the `S` key and `tuile://snapshot` all end in
//! `Command::Snapshot`, which arms the shutter. The next frame drawn is
//! drawn a second time, into a texture of its own, with the same tiles, the
//! same view and the same pass — the globe, and over it only the layers the
//! settings list ([`crate::photo::Layers`]: none by default, so no control,
//! label or panel is in the picture). That texture is copied into a
//! [`tuile_wgpu::ReadbackRing`] and read back without waiting for it: the
//! frame is never held up by its own snapshot. Once mapped, the picture is
//! cropped to the view (the bars around it are not the picture), encoded
//! with its EXIF ([`crate::photo::develop`]) and kept: written into the
//! pictures folder on a thread of its own on a desktop or a phone,
//! downloaded on a page. The title bar says where it went.

use crate::frame_host::Letterbox;
use crate::photo::{Exposure, SnapshotSettings};

/// Snapshots that may be on their way back from the GPU at once.
const SLOTS: usize = 2;

/// A snapshot taken, on its way back from the GPU.
struct Taken {
    tag: u64,
    exposure: Exposure,
    /// The picture's rectangle of the frame.
    picture: Letterbox,
}

/// A snapshot back from the GPU: the facts it was taken with, and its
/// pixels, tightly packed RGBA8, cropped to the picture.
pub(crate) struct Developed {
    pub exposure: Exposure,
    pub rgba: Vec<u8>,
}

/// What a thread writing a snapshot says back.
#[cfg(not(target_arch = "wasm32"))]
type Kept = Result<std::path::PathBuf, String>;

/// The snapshots of a session.
pub(crate) struct Shutter {
    /// Asked for, to be taken on the next frame drawn.
    armed: bool,
    ring: tuile_wgpu::ReadbackRing,
    taken: Vec<Taken>,
    next: u64,
    pub(super) settings: SnapshotSettings,
    /// What the threads that write the files say back: the path written,
    /// or why not.
    #[cfg(not(target_arch = "wasm32"))]
    kept: (
        std::sync::mpsc::Sender<Kept>,
        std::sync::mpsc::Receiver<Kept>,
    ),
    /// Files still being written.
    #[cfg(not(target_arch = "wasm32"))]
    writing: usize,
}

impl Shutter {
    pub(crate) fn new(settings: SnapshotSettings) -> Self {
        Self {
            armed: false,
            ring: tuile_wgpu::ReadbackRing::new(SLOTS),
            taken: Vec::new(),
            next: 0,
            settings,
            #[cfg(not(target_arch = "wasm32"))]
            kept: std::sync::mpsc::channel(),
            #[cfg(not(target_arch = "wasm32"))]
            writing: 0,
        }
    }

    /// Asks for a snapshot of the next frame drawn.
    pub(crate) fn arm(&mut self) {
        self.armed = true;
    }

    /// Whether the next frame is to be taken; disarmed by asking.
    pub(crate) fn take_armed(&mut self) -> bool {
        std::mem::replace(&mut self.armed, false)
    }

    /// Whether a snapshot is asked for, in flight or being written: the
    /// loop keeps turning until it is done.
    pub(crate) fn busy(&self) -> bool {
        let writing = {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.writing > 0
            }
            #[cfg(target_arch = "wasm32")]
            {
                false
            }
        };
        self.armed || !self.taken.is_empty() || writing
    }

    pub(crate) fn format(&self) -> crate::photo::Format {
        self.settings.format.unwrap_or_default()
    }

    pub(crate) fn layers(&self) -> crate::photo::Layers {
        self.settings.layers.unwrap_or_default()
    }

    /// Draws the frame once more, into a texture of its own, with `paint`
    /// and `after` — the frame's own pass and the one that follows — and
    /// copies it to be read back. `false` when every slot is still in
    /// flight: the snapshot is not taken.
    pub(crate) fn expose(
        &mut self,
        gpu: &tuile_wgpu::GpuContext,
        targets: &tuile_wgpu::FrameTargets,
        clear: wgpu::Color,
        draw: (
            impl FnOnce(&mut wgpu::RenderPass<'_>),
            Option<impl FnOnce(&mut wgpu::CommandEncoder, &tuile_wgpu::FramePass<'_>)>,
        ),
        exposure: Exposure,
        picture: Letterbox,
    ) -> bool {
        let (paint, after) = draw;
        let size = targets.size();
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tuile snapshot"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: targets.format(),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        tuile_wgpu::draw_to_texture(
            tuile_wgpu::FrameOnTexture {
                gpu,
                texture: &texture,
                targets,
                clear,
                capture: None,
            },
            paint,
            after,
        );
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let tag = self.next;
        if !self.ring.capture(gpu, &mut encoder, &texture, size, tag) {
            return false;
        }
        self.next += 1;
        gpu.queue.submit([encoder.finish()]);
        self.ring.submitted();
        self.taken.push(Taken {
            tag,
            exposure,
            picture,
        });
        true
    }

    /// The snapshots the GPU has finished, cropped to their pictures. Never
    /// waits, unless `wait`: a test's.
    pub(crate) fn developed(&mut self, gpu: &tuile_wgpu::GpuContext, wait: bool) -> Vec<Developed> {
        let mut out = Vec::new();
        for frame in self.ring.ready(gpu, wait) {
            let Some(at) = self.taken.iter().position(|t| t.tag == frame.tag) else {
                continue;
            };
            let taken = self.taken.remove(at);
            let size = frame.size;
            let Some(rgba) = frame.read(1) else {
                tracing::warn!("a snapshot could not be read back");
                continue;
            };
            let (rgba, w, h) = crop(rgba, size, taken.picture);
            let mut exposure = taken.exposure;
            exposure.size = (w, h);
            out.push(Developed { exposure, rgba });
        }
        out
    }
}

/// The picture's rectangle of a frame of `size`, and its size.
fn crop(pixels: Vec<u8>, size: (u32, u32), picture: Letterbox) -> (Vec<u8>, u32, u32) {
    let x = picture.x.min(size.0);
    let y = picture.y.min(size.1);
    let w = picture.width.min(size.0 - x);
    let h = picture.height.min(size.1 - y);
    if (x, y, w, h) == (0, 0, size.0, size.1) || w == 0 || h == 0 {
        return (pixels, size.0, size.1);
    }
    let stride = size.0 as usize * 4;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for row in y..y + h {
        let start = row as usize * stride + x as usize * 4;
        out.extend_from_slice(&pixels[start..start + w as usize * 4]);
    }
    (out, w, h)
}

impl super::App {
    /// `Command::Snapshot`.
    pub(super) fn take_a_snapshot(&mut self) {
        if self.active.is_none() {
            self.say("no snapshot: nothing is drawn yet");
            return;
        }
        self.shutter.arm();
    }

    /// The facts the frame about to be drawn is taken with.
    pub(super) fn exposure(&self) -> Option<Exposure> {
        let active = self.active.as_ref()?;
        let camera = &self.controller.camera;
        let eye = tuile_core::geo::ecef_to_geodetic(camera.position);
        let picture = self.picture();
        let (vw, vh) = self.viewport();
        let view = crate::snapshot::Snapshot::of(&self.controller, (vw as u32, vh as u32));
        let info = active.gpu.device.adapter_info();
        let m = tuile_core::metrics::metrics();
        let classes = tuile_core::metrics::budget().map(|s| {
            s.bytes
                .iter()
                .map(|(class, [drawable, in_transit])| {
                    (
                        format!("{class:?}"),
                        serde_json::json!({
                            "drawable": drawable,
                            "in_transit": in_transit,
                            "target": s.target_bytes[class],
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        });
        let budget = serde_json::json!({
            "gpu_bytes": self.profile.gpu_bytes,
            "classes": classes,
            "mesh_bytes": m.mesh_bytes_by_level.total(),
            "texture_bytes": m.texture_bytes_by_level.total(),
            "drawn_by_level": m.drawn_by_level.occupied(),
            "tiles_drawn": self.drawn.0,
            "settled": self.drawn.1,
        });
        let terrain = {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.session.as_ref().map(|s| s.plan.terrain.asset)
            }
            #[cfg(target_arch = "wasm32")]
            {
                None
            }
        };
        Some(Exposure {
            taken: super::wall_seconds(),
            lon: eye.lon.to_degrees(),
            lat: eye.lat.to_degrees(),
            height: eye.height,
            ground: Some(eye.height - self.controller.height_above_ground()),
            target: view.target.map(|(t, range)| crate::photo::Target {
                lon: t.lon,
                lat: t.lat,
                height: t.height,
                range,
            }),
            heading: view.view.heading,
            pitch: view.view.pitch,
            roll: camera.roll().to_degrees(),
            fovy: camera.fovy.to_degrees(),
            size: (picture.width, picture.height),
            scene: self.instant,
            rate: self.atmosphere.clock.rate(),
            imagery: self
                .imagery
                .layers
                .get(self.imagery.layer)
                .map(crate::photo::Imagery::of),
            fallback: self.fallback,
            compression: tuile_wgpu::format_name(active.gpu.imagery_plan().target).to_owned(),
            terrain,
            sse: self.profile.sse_threshold,
            budget: Some(budget),
            view: super::DIAGNOSTICS[self.views.diagnostic % super::DIAGNOSTICS.len()].0,
            wireframe: self.views.wireframe,
            adapter: info.name,
            backend: info.backend.to_string(),
            layers: self.shutter.layers(),
        })
    }

    /// Encodes and keeps what the GPU has finished, and says where it went.
    pub(super) fn develop_the_snapshots(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let developed = self.shutter.developed(&active.gpu, false);
        let format = self.shutter.format();
        let mut said = Vec::new();
        for Developed { exposure, rgba } in developed {
            let name = crate::photo::file_name(&exposure, format);
            #[cfg(not(target_arch = "wasm32"))]
            {
                let Some(folder) = crate::photo::folder(&self.shutter.settings) else {
                    said.push("no snapshot: there is no home folder to keep it in".to_owned());
                    continue;
                };
                let kept = self.shutter.kept.0.clone();
                // Encoding a full frame takes longer than a frame: off the
                // render thread.
                let spawned = std::thread::Builder::new()
                    .name("tuile-snapshot".into())
                    .spawn(move || {
                        let result =
                            crate::photo::develop(&rgba, &exposure, format).and_then(|bytes| {
                                crate::photo::keep(&folder, &name, &bytes)
                                    .map_err(|e| format!("{}: {e}", folder.display()))
                            });
                        let _ = kept.send(result);
                    });
                match spawned {
                    Ok(_) => self.shutter.writing += 1,
                    Err(e) => said.push(format!("no snapshot: {e}")),
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                match crate::photo::develop(&rgba, &exposure, format) {
                    Ok(bytes) if crate::web::system::download(&name, format.mime(), &bytes) => {
                        said.push(format!("snapshot downloaded: {name}"));
                    }
                    Ok(_) => said.push("no snapshot: the browser refused the download".into()),
                    Err(why) => said.push(format!("no snapshot: {why}")),
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        for result in self.shutter.kept.1.try_iter() {
            self.shutter.writing = self.shutter.writing.saturating_sub(1);
            said.push(match result {
                Ok(path) => format!("snapshot saved: {}", shown(&path)),
                Err(why) => format!("no snapshot: {why}"),
            });
        }
        for line in said {
            self.say(&line);
        }
    }
}

/// A path as a person reads it: the home folder as `~`.
#[cfg(not(target_arch = "wasm32"))]
fn shown(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home.and_then(|h| path.strip_prefix(h).ok().map(std::path::Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::photo::tests::{assert_tags, exposure};

    #[test]
    fn a_snapshot_is_cropped_to_the_picture() {
        let size = (4, 3);
        let pixels: Vec<u8> = (0..4 * 3 * 4).map(|v| v as u8).collect();
        let whole = Letterbox {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        assert_eq!(crop(pixels.clone(), size, whole), (pixels.clone(), 4, 3));
        let inner = Letterbox {
            x: 1,
            y: 1,
            width: 2,
            height: 1,
        };
        let (out, w, h) = crop(pixels, size, inner);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, (20..28).collect::<Vec<u8>>());
    }

    /// The whole path on a GPU: a frame drawn into the shutter's own
    /// texture, read back without waiting on the frame, cropped to the
    /// picture, encoded with its tags and written — and read back by an
    /// independent reader. The frame is the clear and nothing else: what
    /// was asked to be drawn is what the file holds.
    #[test]
    fn a_frame_taken_is_a_file_with_its_tags() {
        let gpu = match futures_executor::block_on(tuile_wgpu::GpuContext::headless()) {
            Ok(gpu) => gpu,
            Err(e) => {
                assert!(
                    std::env::var("TUILE_REQUIRE_GPU").is_err(),
                    "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
                );
                eprintln!("skipping: no usable adapter ({e})");
                return;
            }
        };
        let size = (160, 120);
        let targets = tuile_wgpu::FrameTargets::new(&gpu, tuile_wgpu::TEXTURE_FORMAT, size);
        // A picture held to 4:3 inside a frame with a bar: 120 × 90 at (20, 10).
        let picture = Letterbox {
            x: 20,
            y: 10,
            width: 120,
            height: 90,
        };
        let mut shutter = Shutter::new(SnapshotSettings::default());
        shutter.arm();
        assert!(shutter.take_armed() && !shutter.take_armed());
        // Nothing painted: the frame is its clear, of a known colour.
        let clear = wgpu::Color {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        };
        let taken = shutter.expose(
            &gpu,
            &targets,
            clear,
            (
                |_: &mut wgpu::RenderPass<'_>| {},
                tuile_wgpu::NOTHING_AFTER,
            ),
            exposure(),
            picture,
        );
        assert!(taken);
        assert!(shutter.busy());
        let developed = shutter.developed(&gpu, true);
        assert_eq!(developed.len(), 1);
        assert!(!shutter.busy());
        let Developed { exposure, rgba } = developed.into_iter().next().expect("one");
        assert_eq!(exposure.size, (120, 90), "the picture, not the frame");
        assert_eq!(rgba.len(), 120 * 90 * 4);
        assert!(
            rgba.as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 255, 255]),
            "the frame drawn, channels in RGBA order"
        );
        let bytes = crate::photo::develop(&rgba, &exposure, crate::photo::Format::default())
            .expect("develops");
        let dir = tempfile::tempdir().expect("a directory");
        let name = crate::photo::file_name(&exposure, crate::photo::Format::default());
        let path = crate::photo::keep(dir.path(), &name, &bytes).expect("kept");
        let back = std::fs::read(&path).expect("reads");
        assert_tags(&back, &exposure);
        let picture = image::load_from_memory(&back).expect("decodes").into_rgb8();
        assert_eq!(picture.dimensions(), (120, 90));
    }
}
