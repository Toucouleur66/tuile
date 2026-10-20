// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How far a delivered film is from the film it was made from: the PSNR of
//! its luma, frame by frame, against the reference's (the mezzanine), both
//! decoded by the Mac's own decoder. Ten-bit pictures are read on the
//! eight-bit scale (a quarter of their value, kept in floats).
//!
//! ```text
//! cargo run --release -p tuile-film-native --features videotoolbox \
//!   --example film-psnr -- <reference.mp4> <film.mp4>…
//! ```
//!
//! Says, for each film: its size, its mean bitrate, the mean PSNR-Y, the
//! worst frame's and the first percentile's. PSNR is not VMAF and is blind
//! to banding; it compares encoders of the same pictures, not looks.

#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc::sync_channel;

    use tuile_film_native::compress::Stream;
    use tuile_film_native::encode::Input;
    use tuile_film_native::videotoolbox::Decoder;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((reference, films)) = args.split_first() else {
        return Err("usage: film-psnr <reference.mp4> <film.mp4>…".into());
    };

    /// A film's luma, frame after frame, on the eight-bit scale.
    fn lumas(path: String) -> std::sync::mpsc::Receiver<Result<Vec<f32>, String>> {
        let (send, frames) = sync_channel(4);
        std::thread::spawn(move || {
            let run = || -> Result<(), String> {
                let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                let track = tuile_mp4::film::read_track(&mut file)
                    .map_err(|e| e.to_string())?
                    .ok_or("no video track")?;
                let stream = Stream::of(track).map_err(|e| e.to_string())?;
                let (w, h) = stream.size().ok_or("no size")?;
                let decoder = Decoder::of(&stream)?;
                let ten = decoder.out() == Input::P010;
                decoder.decode_all(&stream, &mut file, |frame| {
                    let luma: Vec<f32> = if ten {
                        frame.data[..2 * w * h]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|b| f32::from(u16::from_le_bytes(*b) >> 6) / 4.0)
                            .collect()
                    } else {
                        frame.data[..w * h].iter().map(|&v| f32::from(v)).collect()
                    };
                    send.send(Ok(luma)).map_err(|_| "stopped".to_string())
                })?;
                Ok(())
            };
            if let Err(e) = run() {
                let _ = send.send(Err(e));
            }
        });
        frames
    }

    for film in films {
        let bytes = std::fs::metadata(film)?.len();
        let (truth, seen) = (lumas(reference.clone()), lumas(film.clone()));
        let mut psnr = Vec::new();
        for (a, b) in truth.into_iter().zip(seen) {
            let (a, b) = (a?, b?);
            if a.len() != b.len() {
                return Err(format!("{film}: another size than the reference").into());
            }
            let mse = a
                .iter()
                .zip(&b)
                .map(|(x, y)| f64::from(x - y).powi(2))
                .sum::<f64>()
                / a.len() as f64;
            psnr.push(10.0 * (255.0f64 * 255.0 / mse.max(1e-6)).log10());
        }
        let n = psnr.len();
        let mean = psnr.iter().sum::<f64>() / n as f64;
        let mut sorted = psnr.clone();
        sorted.sort_by(f64::total_cmp);
        let seconds = {
            let mut file = std::fs::File::open(film)?;
            tuile_mp4::film::read_track(&mut file)?
                .ok_or("no track")?
                .seconds()
        };
        println!(
            "{film}: {:.1} MB, {:.2} Mb/s, {n} frames, PSNR-Y mean {mean:.2} dB, P1 {:.2}, worst {:.2}",
            bytes as f64 / 1e6,
            bytes as f64 * 8.0 / seconds / 1e6,
            sorted[n / 100],
            sorted[0]
        );
    }
    Ok(())
}

#[cfg(not(all(target_os = "macos", feature = "videotoolbox")))]
fn main() {
    eprintln!(
        "film-psnr decodes with the Mac's decoder: build it on macOS with --features videotoolbox"
    );
}
