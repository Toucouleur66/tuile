// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a phone's application bundle needs from this tool: its property list,
//! its icon and the colour of its launch screen.
//!
//! The bundle itself is laid out and signed by the platform's own build
//! (`crates/tuile-viewer/ios/`, an `xcodegen` project driven by `build.sh`):
//! on a phone the signature is a provisioning question that only the
//! platform's tools can answer. What goes *into* the bundle is decided here,
//! in Rust, and tested here — as it is for the desktop bundle.
//!
//! ```text
//! cargo run -p tuile-viewer-bundle -- phone --out crates/tuile-viewer/ios/Generated
//! ```

use std::path::Path;

use anyhow::Context;

use crate::Bundle;

/// The oldest system the application claims to run on: the first with
/// everything the viewer asks of it — a Metal 3 family GPU through wgpu, the
/// declarative launch screen, the process's memory limit as a call.
pub(crate) const MINIMUM_SYSTEM: &str = "16.0";

/// The launch screen's colour, and the globe's shell's: the same deep ocean
/// blue, so the step from the system's launch screen to the first frame is
/// no step at all. `BARE_GROUND` in `tuile-viewer/src/backdrop.rs`, in sRGB.
pub(crate) const LAUNCH_BACKGROUND: [f64; 3] = [0.09, 0.15, 0.24];

/// The name of the launch colour and of the icon set in the asset catalogue.
const LAUNCH_COLOR: &str = "LaunchBackground";
const APP_ICON: &str = "AppIcon";

/// `Info.plist` for the phone.
///
/// Every key is here for a reason the system gives it:
///
/// - the identity, with the version taken from the workspace;
/// - `UILaunchScreen` names the colour the system paints while the process
///   starts — the shell's, so launch screen and first frame are one picture;
/// - the orientations the viewer handles, every one of them;
/// - `UIRequiredDeviceCapabilities`: the renderer needs Metal;
/// - the location string, without which the system does not ask the person
///   and the answer to "where am I" is no;
/// - `UIFileSharingEnabled` with `LSSupportsOpeningDocumentsInPlace`, which
///   show the application's `Documents` in the Files application: the one
///   place a person can put the access token (see `tuile_viewer` `host`);
/// - `NSAppTransportSecurity` without a single exception: every tile source
///   the application uses is HTTPS. The tile traffic does not go through the
///   system's URL loading — it is the viewer's own TLS client — but the
///   declaration is what the system holds the application to, and it is the
///   truth;
/// - `ITSAppUsesNonExemptEncryption`: HTTPS only, which is exempt.
pub(crate) fn info_plist(bundle: &Bundle, version: &str) -> plist::Value {
    use plist::Value::{Array, Boolean, Dictionary, String as Text};
    let Bundle {
        name,
        identifier,
        executable,
        ..
    } = bundle;
    let text = |s: &str| Text(s.to_owned());
    let texts = |v: &[&str]| Array(v.iter().map(|s| text(s)).collect());
    let purpose = format!("{name} {}", crate::LOCATION_PURPOSE);

    let mut launch = plist::Dictionary::new();
    launch.insert("UIColorName".into(), text(LAUNCH_COLOR));
    let mut transport = plist::Dictionary::new();
    transport.insert("NSAllowsArbitraryLoads".into(), Boolean(false));

    let mut info = plist::Dictionary::new();
    for (key, value) in [
        ("CFBundleDevelopmentRegion", text("en")),
        ("CFBundleName", text(name)),
        ("CFBundleDisplayName", text(name)),
        ("CFBundleIdentifier", text(identifier)),
        ("CFBundleExecutable", text(executable)),
        ("CFBundlePackageType", text("APPL")),
        ("CFBundleInfoDictionaryVersion", text("6.0")),
        ("CFBundleShortVersionString", text(version)),
        ("CFBundleVersion", text(version)),
        ("LSRequiresIPhoneOS", Boolean(true)),
        // Xcode writes the same from the project's deployment target; said
        // here too so the two can be held to each other (see the tests).
        ("MinimumOSVersion", text(MINIMUM_SYSTEM)),
        ("UILaunchScreen", Dictionary(launch)),
        ("UIRequiredDeviceCapabilities", texts(&["arm64", "metal"])),
        (
            "UISupportedInterfaceOrientations",
            texts(&[
                "UIInterfaceOrientationPortrait",
                "UIInterfaceOrientationLandscapeLeft",
                "UIInterfaceOrientationLandscapeRight",
            ]),
        ),
        (
            "UISupportedInterfaceOrientations~ipad",
            texts(&[
                "UIInterfaceOrientationPortrait",
                "UIInterfaceOrientationPortraitUpsideDown",
                "UIInterfaceOrientationLandscapeLeft",
                "UIInterfaceOrientationLandscapeRight",
            ]),
        ),
        // White status-bar text over a dark globe.
        ("UIStatusBarStyle", text("UIStatusBarStyleLightContent")),
        ("UIViewControllerBasedStatusBarAppearance", Boolean(false)),
        ("NSLocationWhenInUseUsageDescription", text(&purpose)),
        ("UIFileSharingEnabled", Boolean(true)),
        ("LSSupportsOpeningDocumentsInPlace", Boolean(true)),
        ("NSAppTransportSecurity", Dictionary(transport)),
        ("ITSAppUsesNonExemptEncryption", Boolean(false)),
        (
            "NSHumanReadableCopyright",
            text("Copyright © lapoule.dev. MIT OR Apache-2.0."),
        ),
    ] {
        info.insert(key.into(), value);
    }
    Dictionary(info)
}

