// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A snapshot of the view, as a picture file a photo library can read: the
//! frame's pixels, encoded, and the view's numbers in its EXIF.
//!
//! What is here has no GPU and no window: the settings, the facts a picture
//! is taken with ([`Exposure`]), its file name, and the bytes of the file
//! ([`develop`]). Taking the frame is `app::shutter`'s; keeping the file is
//! [`keep`] on a desktop or a phone, a download on a page.
//!
//! # Why JPEG, by default
//!
//! EXIF is native to JPEG: Photos, the Finder, Preview, every browser's
//! image viewer and `exiftool` read its GPS position, direction and date
//! without being asked. A PNG carries EXIF in an `eXIf` chunk, which is
//! newer and not read everywhere a JPEG's is (written with the `png`
//! crate the image crate already brings, the EXIF still `little_exif`'s). At quality 95 a frame of the
//! globe is within a level or two of the original per channel and about a
//! tenth of the PNG's size. A lossless PNG stays one setting away
//! (`[snapshot] format = "png"`).
//!
//! # The EXIF
//!
//! Written with `little_exif` (MIT OR Apache-2.0, pure Rust, no `unsafe`,
//! builds for `wasm32-unknown-unknown`, writes to a buffer as well as a
//! file, maintained): the one crate of the ecosystem that both writes the
//! GPS directory and works on a page. No TIFF is encoded here.
//!
//! The eye is the GPS position — to 1e-5 of an arc second, about 0.3 mm —
//! its height above the ellipsoid the GPS altitude, the view's heading the
//! image direction, and the ground point at the centre of the picture the
//! destination. What has no standard tag (pitch, roll, the height above the
//! ground, the scene's clock, the imagery, the budget) is in the user
//! comment, as JSON. **Never a token**: the JSON is built field by field from
//! the view's numbers and the layers' public names, and nothing in it comes
//! from a credential.

use std::fmt::Write as _;

use little_exif::exif_tag::ExifTag;
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;

/// What the file's `Make` says.
pub(crate) const MAKE: &str = "Tuile";

/// The JPEG quality a snapshot is encoded at unless the settings say
/// otherwise: within a level or two of the frame per channel.
pub(crate) const DEFAULT_QUALITY: u8 = 95;

/// How a snapshot is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Jpeg {
        quality: u8,
    },
    /// Lossless, with its EXIF in an `eXIf` chunk.
    Png,
}

impl Default for Format {
    fn default() -> Self {
        Self::Jpeg {
            quality: DEFAULT_QUALITY,
        }
    }
}

impl Format {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Jpeg { .. } => "jpg",
            Self::Png => "png",
        }
    }

    // A page's download says it; a desktop's file says it by its name.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn mime(self) -> &'static str {
        match self {
            Self::Jpeg { .. } => "image/jpeg",
            Self::Png => "image/png",
        }
    }
}

/// What a snapshot draws over the globe. The globe — ground, 3D Tiles,
/// sky — always; each of these only when the settings list it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Layers {
    /// A host's shapes in the scene: tracks, markers.
    pub shapes: bool,
    /// A host's heads-up display, drawn over the picture.
    pub hud: bool,
    /// The viewer's own on-screen controls, labels and panels.
    pub controls: bool,
}

impl Layers {
    /// The names the settings list them by.
    pub(crate) const NAMES: [&str; 3] = ["shapes", "hud", "controls"];

    /// Layers from their names; the name that is not one, refused.
    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut out = Self::default();
        for name in names {
            match name.trim() {
                "shapes" => out.shapes = true,
                "hud" => out.hud = true,
                "controls" => out.controls = true,
                other => {
                    return Err(format!(
                        "{other:?} is not a layer; the layers are {}",
                        Self::NAMES.join(", ")
                    ))
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn names(self) -> Vec<&'static str> {
        [self.shapes, self.hud, self.controls]
            .into_iter()
            .zip(Self::NAMES)
            .filter_map(|(on, name)| on.then_some(name))
            .collect()
    }
}

/// `[snapshot]` of the configuration file: what it says, checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotSettings {
    /// Where the pictures go, as written (`~` for the home directory).
    pub folder: Option<String>,
    pub format: Option<Format>,
    pub layers: Option<Layers>,
}

/// The folder a snapshot goes into: the settings', else `Pictures/Tuile` in
/// the home directory. `None` where there is no home to find.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn folder(settings: &SnapshotSettings) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match settings.folder.as_deref() {
        Some("~") => home,
        Some(path) => match path.strip_prefix("~/") {
            Some(rest) => home.map(|h| h.join(rest)),
            None => Some(std::path::PathBuf::from(path)),
        },
        None => home.map(|h| h.join("Pictures").join("Tuile")),
    }
}

