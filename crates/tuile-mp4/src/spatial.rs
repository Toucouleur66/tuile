// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A spatial film (MV-HEVC) in an mp4, every box written by `mp4-atom`.
//!
//! The two eyes are two layers of one HEVC stream; its sample entry is an
//! `hvc1` carrying the base layer's record (`hvcC`), the second layer's
//! (`lhvC`), and Apple's stereo boxes — which eyes, the hero eye, the
//! cameras' baseline, the disparity adjustment (`vexu`) and the horizontal
//! field of view (`hfov`) — which a player reads to show it in depth.
//!
//! An encoder hands the records and the samples (length-prefixed access
//! units, both layers in each); nothing is reordered. Layout: `ftyp`, `moov`,
//! then `mdat`, so that a player can start before the end.

use mp4_atom::{
    Blin, Cams, Cmfy, Dadj, Decode, Encode, Eyes, Hero, Hfov, Hvc1, HvcCArray, Hvcc, Lhvc, Stri,
    Vexu, Visual,
};

use crate::Mp4Error;

/// The two records of an MV-HEVC stream, from its parameter sets as an
/// encoder emits them (NAL units without start codes): `hvcC` describes the
/// base layer — the left eye — from its VPS, SPS and PPS, and carries the
/// 3D reference displays SEI if given one; `lhvC` holds the second layer's
/// SPS and PPS. Returned as the records' bodies.
pub fn hevc_records(units: &[&[u8]]) -> Result<(Vec<u8>, Vec<u8>), Mp4Error> {
    let (hvcc, lhvc) = records_of(units, true)?;
    Ok((hvcc, lhvc.unwrap_or_default()))
}

/// The `hvcC` record of a single-layer HEVC stream, from its parameter
/// sets as an encoder emits them (NAL units without start codes): its VPS,
/// SPS and PPS. Returned as the record's body.
pub fn hevc_record(units: &[&[u8]]) -> Result<Vec<u8>, Mp4Error> {
    Ok(records_of(units, false)?.0)
}

fn records_of(units: &[&[u8]], two_layers: bool) -> Result<(Vec<u8>, Option<Vec<u8>>), Mp4Error> {
    use std::io::Read;
    let bad = |why: &str| Mp4Error::Atom(format!("parameter sets: {why}"));
    // NAL header: forbidden(1) type(6) layer id(6) temporal id + 1(3).
    let kind = |u: &[u8]| u.first().map(|b| (b >> 1) & 0x3f);
    let layer = |u: &[u8]| match u {
        [a, b, ..] => ((a & 1) << 5) | (b >> 3),
        _ => 0,
    };
    let array = |nal_unit_type: u8, layer_id: u8| HvcCArray {
        completeness: true,
        nal_unit_type,
        nalus: units
            .iter()
            .filter(|u| kind(u) == Some(nal_unit_type) && layer(u) == layer_id)
            .map(|u| u.to_vec())
            .collect(),
    };
    let sps = units
        .iter()
        .find(|u| kind(u) == Some(33) && layer(u) == 0)
        .ok_or_else(|| bad("no SPS of the base layer"))?;
    let parsed = scuffle_h265::SpsNALUnit::parse(*sps)
        .map_err(|e| bad(&e.to_string()))?
        .rbsp;
    // The general profile, tier and level: twelve bytes of the RBSP after
    // its first, as the record carries them.
    let mut rbsp = Vec::new();
    scuffle_bytes_util::EmulationPreventionIo::new(&sps[2..])
        .read_to_end(&mut rbsp)
        .map_err(|e| bad(&e.to_string()))?;
    let ptl = rbsp.get(1..13).ok_or_else(|| bad("an SPS too short"))?;
    let layers = parsed.sps_max_sub_layers_minus1 + 1;
    let hvcc = Hvcc {
        configuration_version: 1,
        general_profile_space: ptl[0] >> 6,
        general_tier_flag: ptl[0] & 0x20 != 0,
        general_profile_idc: ptl[0] & 0x1f,
        general_profile_compatibility_flags: [ptl[1], ptl[2], ptl[3], ptl[4]],
        general_constraint_indicator_flags: [ptl[5], ptl[6], ptl[7], ptl[8], ptl[9], ptl[10]],
        general_level_idc: ptl[11],
        chroma_format_idc: parsed.chroma_format_idc,
        bit_depth_luma_minus8: parsed.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: parsed.bit_depth_chroma_minus8,
        num_temporal_layers: layers,
        temporal_id_nested: parsed.sps_temporal_id_nesting_flag,
        length_size_minus_one: 3,
        // The parameter sets, and the 3D reference displays SEI (payload
        // 176) where the stream has one: AVFoundation takes a stream for
        // stereo multiview by it, in the record.
        arrays: [array(32, 0), array(33, 0), array(34, 0), array(39, 0)]
            .into_iter()
            .filter(|a| a.nal_unit_type != 39 || !a.nalus.is_empty())
            .collect(),
        ..Hvcc::new()
    };
    let body = |atom: &dyn Fn(&mut Vec<u8>) -> mp4_atom::Result<()>| -> Result<Vec<u8>, Mp4Error> {
        let mut out = Vec::new();
        atom(&mut out).map_err(|e| Mp4Error::Atom(e.to_string()))?;
        Ok(out[8..].to_vec())
    };
    if !two_layers {
        return Ok((body(&|b| hvcc.encode(b))?, None));
    }
    let second: Vec<HvcCArray> = [array(33, 1), array(34, 1)]
        .into_iter()
        .filter(|a| !a.nalus.is_empty())
        .collect();
    if second.is_empty() {
        return Err(bad("no parameter set of the second layer: not MV-HEVC"));
    }
    let lhvc = Lhvc {
        configuration_version: 1,
        num_temporal_layers: layers,
        temporal_id_nested: parsed.sps_temporal_id_nesting_flag,
        length_size_minus_one: 3,
        arrays: second,
        ..Default::default()
    };
    Ok((body(&|b| hvcc.encode(b))?, Some(body(&|b| lhvc.encode(b))?)))
}

