// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Changing how the imagery is held on the GPU under a running session.
//!
//! One command ([`crate::steer::Command::Compression`]) — from the menu's
//! *Compress Imagery*, a steering URL (`compression?mode=off|auto|…`) or a
//! script — and the same code on a desktop, a phone and a page:
//!
//! 1. **the device**: the context holds imagery as the new choice from now
//!    on ([`tuile_wgpu::GpuContext::set_imagery_compression`]). Off lets go
//!    of the GPU encoder and its scratch; what the GPU already holds stays
//!    bound and drawn. The device was created able to hold every choice its
//!    adapter can ([`tuile_wgpu::switchable_features`]);
//! 2. **the loader**: both its plans — for a JPEG or PNG tile, and for one
//!    stored already compressed — so what is decoded from now on is marked
//!    for the new form, and charged in it by the budget;
//! 3. **the budget**: the encoder's scratch reserved off the drapes' limits,
//!    or given back ([`tuile_core::protocol::ClientMessage::Reserve`]);
//! 4. **the drapes**: the layer served is asked for again, a new imagery
//!    generation of the same pictures, through the path a change of layer
//!    takes — every drape refreshed **new before old**, the old one worn
//!    until its replacement is drawable. Never a bare tile;
//! 5. **the next session**: the choice is kept, as a tileset's switch is.

use super::App;

/// The name a person gives `choice`: what the flag, the settings page and
/// a steering URL take ([`tuile_wgpu::compression_named`] reads it back).
pub(crate) fn compression_name(choice: tuile_wgpu::ImageryCompression) -> &'static str {
    use tuile_core::texels::BlockFormat;
    use tuile_wgpu::ImageryCompression;
    match choice {
        ImageryCompression::Off => "off",
        ImageryCompression::Auto => "auto",
        ImageryCompression::Format(BlockFormat::Bc7) => "bc7",
        ImageryCompression::Format(BlockFormat::Astc4x4) => "astc",
    }
}

/// What a switch to `choice` asks of the loader and of the budget, on a
/// device whose context holds imagery as it now does: the plan it came to
/// there, the plan for tiles stored compressed, and the encoder's scratch.
pub(crate) fn switched(
    gpu: &mut tuile_wgpu::GpuContext,
    choice: tuile_wgpu::ImageryCompression,
) -> Switched {
    let plan = gpu.set_imagery_compression(choice);
    // Tiles stored compressed are kept as blocks only in a format the
    // device holds: off where the plan fell back to RGBA8.
    let stored = if plan == tuile_core::texels::ImageryPlan::RGBA8 {
        tuile_core::texels::StoredPlan::OFF
    } else {
        choice.stored(gpu.texture_caps())
    };
    Switched {
        plan,
        stored,
        reserved: tuile_wgpu::encode_reservation(plan, 1),
    }
}

/// See [`switched`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Switched {
    pub plan: tuile_core::texels::ImageryPlan,
    pub stored: tuile_core::texels::StoredPlan,
    pub reserved: tuile_core::budget::Bytes,
}

impl App {
    /// Holds the imagery as `choice` says from now on: see the module.
    pub(super) fn compress(&mut self, choice: tuile_wgpu::ImageryCompression) {
        let name = compression_name(choice);
        if choice == self.imagery_compression {
            self.say(&format!("imagery compression: {name} already"));
            return;
        }
        // Before the session, the loader and the budget the switch tells are
        // not there yet: the device would hold one form and the loader mark
        // another.
        let (Some(active), Some(session)) = (self.active.as_mut(), self.session.as_ref()) else {
            self.say("the globe is still starting");
            return;
        };
        let switched = switched(&mut active.gpu, choice);
        session.hold_imagery(switched.plan, switched.stored);
        tracing::info!(
            ?choice,
            held = tuile_wgpu::format_name(switched.plan.target),
            encode = ?switched.plan.encode,
            reserved = switched.reserved.0,
            "imagery compression switched"
        );
        super::gestures::Carry::send(
            self,
            tuile_core::protocol::ClientMessage::Reserve {
                class: tuile_core::budget::Class::Drape,
                bytes: switched.reserved.0,
            },
        );
        self.imagery_compression = choice;
        if self.keeps_choices {
            crate::shell::keep_compression(choice);
        }
        self.renew_the_drapes();
        self.say(&format!(
            "imagery compression: {name} ({})",
            tuile_wgpu::format_name(switched.plan.target)
        ));
    }