/// The ground point at the centre of the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Target {
    pub lon: f64,
    pub lat: f64,
    /// Above the ellipsoid, metres.
    pub height: f64,
    /// From the eye, metres.
    pub range: f64,
}

/// The imagery layer draped, by its public names.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Imagery {
    pub key: String,
    pub name: String,
    pub attribution: String,
    pub asset: i64,
    /// The layer its far tiles are draped from: asset, gap, hysteresis.
    pub substitute: Option<(i64, u32, u32)>,
}

impl Imagery {
    pub(crate) fn of(layer: &crate::embed::ImageryChoice) -> Self {
        Self {
            key: layer.key.clone(),
            name: layer.name.clone(),
            attribution: layer.attribution.clone(),
            asset: layer.asset,
            substitute: layer
                .substitute
                .as_ref()
                .map(|s| (s.asset, s.gap, s.hysteresis)),
        }
    }
}

/// Everything a snapshot is taken with: the view's numbers, read on the
/// frame it is taken of.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Exposure {
    /// When it was taken: Unix seconds, UTC, by the wall clock.
    pub taken: f64,
    /// The eye: degrees, and metres above the ellipsoid.
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
    /// The ground under the eye, metres above the ellipsoid, when the
    /// terrain there is known.
    pub ground: Option<f64>,
    pub target: Option<Target>,
    /// Degrees: clockwise from true north; below the horizon; about the
    /// view direction, clockwise as seen from behind the eye.
    pub heading: f64,
    pub pitch: f64,
    pub roll: f64,
    /// The vertical field of view, degrees.
    pub fovy: f64,
    /// The picture, pixels: the view without the bars around it.
    pub size: (u32, u32),
    /// The scene's clock: the instant drawn (Unix seconds) and its rate.
    pub scene: f64,
    pub rate: f64,
    pub imagery: Option<Imagery>,
    /// A fallback rule given by hand (`tuile://fallback`), if any: `None`
    /// none given, `Some(None)` switched off, `Some(Some((gap,
    /// hysteresis)))`.
    pub fallback: Option<Option<(u32, u32)>>,
    /// How the imagery is held on the GPU: `BC7`, `RGBA8`.
    pub compression: String,
    pub terrain: Option<i64>,
    /// The traversal's screen-space error threshold, pixels.
    pub sse: f64,
    /// What the budget holds and what the frame drew.
    pub budget: Option<serde_json::Value>,
    /// The diagnostic view drawn (`Normal` for the picture itself), and the
    /// wireframe.
    pub view: &'static str,
    pub wireframe: bool,
    /// Whether the ground was lit (`crate::lighting`): `on` or `off`.
    pub lighting: &'static str,
    /// The GPU and its backend.
    pub adapter: String,
    pub backend: String,
    pub layers: Layers,
}

/// `tuile-viewer 0.17.1 (v0.17.1-161-gd0d9f674)`: the build, by its `git
/// describe` where it was built from a checkout.
pub(crate) fn software() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match tuile_telemetry::build::REF_NAME {
        Some(describe) => format!("tuile-viewer {version} ({describe})"),
        None => format!("tuile-viewer {version}"),
    }
}

/// `2026-11-23T19-12-26Z_45.4603N_27.2904E.jpg`: when, by the wall clock in
/// UTC, and where the eye was. Colons are not in it: the Finder shows them
/// as slashes.
pub(crate) fn file_name(e: &Exposure, format: Format) -> String {
    let when = tuile_atmosphere::clock::format_rfc3339(e.taken).replace(':', "-");
    format!(
        "{when}_{:.4}{}_{:.4}{}.{}",
        e.lat.abs(),
        if e.lat < 0.0 { 'S' } else { 'N' },
        e.lon.abs(),
        if e.lon < 0.0 { 'W' } else { 'E' },
        format.extension()
    )
}

/// `45.4603N 27.2904E`.
fn place(lat: f64, lon: f64) -> String {
    format!(
        "{:.4}{} {:.4}{}",
        lat.abs(),
        if lat < 0.0 { 'S' } else { 'N' },
        lon.abs(),
        if lon < 0.0 { 'W' } else { 'E' }
    )
}