/// An access unit in Annex B, split between what goes in the sample entry
/// and what goes in the sample: the parameter sets (VPS, SPS, PPS) and the
/// 3D reference displays SEI (a prefix SEI whose first payload is 176) go
/// to the records ([`hevc_records`]); an `hvc1` sample carries neither.
pub fn entry_units(stream: &[u8]) -> (Vec<&[u8]>, Vec<&[u8]>) {
    crate::nal_units(stream).into_iter().partition(|u| match u {
        [b, ..] if (32..=34).contains(&((b >> 1) & 0x3f)) => true,
        [b, _, 176, ..] => (b >> 1) & 0x3f == 39,
        _ => false,
    })
}

/// What a player is told of the eyes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StereoInfo {
    /// The distance between the eyes, micrometres.
    pub baseline_um: u32,
    /// The horizontal field of view, thousandths of a degree.
    pub hfov_millidegrees: u32,
    /// The horizontal disparity adjustment, ten-thousandths of the width.
    pub disparity_adjustment: i32,
}

/// One spatial film being written.
pub struct SpatialMuxer {
    timescale: u32,
    ticks: u32,
    entry: Hvc1,
    data: Vec<u8>,
    sizes: Vec<u32>,
    sync: Vec<u32>,
    /// Each sample's presentation against its decoding, in frames: all
    /// zero unless the encoder reordered.
    offsets: Vec<i64>,
}

/// A record, as the encoder gives it, read as its box.
fn record<T: Decode>(kind: &[u8; 4], body: &[u8]) -> Result<T, Mp4Error> {
    let mut boxed = Vec::with_capacity(body.len() + 8);
    boxed.extend(((body.len() + 8) as u32).to_be_bytes());
    boxed.extend(kind);
    boxed.extend(body);
    T::decode(&mut boxed.as_slice()).map_err(|e| Mp4Error::Atom(e.to_string()))
}

impl SpatialMuxer {
    /// `hvcc` and `lhvc` are the bodies of the encoder's `hvcC` and `lhvC`
    /// records.
    pub fn new(
        width: u16,
        height: u16,
        fps: u32,
        hvcc: &[u8],
        lhvc: &[u8],
        stereo: StereoInfo,
    ) -> Result<Self, Mp4Error> {
        let ticks = crate::TICKS;
        let entry = Hvc1 {
            visual: Visual {
                data_reference_index: 1,
                width,
                height,
                ..Default::default()
            },
            hvcc: record::<Hvcc>(b"hvcC", hvcc)?,
            lhvc: Some(record::<Lhvc>(b"lhvC", lhvc)?),
            vexu: Some(Vexu {
                eyes: Some(Eyes {
                    stri: Stri {
                        has_left_eye_view: true,
                        has_right_eye_view: true,
                        ..Default::default()
                    },
                    // The left eye is what a flat screen shows.
                    hero: Some(Hero { hero_eye: 1 }),
                    cams: Some(Cams {
                        blin: Some(Blin {
                            baseline_um: stereo.baseline_um,
                        }),
                    }),
                    cmfy: Some(Cmfy {
                        dadj: Some(Dadj {
                            adjustment: stereo.disparity_adjustment,
                        }),
                    }),
                }),
            }),
            hfov: Some(Hfov {
                field_of_view: stereo.hfov_millidegrees,
            }),
            ..Default::default()
        };
        Ok(Self {
            timescale: fps.max(1) * ticks,
            ticks,
            entry,
            data: Vec::new(),
            sizes: Vec::new(),
            sync: Vec::new(),
            offsets: Vec::new(),
        })
    }

