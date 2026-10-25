// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How a page holds its imagery, and how its worker learns it.
//!
//! **The host's choice, off by default** (`imageryCompression`). The page
//! resolves it against its own adapter, as a desktop does
//! ([`tuile_wgpu::plan_for`]), when it brings its device up: the device is
//! what samples the textures, so it is the device that decides. Its plan
//! then crosses to the session's worker in the start message
//! ([`super::control::ToWorker::Start`]), and from the worker's loader to the
//! decode workers inside each imagery job (`tuile_planetary::jobs`):
//!
//! - **WebGPU** with BC or ASTC: the GPU encodes. The decode workers send
//!   RGBA8 marked with the format it will be held in, and the page's pump
//!   encodes it on upload, under its own share of GPU time.
//! - **WebGL2** (no compute) with ASTC — most phones — or BPTC — desktops: the
//!   CPU encodes. Each decode worker encodes its tile to UASTC, transcodes it
//!   to the format, its whole chain built there, and the blocks cross.
//! - Neither: RGBA8, as without the option.
//!
//! A session started while compression is off goes at once; one that asks
//! for it waits for the page's device to say what it holds, a fraction of a
//! second, so that no tile is decoded under the wrong plan.

use serde::{Deserialize, Serialize};
use tuile_core::texels::{ImageryEncode, ImageryFormat, ImageryPlan, StoredPlan};

/// The two plans a session's loader decodes by: for a JPEG or PNG tile and
/// for a tile stored already compressed. As they cross to the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Held {
    pub target: Format,
    pub encode: Encode,
    pub uastc: Format,
    pub keep_bc7: bool,
    pub keep_astc: bool,
}

/// [`ImageryFormat`], as it crosses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Format {
    #[default]
    Rgba8,
    Bc7,
    Astc4x4,
}

/// [`ImageryEncode`], as it crosses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Encode {
    #[default]
    None,
    Gpu,
    Cpu,
    Transcode,
}

impl From<ImageryFormat> for Format {
    fn from(f: ImageryFormat) -> Self {
        match f {
            ImageryFormat::Rgba8 => Format::Rgba8,
            ImageryFormat::Bc7 => Format::Bc7,
            ImageryFormat::Astc4x4 => Format::Astc4x4,
        }
    }
}

impl From<Format> for ImageryFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Rgba8 => ImageryFormat::Rgba8,
            Format::Bc7 => ImageryFormat::Bc7,
            Format::Astc4x4 => ImageryFormat::Astc4x4,
        }
    }
}

impl From<ImageryEncode> for Encode {
    fn from(e: ImageryEncode) -> Self {
        match e {
            ImageryEncode::None => Encode::None,
            ImageryEncode::Gpu => Encode::Gpu,
            ImageryEncode::Cpu => Encode::Cpu,
            ImageryEncode::Transcode => Encode::Transcode,
        }
    }
}

impl From<Encode> for ImageryEncode {
    fn from(e: Encode) -> Self {
        match e {
            Encode::None => ImageryEncode::None,
            Encode::Gpu => ImageryEncode::Gpu,
            Encode::Cpu => ImageryEncode::Cpu,
            Encode::Transcode => ImageryEncode::Transcode,
        }
    }
}

impl Held {
    /// Uncompressed: what a page that did not ask, or whose device cannot,
    /// holds.
    pub(crate) const RGBA8: Held = Held {
        target: Format::Rgba8,
        encode: Encode::None,
        uastc: Format::Rgba8,
        keep_bc7: false,
        keep_astc: false,
    };

    pub(crate) fn new(plan: ImageryPlan, stored: StoredPlan) -> Self {
        Held {
            target: plan.target.into(),
            encode: plan.encode.into(),
            uastc: stored.uastc.into(),
            keep_bc7: stored.keep_bc7,
            keep_astc: stored.keep_astc,
        }
    }

    pub(crate) fn plan(self) -> ImageryPlan {
        ImageryPlan {
            target: self.target.into(),
            encode: self.encode.into(),
        }
    }

    pub(crate) fn stored(self) -> StoredPlan {
        StoredPlan {
            uastc: self.uastc.into(),
            keep_bc7: self.keep_bc7,
            keep_astc: self.keep_astc,
        }
    }
}

/// What `choice` comes to on a page's adapter, from what it reports: the
/// same resolution as a desktop's ([`tuile_wgpu::caps_of`],
/// [`ImageryCompression::plan`](tuile_wgpu::ImageryCompression::plan) and
/// [`stored`](tuile_wgpu::ImageryCompression::stored)).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn resolve(
    choice: tuile_wgpu::ImageryCompression,
    features: wgpu::Features,
    downlevel: wgpu::DownlevelFlags,
    limits: &wgpu::Limits,
) -> Held {
    let caps = tuile_wgpu::caps_of(features, downlevel, limits);
    Held::new(choice.plan(caps), choice.stored(caps))
}

#[cfg(target_arch = "wasm32")]
pub(crate) use device::{begin, held_by_the_device, viewer, when_the_device_holds};

/// The page's device's answer, for a session that waits on it.
#[cfg(target_arch = "wasm32")]
mod device {
    use std::cell::RefCell;

    use super::Held;

    /// One viewer's answer: whose it is, the answer once given, and the
    /// sessions waiting on it.
    #[derive(Default)]
    struct Slot {
        viewer: u64,
        held: Option<Held>,
        waiting: Vec<Box<dyn FnOnce(Held)>>,
    }

    thread_local! {
        static SLOT: RefCell<Slot> = RefCell::default();
    }