/// The 35 mm equivalent focal length of a vertical field of view `fovy`
/// (degrees) on a picture of `size`: the focal length that gives a full
/// frame's diagonal (43.27 mm) the picture's diagonal field of view — EXIF's
/// own definition.
pub(crate) fn focal_35mm(fovy: f64, size: (u32, u32)) -> f64 {
    let aspect = f64::from(size.0) / f64::from(size.1.max(1));
    let half_diagonal = (fovy.to_radians() * 0.5).tan() * (1.0 + aspect * aspect).sqrt();
    let full_frame_half_diagonal = 0.5 * (36.0f64.hypot(24.0));
    full_frame_half_diagonal / half_diagonal
}

/// The state, as the user comment's JSON.
pub(crate) fn state_json(e: &Exposure) -> serde_json::Value {
    let solar = tuile_atmosphere::clock::solar_time_hours(e.scene, e.lon.to_radians());
    let minutes = (solar * 60.0).floor() as u32;
    let aspect = f64::from(e.size.0) / f64::from(e.size.1.max(1));
    let fovx = 2.0
        * ((e.fovy.to_radians() * 0.5).tan() * aspect)
            .atan()
            .to_degrees();
    serde_json::json!({
        "taken_utc": tuile_atmosphere::clock::format_rfc3339(e.taken),
        "eye": {
            "lon": e.lon,
            "lat": e.lat,
            "height_ellipsoid_m": e.height,
            "height_above_ground_m": e.ground.map(|g| e.height - g),
            "ground_ellipsoid_m": e.ground,
        },
        "target": e.target.map(|t| serde_json::json!({
            "lon": t.lon,
            "lat": t.lat,
            "height_ellipsoid_m": t.height,
            "range_m": t.range,
        })),
        "heading_deg": e.heading,
        "pitch_deg": e.pitch,
        "pitch_convention": "below the horizon",
        "roll_deg": e.roll,
        "fov_vertical_deg": e.fovy,
        "fov_horizontal_deg": fovx,
        "viewport": { "width": e.size.0, "height": e.size.1 },
        "scene": {
            "instant_utc": tuile_atmosphere::clock::format_rfc3339(e.scene),
            "solar": format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60),
            "rate": e.rate,
        },
        "imagery": e.imagery.as_ref().map(|i| serde_json::json!({
            "key": i.key,
            "name": i.name,
            "attribution": i.attribution,
            "asset": i.asset,
            "substitute": i.substitute.map(|(asset, gap, hysteresis)| serde_json::json!({
                "asset": asset, "gap": gap, "hysteresis": hysteresis,
            })),
        })),
        "fallback_override": e.fallback.map(|rule| match rule {
            None => serde_json::json!("off"),
            Some((gap, hysteresis)) => serde_json::json!({ "gap": gap, "hysteresis": hysteresis }),
        }),
        "imagery_compression": e.compression,
        "terrain_asset": e.terrain,
        "sse_px": e.sse,
        "budget": e.budget,
        "view": e.view,
        "wireframe": e.wireframe,
        "lighting": e.lighting,
        "layers": e.layers.names(),
        "adapter": e.adapter,
        "backend": e.backend,
        "software": software(),
        "revision": tuile_telemetry::build::REVISION,
    })
}

/// JSON with every character outside ASCII written `\uXXXX`: still the same
/// JSON, and a user comment declared ASCII that is ASCII.
fn ascii_json(value: &serde_json::Value) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

/// `value` as a rational over `den`, the denominator made smaller until the
/// numerator fits.
fn rational(value: f64, mut den: u32) -> uR64 {
    let value = value.max(0.0);
    while den > 1 && value * f64::from(den) > f64::from(u32::MAX) {
        den /= 10;
    }
    uR64 {
        nominator: (value * f64::from(den)).round().min(f64::from(u32::MAX)) as u32,
        denominator: den,
    }
}