    /// The next access unit, both layers in it, in decoding order.
    pub fn push(&mut self, sample: &[u8], key: bool) -> Result<(), Mp4Error> {
        self.push_reordered(sample, key, 0)
    }

    /// The next access unit in decoding order, shown `offset` frames after
    /// it is decoded: an encoder that reorders frames says by how much.
    pub fn push_reordered(
        &mut self,
        sample: &[u8],
        key: bool,
        offset: i64,
    ) -> Result<(), Mp4Error> {
        self.offsets.push(offset);
        if self.sizes.is_empty() && !key {
            return Err(Mp4Error::NoKeyFrame);
        }
        self.data.extend_from_slice(sample);
        self.sizes.push(sample.len() as u32);
        if key {
            self.sync.push(self.sizes.len() as u32);
        }
        Ok(())
    }

    /// The composition offsets, in ticks, shifted so that the first frame
    /// shown is shown at zero; all zero if no frame is reordered.
    fn composition(&self) -> Vec<i64> {
        let ticks = i64::from(self.ticks);
        let first = self
            .offsets
            .iter()
            .enumerate()
            .map(|(i, o)| (i as i64 + o) * ticks)
            .min()
            .unwrap_or(0);
        self.offsets
            .iter()
            .enumerate()
            .map(|(i, o)| (i as i64 + o) * ticks - first - i as i64 * ticks)
            .collect()
    }

