// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Segments into one film, in Rust, without decoding a picture.
//!
//! Every segment of a render is H.264 from the same encoder with the same
//! settings, so they share one sequence and one picture parameter set. Joining
//! them is then bookkeeping, not video: copy each segment's samples in order
//! into one track, shifting their timestamps by the length of what came
//! before, and keep the sync flags and composition offsets as they are. That is
//! what `ffmpeg -f concat -c copy` did, and it was the only thing ffmpeg was
//! still doing after the encode.
//!
//! Parameter sets that differ are refused rather than papered over: a segment
//! encoded at another size or profile would decode as garbage from its first
//! frame, and the join would not know.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

#[derive(Debug, thiserror::Error)]
pub enum ConcatError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Mp4 {
        path: PathBuf,
        #[source]
        source: mp4::Error,
    },
    #[error("{0}: no H.264, HEVC or AV1 video track")]
    NoVideo(PathBuf),
    #[error("{path}: {what} differs from the first segment's — not the same encode")]
    Mismatch { path: PathBuf, what: &'static str },
    #[error("nothing to join")]
    Empty,
    #[error("{path}: {why}")]
    Film { path: PathBuf, why: String },
}

/// The video track of the film at `path`.
fn track(path: &Path) -> Result<tuile_mp4::film::Track, ConcatError> {
    let mut file = File::open(path).map_err(|source| ConcatError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    tuile_mp4::film::read_track(&mut file)
        .map_err(|e| ConcatError::Film {
            path: path.to_path_buf(),
            why: e.to_string(),
        })?
        .ok_or_else(|| ConcatError::NoVideo(path.to_path_buf()))
}

/// What a film holds, read from its index — no picture decoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub frames: u64,
    /// Seconds, from the video track's own timestamps.
    pub seconds: f64,
}

/// Frames and duration of a film's video track. The track's end is its
/// samples' durations added up; the header's duration can be padded by a
/// muxer, and padding is what this measures.
pub fn probe(path: &Path) -> Result<Probe, ConcatError> {
    let track = track(path)?;
    Ok(Probe {
        frames: track.samples.len() as u64,
        seconds: track.seconds(),
    })
}

/// Frames in a film: the sample count of its video track.
pub fn count_frames(path: &Path) -> Result<u64, ConcatError> {
    Ok(probe(path)?.frames)
}

/// Does the film last what its frames and its cadence announce?
///
/// The join copies samples and imposes no cadence: it inherits the segments'.
/// 7200 frames labelled 24 fps make 300 seconds, and nothing else in the chain
/// notices — which is how a five-minute film came out of a two-minute flight
/// once, `JOB_FPS` having never reached the render. Re-labelling here would
/// hide that upstream fault; this only reports it.
///
/// The tolerance is in FRAMES, not a percentage: containers round their
/// timestamps (25 ms of padding was measured on a remux, a frame and a half),
/// and a percentage is too loose on a long shot and too strict on a short one.
pub fn check_cadence(probe: Probe, fps: f64) -> Result<(), String> {
    let expected = probe.frames as f64 / fps;
    let drift = (probe.seconds - expected).abs() * fps;
    if drift > 2.0 {
        return Err(format!(
            "{:.3} s for {} frames at {fps} fps, {expected:.3} s expected — {drift:.1} frames off",
            probe.seconds, probe.frames
        ));
    }
    Ok(())
}

/// Joins `segments`, in the order given, into `out`, laid out for a viewer
/// ([`tuile_mp4::film::join`]: faststart, its colour said). H.264, HEVC —
/// a spatial film's two layers included — or AV1. Returns the frame count.
pub fn concat(segments: &[PathBuf], out: &Path) -> Result<u64, ConcatError> {
    if segments.is_empty() {
        return Err(ConcatError::Empty);
    }
    // Every segment readable as a film first: an error names the segment.
    for path in segments {
        track(path)?;
    }
    tuile_mp4::film::join(segments, out).map_err(|e| match e {
        tuile_mp4::Mp4Error::Mismatch(what) => ConcatError::Mismatch {
            path: out.to_path_buf(),
            what,
        },
        other => ConcatError::Film {
            path: out.to_path_buf(),
            why: other.to_string(),
        },
    })
}