/// Degrees, as EXIF's degrees, minutes and seconds: whole degrees and
/// minutes, seconds to 1e-5 — 1.4e-9° at worst.
fn dms(degrees: f64) -> Vec<uR64> {
    const PER_SECOND: u64 = 100_000;
    let total = (degrees.abs() * 3600.0 * PER_SECOND as f64).round() as u64;
    let d = total / (3600 * PER_SECOND);
    let m = total / (60 * PER_SECOND) % 60;
    let s = total % (60 * PER_SECOND);
    vec![
        uR64 {
            nominator: d as u32,
            denominator: 1,
        },
        uR64 {
            nominator: m as u32,
            denominator: 1,
        },
        uR64 {
            nominator: s as u32,
            denominator: PER_SECOND as u32,
        },
    ]
}

/// The tags of a snapshot of `e`.
pub(crate) fn tags(e: &Exposure) -> Vec<ExifTag> {
    let taken = tuile_atmosphere::clock::format_rfc3339(e.taken);
    // `2026-11-23T19:12:26Z` → `2026:11:23` and `19:12:26`.
    let date = taken[..10].replace('-', ":");
    let time = &taken[11..19];
    let hms: Vec<uR64> = time
        .split(':')
        .map(|n| uR64 {
            nominator: n.parse().unwrap_or(0),
            denominator: 1,
        })
        .collect();
    let (w, h) = e.size;
    let focal = focal_35mm(e.fovy, e.size);
    let mut description = format!(
        "{}, {:.0} m above the ellipsoid, heading {:.1}°, pitch {:.1}° below the horizon",
        place(e.lat, e.lon),
        e.height,
        e.heading,
        e.pitch
    );
    if let Some(imagery) = &e.imagery {
        description = format!("{description} — {}", imagery.name);
    }
    let mut comment = b"ASCII\0\0\0".to_vec();
    comment.extend_from_slice(ascii_json(&state_json(e)).as_bytes());
    let mut tags = vec![
        ExifTag::Make(MAKE.to_owned()),
        ExifTag::Model(format!("{} ({})", e.adapter, e.backend)),
        ExifTag::Software(software()),
        ExifTag::ImageDescription(description),
        ExifTag::DateTimeOriginal(format!("{date} {time}")),
        ExifTag::CreateDate(format!("{date} {time}")),
        ExifTag::OffsetTimeOriginal("+00:00".to_owned()),
        ExifTag::OffsetTime("+00:00".to_owned()),
        ExifTag::ExifImageWidth(vec![w]),
        ExifTag::ExifImageHeight(vec![h]),
        ExifTag::FocalLength(vec![rational(focal, 1000)]),
        ExifTag::FocalLengthIn35mmFormat(vec![focal.round().clamp(1.0, 65_535.0) as u16]),
        ExifTag::UserComment(comment),
        ExifTag::GPSVersionID(vec![2, 3, 0, 0]),
        ExifTag::GPSLatitudeRef(if e.lat < 0.0 { "S" } else { "N" }.to_owned()),
        ExifTag::GPSLatitude(dms(e.lat)),
        ExifTag::GPSLongitudeRef(if e.lon < 0.0 { "W" } else { "E" }.to_owned()),
        ExifTag::GPSLongitude(dms(e.lon)),
        // Above the ellipsoid; the height above the ground is the comment's.
        ExifTag::GPSAltitudeRef(vec![u8::from(e.height < 0.0)]),
        ExifTag::GPSAltitude(vec![rational(e.height.abs(), 1000)]),
        ExifTag::GPSImgDirectionRef("T".to_owned()),
        ExifTag::GPSImgDirection(vec![rational(e.heading.rem_euclid(360.0), 1_000_000)]),
        ExifTag::GPSMapDatum("WGS-84".to_owned()),
        ExifTag::GPSDateStamp(date.clone()),
        ExifTag::GPSTimeStamp(hms),
    ];
    if let Some(t) = e.target {
        tags.extend([
            ExifTag::GPSDestLatitudeRef(if t.lat < 0.0 { "S" } else { "N" }.to_owned()),
            ExifTag::GPSDestLatitude(dms(t.lat)),
            ExifTag::GPSDestLongitudeRef(if t.lon < 0.0 { "W" } else { "E" }.to_owned()),
            ExifTag::GPSDestLongitude(dms(t.lon)),
            ExifTag::GPSDestBearingRef("T".to_owned()),
            ExifTag::GPSDestBearing(vec![rational(e.heading.rem_euclid(360.0), 1_000_000)]),
            ExifTag::GPSDestDistanceRef("K".to_owned()),
            ExifTag::GPSDestDistance(vec![rational(t.range / 1000.0, 1_000_000)]),
        ]);
    }
    tags
}

