// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The knobs a session is started with, and the one it must be shut down by.
//!
//! Every one of these is an environment variable rather than a flag, because
//! they are all things one wants to change *between two runs of the same
//! command* while comparing — and a comparison whose two halves were launched
//! differently is not a comparison.

/// Logs to stderr; `RUST_LOG` overrides. From a terminal the default shows
/// tile streaming (`tuile_planetary=debug`) plus app-level info.
///
/// `quiet` is for a session started from an icon: its stderr is the system
/// log, where a line per tile is a flood nobody asked for and nobody reads.
/// Errors only, then — unless `RUST_LOG` says otherwise, as always.
pub(crate) fn init_tracing(quiet: bool) {
    use tracing_subscriber::EnvFilter;
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            if quiet {
                "error".into()
            } else {
                "info,tuile_planetary=debug".into()
            }
        }))
        .without_time()
        .with_target(false);
    // Android: no stderr anyone reads — logcat, a line at a time.
    #[cfg(target_os = "android")]
    subscriber
        .with_ansi(false)
        .with_writer(super::android::Logcat::new)
        .init();
    #[cfg(not(target_os = "android"))]
    subscriber.init();
}

/// The key the phone's settings page keeps the imagery compression under:
/// `off`, `auto`, `bc7` or `astc`.
pub(crate) const IMAGERY_COMPRESSION_KEY: &str = "imagery_compression";

/// How the imagery is held on the GPU: what `--imagery-compression` said,
/// else what the person chose in the system's settings page for the
/// application (a phone's *Settings › Tuile*, kept between launches), else
/// off. A value nobody can read is an error naming the four there are.
pub(crate) fn imagery_compression(
    flag: Option<tuile_wgpu::ImageryCompression>,
    host: &dyn crate::host::Host,
) -> Result<tuile_wgpu::ImageryCompression, String> {
    if let Some(choice) = flag {
        return Ok(choice);
    }
    match host
        .setting(IMAGERY_COMPRESSION_KEY)
        .filter(|v| !v.trim().is_empty())
    {
        Some(value) => tuile_wgpu::compression_named(&value),
        None => Ok(tuile_wgpu::ImageryCompression::Off),
    }
}

/// The scene's clock: what the command line said (`--time`, `--time-offset`,
/// `--time-rate`), or — where it named no instant — `TUILE_LIT_AT`, UTC
/// seconds since the Unix epoch, a session pinned to a stated moment for
/// comparing two runs. Neither: the present, in real time.
pub(crate) fn clock(
    flags: tuile_atmosphere::ClockSettings,
) -> anyhow::Result<tuile_atmosphere::ClockSettings> {
    if flags.start != tuile_atmosphere::Start::Now {
        return Ok(flags);
    }
    if let Ok(pinned) = std::env::var("TUILE_LIT_AT") {
        let seconds: f64 = pinned
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("TUILE_LIT_AT must be UTC seconds, got {pinned:?}"))?;
        tracing::info!("scene lit for the instant TUILE_LIT_AT={seconds}");
        return Ok(tuile_atmosphere::ClockSettings {
            start: tuile_atmosphere::Start::At(seconds),
            ..flags
        });
    }
    Ok(flags)
}

/// Which level is held in memory for the whole session, from `TUILE_PIN_LEVEL`.
///
/// The count is `4^level`, so the choice is a memory decision and a steep one:
/// level 5 is 1024 tiles over the globe and about 341 MiB of imagery if every
/// one were held; level 8 is 65 536 tiles and 21 GiB, which this machine does
/// not have. Unset means `default` — level 4 on a desktop, the backdrop's level
/// on a phone (see [`crate::profile::Profile::pinned_level`]); `none` disables
/// the pin.
///
/// Level 4 rather than 5 because the window is held shut until the whole pinned
/// pyramid is on the GPU, and the pyramid is a quarter of the size at each step
/// up: 2730 tiles through level 5 against 682 through level 4. What is bought
/// with the other three quarters is a fallback texel twice as wide — a blurrier
/// stand-in for the moment before real data lands, not a gap.
pub fn pinned_level(default: u32) -> Option<u32> {
    let Ok(setting) = std::env::var("TUILE_PIN_LEVEL") else {
        return Some(default);
    };
    if setting.trim().eq_ignore_ascii_case("none") {
        tracing::info!("no pinned level: every tile is evictable");
        return None;
    }
    match setting.trim().parse::<u32>() {
        Ok(level) => {
            let tiles = 4u64.saturating_pow(level);
            tracing::info!(
                level,
                tiles_over_the_globe = tiles,
                imagery_mib = tiles * 341 / 1024,
                "pinning a level in memory"
            );
            Some(level)
        }
        Err(_) => {
            tracing::warn!("TUILE_PIN_LEVEL={setting:?} is not a level; keeping the default");
            Some(default)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_wgpu::ImageryCompression;

    /// A machine whose settings page holds `page`, if anything.
    struct Page(Option<&'static str>);

    impl crate::host::Host for Page {
        fn var(&self, _name: &str) -> Option<String> {
            None
        }
        fn read(&self, path: &std::path::Path) -> std::io::Result<String> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                path.display().to_string(),
            ))
        }
        fn is_macos(&self) -> bool {
            true
        }
        fn setting(&self, key: &str) -> Option<String> {
            (key == IMAGERY_COMPRESSION_KEY)
                .then_some(self.0)
                .flatten()
                .map(str::to_owned)
        }
    }

    /// **Off unless the person chose**: the flag first, then the settings
    /// page, and a value nobody can read is said, not guessed at.
    #[test]
    fn compression_is_the_flag_then_the_settings_page_then_off() {
        assert_eq!(
            imagery_compression(None, &Page(None)),
            Ok(ImageryCompression::Off)
        );
        assert_eq!(
            imagery_compression(None, &Page(Some(""))),
            Ok(ImageryCompression::Off)
        );
        assert_eq!(
            imagery_compression(None, &Page(Some("auto"))),
            Ok(ImageryCompression::Auto)
        );
        assert_eq!(
            imagery_compression(Some(ImageryCompression::Off), &Page(Some("astc"))),
            Ok(ImageryCompression::Off),
            "the command line wins"
        );
        assert!(imagery_compression(None, &Page(Some("etc2"))).is_err());
    }
}
