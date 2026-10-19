// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Finds the black frames in a trace, and says when they happened.
//!
//! A trace is minutes of pictures and nobody scrubs through them looking for a
//! hole that lasts three frames. This reads both channels, measures how much of
//! each picture is bare **where ground was supposed to be**, and prints the
//! worst — with the timestamp, so the frame can be found in a viewer and the
//! camera beside it read off the same timeline.
//!
//! Only the pixels whose ray meets the globe count — see
//! [`tuile_tape::scan`], which this reads every picture through: a black sky
//! is not a hole.
//!
//! ```text
//! cargo run --release -p tuile-tape --bin tuile-scan-black -- trace.mcap
//! ```

/// Sampled rather than scanned: a hole is thousands of pixels, and reading
/// every one of a two-megapixel frame times a thousand frames is minutes for an
/// answer that does not change.
const STEP: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: tuile-scan-black <trace.mcap>")?;
    let bytes = std::fs::read(&path)?;

    // Both channels, joined on the timeline they share. A camera message is
    // written when the camera *changes*, so the one in force for a frame is the
    // last one at or before it.
    let mut cameras: Vec<(u64, tuile_tape::Frame)> = Vec::new();
    let mut worst: Vec<(f64, u64, f64)> = Vec::new();
    let mut frames = 0usize;

    for message in mcap::MessageStream::new(&bytes)? {
        let message = message?;
        let text = std::str::from_utf8(&message.data)?;
        match message.channel.topic.as_str() {
            "/camera" => cameras.push((
                message.log_time,
                tuile_tape::Frame {
                    position: triple(text, "position").ok_or("no position")?,
                    direction: triple(text, "direction").ok_or("no direction")?,
                    up: triple(text, "up").ok_or("no up")?,
                    fovy: number(text, "fovy").ok_or("no fovy")?,
                },
            )),
            "/frame" => {
                frames += 1;
                let Some((_, camera)) = cameras.iter().rev().find(|(t, _)| *t <= message.log_time)
                else {
                    continue;
                };
                let png = base64_decode(field(text, "\"data\":\"").ok_or("no image data")?)?;
                let image = image::load_from_memory(&png)?.to_rgba8();
                let bare = tuile_tape::scan::bare_ground(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    camera,
                    STEP,
                );
                let p = camera.position;
                let altitude = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() - 6_378_137.0;
                worst.push((bare.fraction(), message.log_time, altitude));
            }
            _ => {}
        }
    }

    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("{frames} frames in {path}");
    println!("worst frames, counting only pixels whose ray meets the globe:");
    for (bare, log_time, altitude) in worst.iter().take(12) {
        println!(
            "  {:6.2}% bare   at {:7.3}s   altitude {:9.1} km",
            bare * 100.0,
            *log_time as f64 / 1e9,
            altitude / 1000.0
        );
    }
    Ok(())
}

/// A three-number array field.
fn triple(text: &str, key: &str) -> Option<[f64; 3]> {
    let at = text.find(&format!("\"{key}\":["))? + key.len() + 4;
    let end = text[at..].find(']')? + at;
    let mut out = [0.0; 3];
    for (slot, part) in out.iter_mut().zip(text[at..end].split(',')) {
        *slot = part.trim().parse().ok()?;
    }
    Some(out)
}

/// A scalar field.
fn number(text: &str, key: &str) -> Option<f64> {
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &text[at..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// The value of a JSON string field, without a JSON parser: the document is
/// written by this crate and has five keys.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = text.find(key)? + key.len();
    let end = text[at..].find('"')? + at;
    Some(&text[at..end])
}

fn base64_decode(text: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, c) in ALPHABET.iter().enumerate() {
        table[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = table[byte as usize];
        if value == 255 {
            return Err("not base64".into());
        }
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}