/// The file of a snapshot: `rgba` (tightly packed, sRGB, `e.size`) encoded
/// as `format` says, with the tags of `e`.
pub(crate) fn develop(rgba: &[u8], e: &Exposure, format: Format) -> Result<Vec<u8>, String> {
    let (w, h) = e.size;
    if rgba.len() != w as usize * h as usize * 4 {
        return Err(format!(
            "{} bytes is not a picture of {w} × {h}",
            rgba.len()
        ));
    }
    let mut metadata = Metadata::new();
    for tag in tags(e) {
        metadata.set_tag(tag);
    }
    let mut out = Vec::new();
    match format {
        Format::Jpeg { quality } => {
            // No alpha in a JPEG: the frame is opaque anyway.
            let rgb: Vec<u8> = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
                .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| e.to_string())?;
            metadata
                .write_to_vec(&mut out, FileExtension::JPEG)
                .map_err(|e| format!("the EXIF could not be written: {e}"))?;
        }
        Format::Png => {
            // The EXIF in the chunk the PNG specification gives it, `eXIf`,
            // before the pixels: what readers of EXIF look for in a PNG.
            // (`little_exif` writes a PNG's as ImageMagick's zTXt text
            // profile, which only some of them read.) The TIFF stream is
            // still `little_exif`'s; `png` only frames it as a chunk.
            let exif = metadata
                .encode()
                .map_err(|e| format!("the EXIF could not be written: {e}"))?;
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
            let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
            writer
                .write_chunk(png::chunk::ChunkType(*b"eXIf"), &exif)
                .map_err(|e| e.to_string())?;
            writer.write_image_data(rgba).map_err(|e| e.to_string())?;
            writer.finish().map_err(|e| e.to_string())?;
        }
    }
    Ok(out)
}