    /// The film, laid out for a viewer ([`crate::film`]): its index first,
    /// its colour said.
    pub fn finish(self) -> Result<Vec<u8>, Mp4Error> {
        let composition = self.composition();
        let bytes = self.data.len() as u64;
        let table = crate::film::Table {
            entry: crate::film::dressed(self.entry.into()),
            timescale: self.timescale,
            durations: vec![self.ticks; self.sizes.len()],
            composition,
            sizes: self.sizes,
            sync: self.sync,
            start: 0,
        };
        let mut out = table.head(bytes)?;
        out.extend_from_slice(&self.data);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mp4_atom::{Ftyp, Moov};

    /// The records of a spatial film written by Apple's AVFoundation, as
    /// mp4-atom's own test reads them.
    fn records() -> (Vec<u8>, Vec<u8>) {
        // A minimal hvcC (no arrays) and lhvC: enough to be read back.
        let hvcc = vec![
            1, 1, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 0x99, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0,
            0x0f, 0,
        ];
        let lhvc = vec![1, 0xf0, 0, 0xfc, 0xcb, 0];
        (hvcc, lhvc)
    }

    #[test]
    fn a_spatial_film_holds_its_eyes_records_and_samples_where_it_says() {
        let (hvcc, lhvc) = records();
        let stereo = StereoInfo {
            baseline_um: 10_000_000,
            hfov_millidegrees: 72_489,
            disparity_adjustment: 0,
        };
        let mut m = SpatialMuxer::new(1920, 1080, 30, &hvcc, &lhvc, stereo).expect("muxer");
        assert!(
            m.push(&[0, 0, 0, 1, 0x02], false).is_err(),
            "a film opens on a key frame"
        );
        m.push(&[0, 0, 0, 2, 0x26, 0x01], true).expect("0");
        m.push(&[0, 0, 0, 1, 0x02], false).expect("1");
        let file = m.finish().expect("film");

        let mut buf = file.as_slice();
        let _ftyp = Ftyp::decode(&mut buf).expect("ftyp");
        let moov = Moov::decode(&mut buf).expect("moov");
        let stbl = &moov.trak[0].mdia.minf.stbl;
        let mp4_atom::Codec::Hvc1(entry) = &stbl.stsd.codecs[0] else {
            panic!("an hvc1 entry")
        };
        assert!(entry.lhvc.is_some());
        let eyes = entry
            .vexu
            .as_ref()
            .and_then(|v| v.eyes.as_ref())
            .expect("eyes");
        assert!(eyes.stri.has_left_eye_view && eyes.stri.has_right_eye_view);
        assert_eq!(
            eyes.cams
                .as_ref()
                .and_then(|c| c.blin.as_ref())
                .map(|b| b.baseline_um),
            Some(10_000_000)
        );
        assert_eq!(entry.hfov.as_ref().map(|h| h.field_of_view), Some(72_489));
        // The one chunk offset points at the first sample's first byte.
        let at = stbl.co64.as_ref().expect("co64").entries[0] as usize;
        assert_eq!(&file[at..at + 6], [0, 0, 0, 2, 0x26, 0x01]);
        assert_eq!(stbl.stss.as_ref().expect("stss").entries, [1]);
        assert!(stbl.ctts.is_none(), "nothing reordered, nothing said");
    }

    /// The records AVFoundation wrote, made again from the parameter sets
    /// they hold: the same stream described the same way.
    #[test]
    fn records_are_made_again_from_the_parameter_sets_they_hold() {
        const ENTRY: &[u8] = include_bytes!("../tests/spatial_hvc1.bin");
        let apple = Hvc1::decode(&mut &ENTRY[..]).expect("an hvc1 entry");
        let lhvc = apple.lhvc.clone().expect("lhvC");
        let units: Vec<&[u8]> = apple
            .hvcc
            .arrays
            .iter()
            .chain(&lhvc.arrays)
            .flat_map(|a| a.nalus.iter().map(Vec::as_slice))
            .collect();
        let (hvcc, made_lhvc) = hevc_records(&units).expect("records");
        let hvcc = record::<Hvcc>(b"hvcC", &hvcc).expect("hvcC");
        let made_lhvc = record::<Lhvc>(b"lhvC", &made_lhvc).expect("lhvC");
        let a = &apple.hvcc;
        assert_eq!(
            (
                hvcc.general_profile_space,
                hvcc.general_tier_flag,
                hvcc.general_profile_idc,
                hvcc.general_level_idc
            ),
            (
                a.general_profile_space,
                a.general_tier_flag,
                a.general_profile_idc,
                a.general_level_idc
            )
        );
        assert_eq!(
            hvcc.general_profile_compatibility_flags,
            a.general_profile_compatibility_flags
        );
        assert_eq!(
            hvcc.general_constraint_indicator_flags,
            a.general_constraint_indicator_flags
        );
        assert_eq!(
            (
                hvcc.chroma_format_idc,
                hvcc.bit_depth_luma_minus8,
                hvcc.bit_depth_chroma_minus8
            ),
            (
                a.chroma_format_idc,
                a.bit_depth_luma_minus8,
                a.bit_depth_chroma_minus8
            )
        );
        for t in [32u8, 33, 34, 39] {
            let of = |r: &Hvcc| {
                r.arrays
                    .iter()
                    .filter(|x| x.nal_unit_type == t)
                    .flat_map(|x| x.nalus.clone())
                    .collect::<Vec<_>>()
            };
            assert_eq!(of(&hvcc), of(a), "NAL type {t}");
        }
        let nalus = |l: &Lhvc| {
            l.arrays
                .iter()
                .flat_map(|x| x.nalus.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            nalus(&made_lhvc),
            nalus(&lhvc),
            "the second layer's parameter sets"
        );
        let base: Vec<&[u8]> = apple
            .hvcc
            .arrays
            .iter()
            .flat_map(|a| a.nalus.iter().map(Vec::as_slice))
            .collect();
        assert!(hevc_records(&base).is_err(), "one layer is no MV-HEVC");
    }

    #[test]
    fn parameter_sets_and_the_displays_sei_go_to_the_entry_and_the_rest_to_the_sample() {
        let nal = |kind: u8, layer: u8, body: &[u8]| {
            let mut u = vec![
                0,
                0,
                0,
                1,
                (kind << 1) | (layer >> 5),
                ((layer & 0x1f) << 3) | 1,
            ];
            u.extend_from_slice(body);
            u
        };
        let stream = [
            nal(32, 0, &[1]),
            nal(33, 0, &[2]),
            nal(34, 0, &[3]),
            nal(39, 0, &[176, 4]),
            nal(39, 0, &[5, 4]),
            nal(19, 0, &[6]),
            nal(33, 1, &[7]),
            nal(1, 1, &[8]),
        ]
        .concat();
        let (entry, sample) = entry_units(&stream);
        let kinds = |units: &[&[u8]]| units.iter().map(|u| (u[0] >> 1, u[2])).collect::<Vec<_>>();
        assert_eq!(
            kinds(&entry),
            [(32, 1), (33, 2), (34, 3), (39, 176), (33, 7)]
        );
        assert_eq!(
            kinds(&sample),
            [(39, 5), (19, 6), (1, 8)],
            "another SEI stays with its picture"
        );
    }

    fn segment(path: &std::path::Path, tag: u8, frames: u8, baseline_um: u32, hfov: u32) {
        let (hvcc, lhvc) = records();
        let stereo = StereoInfo {
            baseline_um,
            hfov_millidegrees: hfov,
            disparity_adjustment: 0,
        };
        let mut m = SpatialMuxer::new(64, 64, 30, &hvcc, &lhvc, stereo).expect("muxer");
        for i in 0..frames {
            // I P B, P B, …: offsets as an encoder that reorders says them.
            let offset = if i == 0 || i % 2 == 1 { 1 } else { -1 };
            m.push_reordered(&[0, 0, 0, 2, tag, i], i % 4 == 0, offset)
                .expect("frame");
        }
        std::fs::write(path, m.finish().expect("film")).expect("written");
    }

    /// Segments of one spatial film, joined: every sample once, in order,
    /// its sync and timing kept, the stream described once — and segments
    /// of another stream refused.
    #[test]
    fn spatial_segments_join_into_one_film_streamed_from_their_files() {
        let dir = tempfile::tempdir().expect("dir");
        let (a, b, c) = (
            dir.path().join("a.mp4"),
            dir.path().join("b.mp4"),
            dir.path().join("c.mp4"),
        );
        segment(&a, 0xa, 6, 10_000_000, 72_489);
        // The field of its first frame's camera: a camera's field moves
        // during a film, so each segment says a little another.
        segment(&b, 0xb, 5, 10_000_000, 72_269);
        let out = dir.path().join("film.mp4");
        assert_eq!(
            crate::film::join(&[a.clone(), b.clone()], &out).expect("joined"),
            11
        );

        let mut film = std::fs::File::open(&out).expect("film");
        let track = crate::film::read_track(&mut film)
            .expect("read")
            .expect("a spatial film");
        let bytes = std::fs::read(&out).expect("bytes");
        let payloads: Vec<(u8, u8)> = track
            .samples
            .iter()
            .map(|s| (bytes[s.offset as usize + 4], bytes[s.offset as usize + 5]))
            .collect();
        let expected: Vec<(u8, u8)> = (0..6)
            .map(|i| (0xa, i))
            .chain((0..5).map(|i| (0xb, i)))
            .collect();
        assert_eq!(payloads, expected, "every sample once, in order");
        let keys: Vec<usize> = track
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.key)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            keys,
            [0, 4, 6, 10],
            "each segment's key frames, where they were"
        );
        let mut one = std::fs::File::open(&a).expect("a");
        let alone = crate::film::read_track(&mut one)
            .expect("read")
            .expect("spatial");
        assert_eq!(
            &track.samples[..6]
                .iter()
                .map(|s| s.composition)
                .collect::<Vec<_>>(),
            &alone
                .samples
                .iter()
                .map(|s| s.composition)
                .collect::<Vec<_>>()
        );
        assert!(
            (track.seconds() - 11.0 / 30.0).abs() < 1e-9,
            "{}",
            track.seconds()
        );
        assert_eq!(
            track.entry, alone.entry,
            "the stream described as the first segment does, its field too"
        );

        segment(&c, 0xc, 3, 20_000_000, 72_489);
        assert!(
            crate::film::join(&[a, c], &out).is_err(),
            "another baseline is another stream"
        );
    }