    /// A new viewer: what an earlier one's device said no longer applies.
    /// Returns the viewer's number, which its device answers under.
    pub(crate) fn begin() -> u64 {
        SLOT.with(|s| {
            let mut s = s.borrow_mut();
            let viewer = s.viewer + 1;
            *s = Slot {
                viewer,
                ..Slot::default()
            };
            viewer
        })
    }

    /// The viewer being brought up: what its device answers under.
    pub(crate) fn viewer() -> u64 {
        SLOT.with(|s| s.borrow().viewer)
    }

    /// The device of viewer `viewer` holds imagery as `held`: every session
    /// waiting on it starts. An earlier viewer's late answer is dropped.
    pub(crate) fn held_by_the_device(viewer: u64, held: Held) {
        let waiting = SLOT.with(|s| {
            let mut s = s.borrow_mut();
            if s.viewer != viewer {
                return Vec::new();
            }
            s.held = Some(held);
            std::mem::take(&mut s.waiting)
        });
        for then in waiting {
            then(held);
        }
    }

    /// Runs `then` with the device's answer: now if it has given it, when it
    /// does otherwise.
    pub(crate) fn when_the_device_holds(then: impl FnOnce(Held) + 'static) {
        let mut then: Option<Box<dyn FnOnce(Held)>> = Some(Box::new(then));
        let now = SLOT.with(|s| {
            let mut s = s.borrow_mut();
            if s.held.is_none() {
                s.waiting.extend(then.take());
            }
            s.held
        });
        // Not called while the slot is borrowed: `then` may ask it again.
        if let (Some(held), Some(then)) = (now, then) {
            then(held);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::texels::BlockFormat;
    use tuile_wgpu::ImageryCompression;

    /// A WebGPU adapter: compute, and the limits the encoder needs.
    fn webgpu(features: wgpu::Features) -> (wgpu::Features, wgpu::DownlevelFlags, wgpu::Limits) {
        (
            features,
            wgpu::DownlevelFlags::all(),
            wgpu::Limits::downlevel_defaults(),
        )
    }

    /// A WebGL2 context: no compute, WebGL2's limits.
    fn webgl2(features: wgpu::Features) -> (wgpu::Features, wgpu::DownlevelFlags, wgpu::Limits) {
        (
            features,
            wgpu::DownlevelFlags::all() - wgpu::DownlevelFlags::COMPUTE_SHADERS,
            wgpu::Limits::downlevel_webgl2_defaults(),
        )
    }

    /// Every page a browser gives, as its adapter reports itself, under
    /// `auto`: the GPU encodes where WebGPU runs it, the decode workers where
    /// WebGL2 samples a block format, and nothing where neither.
    #[test]
    fn the_page_plan_matrix() {
        let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
        let astc = wgpu::Features::TEXTURE_COMPRESSION_ASTC;
        let none = wgpu::Features::empty();
        let cases = [
            (
                "WebGPU desktop, texture-compression-bc",
                webgpu(bc),
                (Format::Bc7, Encode::Gpu),
            ),
            (
                "WebGPU on Apple, -bc and -astc",
                webgpu(bc | astc),
                (Format::Bc7, Encode::Gpu),
            ),
            (
                "WebGPU phone, texture-compression-astc",
                webgpu(astc),
                (Format::Astc4x4, Encode::Gpu),
            ),
            (
                "WebGPU, no block format",
                webgpu(none),
                (Format::Rgba8, Encode::None),
            ),
            (
                "WebGL2 phone, WEBGL_compressed_texture_astc",
                webgl2(astc),
                (Format::Astc4x4, Encode::Cpu),
            ),
            (
                "WebGL2 desktop, EXT_texture_compression_bptc and s3tc/rgtc",
                webgl2(bc),
                (Format::Bc7, Encode::Cpu),
            ),
            (
                "WebGL2 with both",
                webgl2(bc | astc),
                (Format::Astc4x4, Encode::Cpu),
            ),
            (
                "WebGL2, no block format",
                webgl2(none),
                (Format::Rgba8, Encode::None),
            ),
        ];
        for (page, (features, downlevel, limits), (target, encode)) in cases {
            let held = resolve(ImageryCompression::Auto, features, downlevel, &limits);
            assert_eq!((held.target, held.encode), (target, encode), "{page}");
            let off = resolve(ImageryCompression::Off, features, downlevel, &limits);
            assert_eq!(off, Held::RGBA8, "{page}: off is RGBA8 whatever the device");
        }
        // A named format where only the other is sampled is RGBA8.
        let (f, d, l) = webgl2(astc);
        assert_eq!(
            resolve(ImageryCompression::Format(BlockFormat::Bc7), f, d, &l).plan(),
            ImageryPlan::RGBA8
        );
        let (f, d, l) = webgl2(bc);
        assert_eq!(
            resolve(ImageryCompression::Format(BlockFormat::Bc7), f, d, &l).plan(),
            ImageryPlan {
                target: ImageryFormat::Bc7,
                encode: ImageryEncode::Cpu
            }
        );
    }

    /// The plans cross to the worker and back unchanged.
    #[test]
    fn the_plans_cross_whole() {
        let (f, d, l) = webgl2(wgpu::Features::TEXTURE_COMPRESSION_ASTC);
        let held = resolve(ImageryCompression::Auto, f, d, &l);
        let back: Held =
            serde_json::from_str(&serde_json::to_string(&held).expect("written")).expect("read");
        assert_eq!(back, held);
        assert_eq!(Held::new(back.plan(), back.stored()), held);
        assert_eq!(back.stored().uastc, ImageryFormat::Astc4x4);
        assert_eq!(Held::default(), Held::RGBA8);
    }
}