/// The asset catalogue: the icon, one 1024-pixel image that the platform's
/// compiler cuts into every size, and the launch colour.
fn asset_catalogue(master: &image::RgbaImage, out: &Path) -> anyhow::Result<()> {
    let catalogue = out.join("Assets.xcassets");
    if catalogue.exists() {
        std::fs::remove_dir_all(&catalogue)?;
    }
    let icons = catalogue.join(format!("{APP_ICON}.appiconset"));
    let colour = catalogue.join(format!("{LAUNCH_COLOR}.colorset"));
    std::fs::create_dir_all(&icons)?;
    std::fs::create_dir_all(&colour)?;
    let info = serde_json::json!({ "author": "tuile-viewer-bundle", "version": 1 });
    std::fs::write(
        catalogue.join("Contents.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "info": info }))?,
    )?;

    // An application icon may not be transparent: flattened onto the shell's
    // blue, which is the colour the master's own background tends toward.
    let side = 1024;
    let scaled = if master.width() == side {
        master.clone()
    } else {
        image::imageops::resize(master, side, side, image::imageops::FilterType::Lanczos3)
    };
    let background = LAUNCH_BACKGROUND.map(|c| (c * 255.0).round() as u8);
    let flat = image::RgbImage::from_fn(side, side, |x, y| {
        let p = scaled.get_pixel(x, y).0;
        let a = f32::from(p[3]) / 255.0;
        let mix = |c: u8, b: u8| (f32::from(c) * a + f32::from(b) * (1.0 - a)).round() as u8;
        image::Rgb([
            mix(p[0], background[0]),
            mix(p[1], background[1]),
            mix(p[2], background[2]),
        ])
    });
    flat.save(icons.join("icon-1024.png"))
        .context("writing the icon")?;
    std::fs::write(
        icons.join("Contents.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "images": [{
                "filename": "icon-1024.png",
                "idiom": "universal",
                "platform": "ios",
                "size": "1024x1024",
            }],
            "info": info,
        }))?,
    )?;

    let component = |c: f64| format!("{c:.3}");
    std::fs::write(
        colour.join("Contents.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "colors": [{
                "idiom": "universal",
                "color": {
                    "color-space": "srgb",
                    "components": {
                        "red": component(LAUNCH_BACKGROUND[0]),
                        "green": component(LAUNCH_BACKGROUND[1]),
                        "blue": component(LAUNCH_BACKGROUND[2]),
                        "alpha": "1.000",
                    },
                },
            }],
            "info": info,
        }))?,
    )?;
    Ok(())
}

