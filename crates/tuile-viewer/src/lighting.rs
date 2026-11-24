// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Whether the ground is lit: the viewer's setting, **off by default**.
//!
//! Off, the ground is the imagery exactly as sampled — no sun shading the
//! slopes, no air between the eye and the ground. On, it is the physical
//! picture the film draws: the ground lit by the sun and the Moon where it is,
//! and seen through the air (`ground_seen`, a ray-march of the air at every
//! fragment). The sky is drawn either way.
//!
//! Off by default because the lit ground is most of the GPU's work at rest:
//! on a converged view at 2348 m, 2400×1436 px with MSAA×4, the lit ground
//! ran at 20.6 fps, 32.8 ms of GPU a frame (the GPU full), and the same view
//! unlit at 61 fps, 8.5 ms. The viewers are where the tiles are looked at;
//! the light is a choice a person makes, kept between sessions.
//!
//! Only the viewers choose. The renderer's own default, the film, the
//! headless runner (`crate::control`) and every golden picture stay lit.
//!
//! Every viewer reads it the same way: what was said for this session (the
//! `--lighting` flag, a page's `lighting` option, the phone's settings page),
//! else the person's last choice (the menu, `tuile://view?lighting=on`), else
//! off. The diagnostic views are untouched: `unlit` is the same picture as
//! lighting off, and the others draw what they always drew.

/// Whether the ground is lit. See the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lighting {
    /// The imagery as sampled: no sun, no air. The viewers' default.
    #[default]
    Off,
    /// Lit by the sun and the Moon, and seen through the air.
    On,
}

impl Lighting {
    /// `on` or `off` — and, as a phone's settings switch keeps a boolean,
    /// `1`/`0`, `true`/`false`, `yes`/`no` — in any case. Anything else is
    /// the sentence that says what it takes.
    pub fn named(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "on" | "1" | "true" | "yes" => Ok(Self::On),
            "off" | "0" | "false" | "no" => Ok(Self::Off),
            _ => Err(format!("lighting is on or off, not {text:?}")),
        }
    }

    /// `on` or `off`: what [`Lighting::named`] reads back.
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }

    pub fn is_on(self) -> bool {
        self == Self::On
    }

    /// The lighting a session opens with: what was said for it, else what
    /// the person last chose (`kept`), else off.
    pub(crate) fn chosen(said: Option<Self>, kept: impl FnOnce() -> Option<Self>) -> Self {
        said.or_else(kept).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Off unless a person asks**: the viewers' default.
    #[test]
    fn the_default_is_off() {
        assert_eq!(Lighting::default(), Lighting::Off);
        assert!(!Lighting::default().is_on());
        assert_eq!(Lighting::chosen(None, || None), Lighting::Off);
    }

    /// What was said wins over the kept choice, the kept choice over the
    /// default.
    #[test]
    fn said_then_kept_then_off() {
        assert_eq!(Lighting::chosen(None, || Some(Lighting::On)), Lighting::On);
        assert_eq!(
            Lighting::chosen(Some(Lighting::Off), || Some(Lighting::On)),
            Lighting::Off
        );
        assert_eq!(Lighting::chosen(Some(Lighting::On), || None), Lighting::On);
    }

    /// Each name reads back as itself; a settings switch's booleans read;
    /// anything else is said.
    #[test]
    fn the_names_round_trip() {
        for lighting in [Lighting::Off, Lighting::On] {
            assert_eq!(Lighting::named(lighting.name()), Ok(lighting));
        }
        assert_eq!(Lighting::named(" ON "), Ok(Lighting::On));
        assert_eq!(Lighting::named("1"), Ok(Lighting::On));
        assert_eq!(Lighting::named("0"), Ok(Lighting::Off));
        assert_eq!(Lighting::named("false"), Ok(Lighting::Off));
        let why = Lighting::named("dim").expect_err("not a lighting");
        assert!(why.contains("on or off") && why.contains("dim"), "{why}");
    }
}