    /// Frames decoded I P B, shown I B P: the first shown at zero, each
    /// one's offset from its decoding as the encoder said.
    #[test]
    fn reordered_frames_are_shown_in_their_order_from_zero() {
        let (hvcc, lhvc) = records();
        let stereo = StereoInfo {
            baseline_um: 1,
            hfov_millidegrees: 1,
            disparity_adjustment: 0,
        };
        let mut m = SpatialMuxer::new(64, 64, 30, &hvcc, &lhvc, stereo).expect("muxer");
        for (key, offset) in [(true, 1), (false, 2), (false, 0)] {
            m.push_reordered(&[0, 0, 0, 1, 0], key, offset)
                .expect("a frame");
        }
        let file = m.finish().expect("film");
        let mut buf = file.as_slice();
        let _ = Ftyp::decode(&mut buf).expect("ftyp");
        let moov = Moov::decode(&mut buf).expect("moov");
        let ctts = moov.trak[0].mdia.minf.stbl.ctts.clone().expect("ctts");
        let offsets: Vec<i64> = ctts
            .entries
            .iter()
            .flat_map(|e| std::iter::repeat_n(e.sample_offset, e.sample_count as usize))
            .collect();
        let shown: Vec<i64> = offsets
            .iter()
            .enumerate()
            .map(|(i, o)| i as i64 * 512 + o)
            .collect();
        assert_eq!(shown, [0, 2 * 512, 512], "I at 0, P at 2, B at 1");
    }
}