    /// Asks for the layer served again: every drape prepared anew, the old
    /// worn until the new one is drawable.
    fn renew_the_drapes(&mut self) {
        let Some(layer) = self.imagery.ask_again() else {
            return;
        };
        if let (Some(choice), Some(session)) =
            (self.imagery.layers.get(layer), self.session.as_ref())
        {
            session.switcher.ask(layer, choice);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::texels::{ImageryEncode, ImageryFormat, StoredPlan};
    use tuile_wgpu::{GpuContext, ImageryCompression};

    /// A context able to hold every choice its adapter can, opened on
    /// `choice`; a stated reason when there is no adapter
    /// (`TUILE_REQUIRE_GPU=1` turns that into a failure).
    fn gpu(choice: ImageryCompression) -> Option<GpuContext> {
        match pollster::block_on(GpuContext::headless_switchable(choice)) {
            Ok(gpu) => Some(gpu),
            Err(e) => {
                assert!(
                    std::env::var("TUILE_REQUIRE_GPU").is_err(),
                    "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
                );
                eprintln!("skipping GPU test: no usable adapter ({e})");
                None
            }
        }
    }

    /// The names are the ones every reader takes back.
    #[test]
    fn a_choice_is_named_as_it_is_read() {
        use tuile_core::texels::BlockFormat;
        for choice in [
            ImageryCompression::Off,
            ImageryCompression::Auto,
            ImageryCompression::Format(BlockFormat::Bc7),
            ImageryCompression::Format(BlockFormat::Astc4x4),
        ] {
            assert_eq!(
                tuile_wgpu::compression_named(compression_name(choice)),
                Ok(choice)
            );
        }
    }

    /// **Off is really off, and back on is really on**: switched off, the
    /// context holds RGBA8 with no encoder, no scratch and no reservation,
    /// and the loader is told RGBA8 for both kinds of tile; switched back,
    /// the device's block format, its encoder and its scratch reserved again
    /// — the numbers a session started on that choice has.
    #[test]
    fn the_switch_takes_the_device_the_loader_and_the_budget_with_it() {
        let Some(mut gpu) = gpu(ImageryCompression::Auto) else {
            return;
        };
        let caps = gpu.texture_caps();
        let auto = ImageryCompression::Auto.plan(caps);
        if auto.encode != ImageryEncode::Gpu {
            eprintln!("skipping: this adapter does not encode on its GPU ({caps:?})");
            return;
        }
        let started = gpu.imagery_plan();
        assert_eq!(started, auto);

        let off = switched(&mut gpu, ImageryCompression::Off);
        assert_eq!(off.plan, tuile_core::texels::ImageryPlan::RGBA8);
        assert_eq!(off.stored, StoredPlan::OFF);
        assert_eq!(off.reserved, tuile_core::budget::Bytes::ZERO);
        assert_eq!(gpu.imagery_plan().target, ImageryFormat::Rgba8);
        assert_eq!(gpu.imagery_scratch_bytes(), None, "the scratch let go");
        assert!(gpu.imagery_encode_stats().is_none(), "no encoder left");

        let on = switched(&mut gpu, ImageryCompression::Auto);
        assert_eq!(on.plan, auto);
        assert_eq!(on.stored, ImageryCompression::Auto.stored(caps));
        assert_eq!(on.reserved, tuile_wgpu::encode_reservation(auto, 1));
        assert!(on.reserved > tuile_core::budget::Bytes::ZERO);
        assert_eq!(gpu.imagery_plan(), auto);
        assert!(gpu.imagery_scratch_bytes().is_some(), "an encoder again");
    }

    /// A device not created to hold the choice holds RGBA8, and the loader
    /// and the budget are told that — never a format nothing samples.
    #[test]
    fn a_choice_the_device_cannot_hold_comes_to_rgba8() {
        let Some(mut gpu) = pollster::block_on(GpuContext::headless()).ok() else {
            return;
        };
        let to = switched(&mut gpu, ImageryCompression::Auto);
        if gpu.device.features().contains(
            wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::TEXTURE_COMPRESSION_ASTC,
        ) {
            return;
        }
        assert_eq!(to.plan, tuile_core::texels::ImageryPlan::RGBA8);
        assert_eq!(to.stored, StoredPlan::OFF);
        assert_eq!(to.reserved, tuile_core::budget::Bytes::ZERO);
    }
}