/// A synthetic H.264 segment: real container, placeholder NAL payloads. Enough
/// for everything here, which never decodes a picture.
#[doc(hidden)]
pub fn write_test_segment(
    path: &Path,
    frames: u32,
    sps: &[u8],
    tag: u8,
) -> Result<(), ConcatError> {
    let io = |source| ConcatError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mp4 = |source| ConcatError::Mp4 {
        path: path.to_path_buf(),
        source,
    };
    let file = File::create(path).map_err(io)?;
    let brand = |s: &str| s.parse().map_err(mp4);
    let config = Mp4Config {
        major_brand: brand("isom")?,
        minor_version: 512,
        compatible_brands: vec![brand("isom")?, brand("avc1")?],
        timescale: 1000,
    };
    let mut w = Mp4Writer::write_start(BufWriter::new(file), &config).map_err(mp4)?;
    w.add_track(&TrackConfig {
        track_type: TrackType::Video,
        timescale: 15360,
        language: "und".into(),
        media_conf: MediaConfig::AvcConfig(AvcConfig {
            width: 160,
            height: 90,
            seq_param_set: sps.to_vec(),
            pic_param_set: vec![0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0],
        }),
    })
    .map_err(mp4)?;
    for i in 0..frames {
        // The payload carries the segment's tag and the frame's index, so a
        // join that reorders or drops samples changes the bytes.
        let bytes = vec![0, 0, 0, 3, 0x65, tag, i as u8].into();
        w.write_sample(
            1,
            &Mp4Sample {
                start_time: u64::from(i) * 256,
                duration: 256,
                rendering_offset: if i % 3 == 1 { 512 } else { 0 },
                is_sync: i % 10 == 0,
                bytes,
            },
        )
        .map_err(mp4)?;
    }
    w.write_end().map_err(mp4)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];

    /// Segments of a spatial film (MV-HEVC) are joined as one, and probed
    /// as one: the render service's segments of a film for a headset.
    #[test]
    fn spatial_segments_are_joined_as_a_spatial_film() {
        use tuile_mp4::spatial::{SpatialMuxer, StereoInfo};
        let dir = tempfile::tempdir().expect("tempdir");
        let hvcc = vec![
            1, 1, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 0x99, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0,
            0x0f, 0,
        ];
        let lhvc = vec![1, 0xf0, 0, 0xfc, 0xcb, 0];
        let stereo = StereoInfo {
            baseline_um: 10_000_000,
            hfov_millidegrees: 72_489,
            disparity_adjustment: 0,
        };
        let mut paths = Vec::new();
        for (tag, frames) in [(1u8, 4u8), (2, 3)] {
            let mut m = SpatialMuxer::new(64, 64, 30, &hvcc, &lhvc, stereo).expect("muxer");
            for i in 0..frames {
                m.push(&[0, 0, 0, 2, tag, i], i == 0).expect("frame");
            }
            let path = dir.path().join(format!("s{tag}.mp4"));
            std::fs::write(&path, m.finish().expect("film")).expect("written");
            paths.push(path);
        }
        let out = dir.path().join("film.mp4");
        assert_eq!(concat(&paths, &out).expect("joined"), 7);
        let p = probe(&out).expect("probe");
        assert_eq!(p.frames, 7);
        assert!(check_cadence(p, 30.0).is_ok());
        assert_eq!(probe(&paths[0]).expect("probe").frames, 4);
    }

    #[test]
    fn samples_keep_their_order_flags_and_offsets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        write_test_segment(&a, 12, SPS, 0xa).expect("a");
        write_test_segment(&b, 7, SPS, 0xb).expect("b");
        let out = dir.path().join("film.mp4");
        assert_eq!(concat(&[a, b], &out).expect("concat"), 19);
        assert_eq!(count_frames(&out).expect("count"), 19);
        // The film lasts its frames: 19 of 256 ticks at 15 360 a second,
        // 316.7 ms — not 19 times a frame cut to a whole millisecond (304).
        let bytes = std::fs::read(&out).expect("read");
        let size = bytes.len() as u64;
        let header =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(bytes), size).expect("header");
        assert_eq!(header.duration().as_millis(), 317);

        let film = std::fs::read(&out).expect("film");
        let size = film.len() as u64;
        let mut reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(film), size).expect("open film");
        let id = *reader.tracks().keys().next().expect("track");
        let mut previous_end = 0;
        for n in 1..=19u32 {
            let s = reader.read_sample(id, n).expect("read").expect("sample");
            let (tag, index) = if n <= 12 { (0xa, n - 1) } else { (0xb, n - 13) };
            assert_eq!(&s.bytes[5..], &[tag, index as u8], "sample {n} payload");
            assert_eq!(s.start_time, previous_end, "sample {n} is not contiguous");
            assert_eq!(s.is_sync, index % 10 == 0, "sample {n} sync flag");
            assert_eq!(
                s.rendering_offset,
                if index % 3 == 1 { 512 } else { 0 },
                "sample {n} cts"
            );
            previous_end = s.start_time + u64::from(s.duration);
        }
    }

    /// The joined film plays from its first bytes and says its colour,
    /// whatever its segments said.
    #[test]
    fn the_joined_film_is_laid_out_for_a_viewer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        write_test_segment(&a, 3, SPS, 1).expect("a");
        let out = dir.path().join("film.mp4");
        concat(&[a], &out).expect("concat");
        let bytes = std::fs::read(&out).expect("film");
        assert_eq!(&bytes[4..8], b"ftyp");
        let ftyp = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(&bytes[ftyp + 4..ftyp + 8], b"moov", "the index before the pictures");
        let track = tuile_mp4::film::read_track(&mut std::io::Cursor::new(&bytes))
            .expect("read")
            .expect("track");
        let mp4_atom_colour = match &track.entry {
            tuile_mp4::film::Entry::Avc1(e) => e.colr.clone(),
            _ => None,
        };
        assert_eq!(mp4_atom_colour, Some(tuile_mp4::film::bt709()));
    }

    #[test]
    fn a_film_at_the_wrong_cadence_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        // 30 samples of 256 ticks at 15360 Hz: exactly 60 fps, 0.5 s.
        write_test_segment(&a, 30, SPS, 1).expect("a");
        let p = probe(&a).expect("probe");
        assert_eq!(p.frames, 30);
        assert!((p.seconds - 0.5).abs() < 1e-9, "{}", p.seconds);
        assert!(check_cadence(p, 60.0).is_ok());
        assert!(check_cadence(p, 24.0).is_err(), "60 fps passed for 24");
    }

    #[test]
    fn another_encode_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        write_test_segment(&a, 3, SPS, 1).expect("a");
        write_test_segment(&b, 3, &[0x67, 0x42, 0x00, 0x1e], 2).expect("b");
        let err = concat(&[a, b], &dir.path().join("f.mp4")).expect_err("must refuse");
        assert!(
            matches!(
                err,
                ConcatError::Mismatch {
                    what: "stream description",
                    ..
                }
            ),
            "{err}"
        );
    }
}