/// Writes `Info.plist` and the asset catalogue under `out`.
pub(crate) fn resources(
    bundle: &Bundle,
    master: &image::RgbaImage,
    out: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    info_plist(bundle, env!("CARGO_PKG_VERSION")).to_file_xml(out.join("Info.plist"))?;
    asset_catalogue(master, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> plist::Dictionary {
        info_plist(&Bundle::default(), "1.2.3")
            .into_dictionary()
            .expect("a dictionary")
    }

    fn string(info: &plist::Dictionary, key: &str) -> Option<String> {
        info.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_owned)
    }

    #[test]
    fn the_phone_property_list_carries_the_identity_and_the_launch_screen() {
        let info = info();
        assert_eq!(
            string(&info, "CFBundleIdentifier").as_deref(),
            Some("dev.lapoule.tuile.viewer")
        );
        assert_eq!(
            string(&info, "CFBundleExecutable").as_deref(),
            Some("tuile-wgpu-viewer")
        );
        assert_eq!(
            string(&info, "CFBundleShortVersionString").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            info.get("LSRequiresIPhoneOS")
                .and_then(plist::Value::as_boolean),
            Some(true)
        );
        let launch = info
            .get("UILaunchScreen")
            .and_then(plist::Value::as_dictionary)
            .expect("a launch screen");
        assert_eq!(
            launch.get("UIColorName").and_then(plist::Value::as_string),
            Some(LAUNCH_COLOR)
        );
        let needs: Vec<_> = info
            .get("UIRequiredDeviceCapabilities")
            .and_then(plist::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(plist::Value::as_string)
            .collect();
        assert!(needs.contains(&"metal"));
    }

    /// Every tile source is HTTPS: no exception to the transport rules, and
    /// the location string that makes the system ask the person.
    #[test]
    fn the_phone_property_list_allows_no_plain_http_and_asks_for_the_location() {
        let info = info();
        let transport = info
            .get("NSAppTransportSecurity")
            .and_then(plist::Value::as_dictionary)
            .expect("the transport rules");
        assert_eq!(
            transport
                .get("NSAllowsArbitraryLoads")
                .and_then(plist::Value::as_boolean),
            Some(false)
        );
        assert_eq!(transport.len(), 1, "an exception crept in: {transport:?}");
        assert!(string(&info, "NSLocationWhenInUseUsageDescription")
            .is_some_and(|s| s.contains("current location")));
        // The token goes in Documents, which only these two keys show.
        for key in ["UIFileSharingEnabled", "LSSupportsOpeningDocumentsInPlace"] {
            assert_eq!(
                info.get(key).and_then(plist::Value::as_boolean),
                Some(true),
                "{key}"
            );
        }
    }

    /// The launch colour is the shell's: read from the viewer's source, so
    /// the two cannot drift apart.
    #[test]
    fn the_launch_screen_is_the_colour_of_the_shell() {
        let backdrop = include_str!("../../tuile-viewer/src/backdrop.rs");
        let [r, g, b] = LAUNCH_BACKGROUND;
        assert!(
            backdrop.contains(&format!(
                "const BARE_GROUND: [f32; 4] = [{r}, {g}, {b}, 1.0];"
            )),
            "the launch screen and the shell disagree"
        );
    }

    /// The property list and the Xcode project claim the same oldest system.
    #[test]
    fn the_plist_and_the_project_agree_on_the_oldest_system() {
        let project = include_str!("../../tuile-viewer/ios/project.yml");
        assert!(
            project.contains(&format!("iOS: \"{MINIMUM_SYSTEM}\"")),
            "project.yml deploys to another iOS than {MINIMUM_SYSTEM}"
        );
        assert_eq!(
            string(&info(), "MinimumOSVersion").as_deref(),
            Some(MINIMUM_SYSTEM)
        );
    }

    #[test]
    fn the_resources_are_a_plist_and_a_catalogue() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        resources(&Bundle::default(), &crate::icon::draw(64), dir.path()).expect("written");
        let read_back = plist::Value::from_file(dir.path().join("Info.plist")).expect("a plist");
        assert_eq!(
            read_back,
            info_plist(&Bundle::default(), env!("CARGO_PKG_VERSION"))
        );
        let icon = image::open(
            dir.path()
                .join("Assets.xcassets/AppIcon.appiconset/icon-1024.png"),
        )
        .expect("an icon");
        assert_eq!((icon.width(), icon.height()), (1024, 1024));
        assert!(
            !icon.color().has_alpha(),
            "an application icon may not be transparent"
        );
        for json in [
            "Assets.xcassets/Contents.json",
            "Assets.xcassets/AppIcon.appiconset/Contents.json",
            "Assets.xcassets/LaunchBackground.colorset/Contents.json",
        ] {
            let text = std::fs::read_to_string(dir.path().join(json)).expect(json);
            serde_json::from_str::<serde_json::Value>(&text).expect(json);
        }
    }
}
