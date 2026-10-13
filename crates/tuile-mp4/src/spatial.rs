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
    Blin, Cams, Cmfy, Co64, Ctts, CttsEntry, Dadj, Decode, Dinf, Dref, Encode, Eyes, FourCC, Ftyp,
    Hdlr, Hero, Hfov, Hvc1, HvcCArray, Hvcc, Lhvc, Mdat, Mdhd, Mdia, Minf, Moov, Mvhd, Stbl, Stco,
    Stri, Stsc, StscEntry, Stsd, Stss, Stsz, StszSamples, Stts, SttsEntry, Tkhd, Trak, Url, Vexu,
    Visual, Vmhd,
};

use crate::Mp4Error;

/// The two records of an MV-HEVC stream, from its parameter sets as an
/// encoder emits them (NAL units without start codes): `hvcC` describes the
/// base layer — the left eye — from its VPS, SPS and PPS, and carries the
/// 3D reference displays SEI if given one; `lhvC` holds the second layer's
/// SPS and PPS. Returned as the records' bodies.
pub fn hevc_records(units: &[&[u8]]) -> Result<(Vec<u8>, Vec<u8>), Mp4Error> {
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
    let body = |atom: &dyn Fn(&mut Vec<u8>) -> mp4_atom::Result<()>| -> Result<Vec<u8>, Mp4Error> {
        let mut out = Vec::new();
        atom(&mut out).map_err(|e| Mp4Error::Atom(e.to_string()))?;
        Ok(out[8..].to_vec())
    };
    Ok((body(&|b| hvcc.encode(b))?, body(&|b| lhvc.encode(b))?))
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
    width: u16,
    height: u16,
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
            width,
            height,
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
    /// shown is shown at zero; none if no frame is reordered.
    fn ctts(&self) -> Option<Ctts> {
        if self.offsets.iter().all(|&o| o == 0) {
            return None;
        }
        let ticks = i64::from(self.ticks);
        let shown = self
            .offsets
            .iter()
            .enumerate()
            .map(|(i, o)| (i as i64 + o) * ticks);
        let first = shown.min().unwrap_or(0);
        let mut entries: Vec<CttsEntry> = Vec::new();
        for (i, o) in self.offsets.iter().enumerate() {
            let offset = (i as i64 + o) * ticks - first - i as i64 * ticks;
            match entries.last_mut() {
                Some(e) if e.sample_offset == offset => e.sample_count += 1,
                _ => entries.push(CttsEntry {
                    sample_count: 1,
                    sample_offset: offset,
                }),
            }
        }
        Some(Ctts { entries })
    }

    fn moov(&self, first_sample: u64) -> Moov {
        let frames = self.sizes.len() as u32;
        let media = u64::from(frames) * u64::from(self.ticks);
        let movie = media * 1000 / u64::from(self.timescale);
        Moov {
            mvhd: Mvhd {
                timescale: 1000,
                duration: movie,
                rate: 1.into(),
                volume: 1.into(),
                next_track_id: 2,
                ..Default::default()
            },
            trak: vec![Trak {
                tkhd: Tkhd {
                    track_id: 1,
                    duration: movie,
                    enabled: true,
                    in_movie: true,
                    width: self.width.into(),
                    height: self.height.into(),
                    ..Default::default()
                },
                mdia: Mdia {
                    mdhd: Mdhd {
                        timescale: self.timescale,
                        duration: media,
                        language: "und".into(),
                        ..Default::default()
                    },
                    hdlr: Hdlr {
                        handler: FourCC::new(b"vide"),
                        name: "VideoHandler".into(),
                    },
                    minf: Minf {
                        vmhd: Some(Vmhd::default()),
                        dinf: Dinf {
                            dref: Dref {
                                urls: vec![Url {
                                    location: "".into(),
                                }],
                            },
                        },
                        stbl: Stbl {
                            stsd: Stsd {
                                codecs: vec![self.entry.clone().into()],
                            },
                            stts: Stts {
                                entries: vec![SttsEntry {
                                    sample_count: frames,
                                    sample_delta: self.ticks,
                                }],
                            },
                            ctts: self.ctts(),
                            stss: Some(Stss {
                                entries: self.sync.clone(),
                            }),
                            stsc: Stsc {
                                entries: vec![StscEntry {
                                    first_chunk: 1,
                                    samples_per_chunk: frames,
                                    sample_description_index: 1,
                                }],
                            },
                            stsz: Stsz {
                                samples: StszSamples::Different {
                                    sizes: self.sizes.clone(),
                                },
                            },
                            stco: None::<Stco>,
                            co64: Some(Co64 {
                                entries: vec![first_sample],
                            }),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    pub fn finish(self) -> Result<Vec<u8>, Mp4Error> {
        let atom = |e: mp4_atom::Error| Mp4Error::Atom(e.to_string());
        let mut out = Vec::new();
        Ftyp {
            major_brand: FourCC::new(b"isom"),
            minor_version: 512,
            compatible_brands: [b"isom", b"iso2", b"hvc1", b"mp41"]
                .map(FourCC::new)
                .to_vec(),
        }
        .encode(&mut out)
        .map_err(atom)?;
        // The samples' offset depends on the moov's size, which does not
        // depend on the offset (a 64-bit entry either way).
        let mut probe = Vec::new();
        self.moov(0).encode(&mut probe).map_err(atom)?;
        let first_sample = (out.len() + probe.len() + 8) as u64;
        self.moov(first_sample).encode(&mut out).map_err(atom)?;
        Mdat { data: self.data }.encode(&mut out).map_err(atom)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