/// Writes `bytes` as `name` into `folder`, made if it is not there; the
/// path written.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn keep(
    folder: &std::path::Path,
    name: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(folder)?;
    let path = folder.join(name);
    // Beside, then renamed: a library watching the folder never sees half
    // a picture.
    let part = folder.join(format!(".{name}.part"));
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, &path)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A view of the Danube's mouth from 2.5 km, looking south-east.
    pub(crate) fn exposure() -> Exposure {
        Exposure {
            // 2026-11-23T19:12:26Z
            taken: 1_795_461_146.25,
            lon: 27.290_412_345_678,
            lat: 45.460_312_345_678,
            height: 2_512.345,
            ground: Some(87.5),
            target: Some(Target {
                lon: 27.31,
                lat: 45.44,
                height: 60.0,
                range: 3_456.0,
            }),
            heading: 135.123_456,
            pitch: 32.5,
            roll: -0.25,
            fovy: 45.0,
            size: (96, 64),
            scene: 1_795_430_000.0,
            rate: 1.0,
            imagery: Some(Imagery {
                key: "aerial".into(),
                name: "Aerial — © suppliers".into(),
                attribution: "© Microsoft".into(),
                asset: 2,
                substitute: Some((-1001, 4, 1)),
            }),
            fallback: None,
            compression: "BC7".into(),
            terrain: Some(1),
            sse: 1.0,
            budget: Some(serde_json::json!({ "drawn_by_level": [[12, 40]] })),
            view: "Normal",
            wireframe: false,
            lighting: "off",
            adapter: "Test GPU".into(),
            backend: "Metal".into(),
            layers: Layers::default(),
        }
    }

    /// A smooth sky over ground.
    pub(crate) fn frame((w, h): (u32, u32)) -> Vec<u8> {
        (0..w * h)
            .flat_map(|p| {
                let (x, y) = (p % w, p / w);
                if y < h / 2 {
                    [10, (20 + y * 2) as u8, 120, 255]
                } else {
                    [(90 + x % 7) as u8, (100 + y % 5) as u8, 80, 255]
                }
            })
            .collect()
    }

    fn rational_of(field: &exif::Field, index: usize) -> f64 {
        match &field.value {
            exif::Value::Rational(v) => v[index].to_f64(),
            other => panic!("{:?} is not rational: {other:?}", field.tag),
        }
    }

    fn degrees(field: &exif::Field) -> f64 {
        rational_of(field, 0) + rational_of(field, 1) / 60.0 + rational_of(field, 2) / 3600.0
    }

    fn ascii(field: &exif::Field) -> String {
        match &field.value {
            exif::Value::Ascii(v) => v
                .iter()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(""),
            other => panic!("{:?} is not text: {other:?}", field.tag),
        }
    }

    /// The tags of a file, read back by a reader that is not the writer.
    pub(crate) fn read_back(bytes: &[u8]) -> exif::Exif {
        exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(bytes))
            .expect("the file carries EXIF a reader can read")
    }

    /// What the user comment holds, as JSON.
    pub(crate) fn comment(read: &exif::Exif) -> serde_json::Value {
        let field = read
            .get_field(exif::Tag::UserComment, exif::In::PRIMARY)
            .expect("a user comment");
        let exif::Value::Undefined(bytes, _) = &field.value else {
            panic!("the user comment is not undefined bytes");
        };
        assert_eq!(&bytes[..8], b"ASCII\0\0\0");
        assert!(bytes[8..].is_ascii());
        serde_json::from_slice(&bytes[8..]).expect("the comment is JSON")
    }

    /// Checks every tag the spec of the snapshot names against `e`.
    pub(crate) fn assert_tags(bytes: &[u8], e: &Exposure) {
        let read = read_back(bytes);
        let field = |tag| {
            read.get_field(tag, exif::In::PRIMARY)
                .unwrap_or_else(|| panic!("{tag} is missing"))
        };
        use exif::Tag;
        // The eye, to a millionth of a degree.
        assert!((degrees(field(Tag::GPSLatitude)) - e.lat).abs() < 1e-6);
        assert_eq!(ascii(field(Tag::GPSLatitudeRef)), "N");
        assert!((degrees(field(Tag::GPSLongitude)) - e.lon).abs() < 1e-6);
        assert_eq!(ascii(field(Tag::GPSLongitudeRef)), "E");
        assert!((rational_of(field(Tag::GPSAltitude), 0) - e.height).abs() < 1e-3);
        assert_eq!(
            field(Tag::GPSAltitudeRef).value.get_uint(0),
            Some(0),
            "above the reference"
        );
        assert!((rational_of(field(Tag::GPSImgDirection), 0) - e.heading).abs() < 1e-6);
        assert_eq!(ascii(field(Tag::GPSImgDirectionRef)), "T");
        assert_eq!(ascii(field(Tag::GPSMapDatum)), "WGS-84");
        assert_eq!(ascii(field(Tag::GPSDateStamp)), "2026:11:23");
        assert_eq!(rational_of(field(Tag::GPSTimeStamp), 0), 19.0);
        assert_eq!(rational_of(field(Tag::GPSTimeStamp), 1), 12.0);
        assert_eq!(rational_of(field(Tag::GPSTimeStamp), 2), 26.0);
        let target = e.target.expect("a target");
        assert!((degrees(field(Tag::GPSDestLatitude)) - target.lat).abs() < 1e-6);
        assert!((degrees(field(Tag::GPSDestLongitude)) - target.lon).abs() < 1e-6);
        // When, in UTC, said so.
        assert_eq!(ascii(field(Tag::DateTimeOriginal)), "2026:11:23 19:12:26");
        assert_eq!(ascii(field(Tag::OffsetTimeOriginal)), "+00:00");
        // Who and what.
        assert_eq!(ascii(field(Tag::Make)), "Tuile");
        assert_eq!(ascii(field(Tag::Model)), "Test GPU (Metal)");
        let software = ascii(field(Tag::Software));
        assert_eq!(software, super::software());
        if let Some(describe) = tuile_telemetry::build::REF_NAME {
            assert!(software.contains(describe), "{software}");
        }
        assert!(ascii(field(Tag::ImageDescription)).starts_with("45.4603N 27.2904E"));
        assert_eq!(
            field(Tag::PixelXDimension).value.get_uint(0),
            Some(e.size.0)
        );
        assert_eq!(
            field(Tag::PixelYDimension).value.get_uint(0),
            Some(e.size.1)
        );
        let focal = focal_35mm(e.fovy, e.size);
        assert!((rational_of(field(Tag::FocalLength), 0) - focal).abs() < 1e-3);
        assert_eq!(
            field(Tag::FocalLengthIn35mmFilm).value.get_uint(0),
            Some(focal.round() as u32)
        );
        // The rest, in the comment.
        let state = comment(&read);
        assert!((state["eye"]["lat"].as_f64().unwrap() - e.lat).abs() < 1e-12);
        assert!((state["eye"]["lon"].as_f64().unwrap() - e.lon).abs() < 1e-12);
        assert_eq!(state["eye"]["height_ellipsoid_m"], e.height);
        if let Some(ground) = e.ground {
            assert_eq!(state["eye"]["height_above_ground_m"], e.height - ground);
        }
        assert_eq!(state["heading_deg"], e.heading);
        assert_eq!(state["pitch_deg"], e.pitch);
        assert_eq!(state["roll_deg"], e.roll);
        assert_eq!(state["fov_vertical_deg"], e.fovy);
        assert_eq!(state["viewport"]["width"], e.size.0);
        assert_eq!(state["target"]["range_m"], target.range);
        assert!(state["scene"]["solar"].as_str().is_some());
        assert_eq!(state["imagery_compression"], e.compression.as_str());
        assert_eq!(state["lighting"], e.lighting);
        assert_eq!(state["sse_px"], e.sse);
        assert_eq!(state["software"], super::software().as_str());
        if let Some(imagery) = &e.imagery {
            // Non-ASCII, escaped, and back as it was.
            assert_eq!(state["imagery"]["name"], imagery.name.as_str());
        }
        assert!(state.get("budget").is_some());
    }

    /// The red test: a snapshot's tags, read back by an independent reader.
    #[test]
    fn a_jpeg_snapshot_carries_the_view_in_its_exif() {
        let e = exposure();
        let bytes = develop(&frame(e.size), &e, Format::default()).expect("develops");
        assert_eq!(&bytes[..2], &[0xff, 0xd8], "a JPEG");
        assert_tags(&bytes, &e);
        // And the picture is still the picture.
        let back = image::load_from_memory(&bytes)
            .expect("decodes")
            .into_rgb8();
        assert_eq!(back.dimensions(), e.size);
    }

    /// The lossless setting: the same tags, the frame bit for bit.
    #[test]
    fn a_png_snapshot_carries_the_same_exif() {
        let e = exposure();
        let rgba = frame(e.size);
        let bytes = develop(&rgba, &e, Format::Png).expect("develops");
        assert_eq!(&bytes[1..4], b"PNG");
        assert_tags(&bytes, &e);
        let back = image::load_from_memory(&bytes)
            .expect("decodes")
            .into_rgba8();
        assert_eq!(back.into_raw(), rgba);
    }

    /// South and west are refs, not negative rationals; a target under
    /// the sky leaves no destination.
    #[test]
    fn south_and_west_are_said_by_their_refs() {
        let e = Exposure {
            lat: -33.856_789_012,
            lon: -70.654_321_098,
            target: None,
            ..exposure()
        };
        let bytes = develop(&frame(e.size), &e, Format::default()).expect("develops");
        let read = read_back(&bytes);
        let get = |tag| read.get_field(tag, exif::In::PRIMARY);
        assert_eq!(ascii(get(exif::Tag::GPSLatitudeRef).unwrap()), "S");
        assert_eq!(ascii(get(exif::Tag::GPSLongitudeRef).unwrap()), "W");
        assert!((degrees(get(exif::Tag::GPSLatitude).unwrap()) + e.lat).abs() < 1e-6);
        assert!((degrees(get(exif::Tag::GPSLongitude).unwrap()) + e.lon).abs() < 1e-6);
        assert!(get(exif::Tag::GPSDestLatitude).is_none());
        assert_eq!(
            file_name(&e, Format::default()),
            "2026-11-23T19-12-26Z_33.8568S_70.6543W.jpg"
        );
    }

    #[test]
    fn the_file_name_says_when_and_where() {
        let e = exposure();
        assert_eq!(
            file_name(&e, Format::default()),
            "2026-11-23T19-12-26Z_45.4603N_27.2904E.jpg"
        );
        assert_eq!(
            file_name(&e, Format::Png),
            "2026-11-23T19-12-26Z_45.4603N_27.2904E.png"
        );
    }

    /// The seconds' precision: a ten-millionth of a degree survives, in
    /// every corner of the globe.
    #[test]
    fn degrees_round_trip_to_a_ten_millionth() {
        for deg in [0.0, 1e-7, 45.460_312_345_678, 89.999_999_9, 179.999_999_9] {
            let v = dms(deg);
            let back = f64::from(v[0].nominator)
                + f64::from(v[1].nominator) / 60.0
                + f64::from(v[2].nominator) / f64::from(v[2].denominator) / 3600.0;
            assert!((back - deg).abs() < 2e-9, "{deg} came back {back}");
        }
        // An altitude beyond what a thousandth can hold keeps a coarser
        // denominator rather than wrapping.
        let far = rational(36_000_000.0, 1000);
        assert_eq!(
            f64::from(far.nominator) / f64::from(far.denominator),
            36_000_000.0
        );
    }

    /// A 45° vertical field on 3:2 is a 35 mm lens's 43 mm diagonal at
    /// about 29 mm; the wider the picture, the shorter.
    #[test]
    fn the_focal_length_follows_the_field_of_view() {
        let f = focal_35mm(45.0, (1500, 1000));
        // 3:2 is the full frame's own shape: the vertical field alone says
        // it, 12 mm over tan(22.5°).
        assert!((f - 12.0 / 22.5f64.to_radians().tan()).abs() < 1e-9, "{f}");
        assert!(focal_35mm(45.0, (2000, 1000)) < f);
    }

    #[test]
    fn the_layers_are_read_by_name() {
        assert_eq!(Layers::from_names([]), Ok(Layers::default()));
        let both = Layers::from_names(["hud", "shapes"]).expect("reads");
        assert!(both.hud && both.shapes && !both.controls);
        assert_eq!(both.names(), vec!["shapes", "hud"]);
        assert!(Layers::from_names(["menu"])
            .unwrap_err()
            .contains("shapes, hud, controls"));
    }

    /// No credential reaches the file: a configuration with three tokens —
    /// the source's, a template layer's, the collector's — and the layer it
    /// names draped, and none of the three anywhere in the snapshot.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn no_token_is_in_a_snapshot() {
        const SOURCE: &str = "src-0123456789abcdef-not-real";
        const LAYER: &str = "lyr-0123456789abcdef-not-real";
        const COLLECTOR: &str = "col-0123456789abcdef-not-real";
        let text = format!(
            "[ion]\ntoken = \"{SOURCE}\"\n\n\
             [[ion.imagery]]\nkey = \"tiles\"\nname = \"Tiles\"\n\
             template = \"https://tiles.example/{{z}}/{{x}}/{{y}}.webp\"\n\
             attribution = \"© the owner\"\ntoken = \"{LAYER}\"\n\n\
             [telemetry]\nendpoint = \"https://collector.example/otlp\"\ntoken = \"{COLLECTOR}\"\n"
        );
        let file = crate::config_file::parse("a.tuile", &text, "ion").expect("reads");
        let layer = file.settings.imagery.first().expect("a layer");
        let e = Exposure {
            imagery: Some(Imagery::of(layer)),
            ..exposure()
        };
        for format in [Format::default(), Format::Png] {
            let bytes = develop(&frame(e.size), &e, format).expect("develops");
            let state = comment(&read_back(&bytes));
            assert_eq!(state["imagery"]["key"], "tiles", "the layer is said");
            for token in [SOURCE, LAYER, COLLECTOR] {
                assert!(
                    !bytes.windows(token.len()).any(|w| w == token.as_bytes()),
                    "a token is in the {format:?} file"
                );
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_folder_is_pictures_unless_the_settings_say() {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("a home"));
        assert_eq!(
            folder(&SnapshotSettings::default()),
            Some(home.join("Pictures/Tuile"))
        );
        let mine = SnapshotSettings {
            folder: Some("~/Desktop/shots".into()),
            ..SnapshotSettings::default()
        };
        assert_eq!(folder(&mine), Some(home.join("Desktop/shots")));
        let absolute = SnapshotSettings {
            folder: Some("/tmp/x".into()),
            ..SnapshotSettings::default()
        };
        assert_eq!(folder(&absolute), Some(std::path::PathBuf::from("/tmp/x")));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_kept_snapshot_is_whole_and_alone() {
        let dir = tempfile::tempdir().expect("a directory");
        let folder = dir.path().join("Pictures/Tuile");
        let path = keep(&folder, "a.jpg", b"bytes").expect("kept");
        assert_eq!(path, folder.join("a.jpg"));
        assert_eq!(std::fs::read(&path).expect("reads"), b"bytes");
        let names: Vec<_> = std::fs::read_dir(&folder)
            .expect("lists")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("a.jpg")]);
    }
}
