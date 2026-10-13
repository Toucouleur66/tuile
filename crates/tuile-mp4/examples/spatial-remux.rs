// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A spatial film written again by [`tuile_mp4::spatial::SpatialMuxer`]: its
//! records, its stereo metadata and its samples read from another one, the
//! file made by `mp4-atom` alone. A check of the muxer against what a
//! player accepts.
//!
//! ```text
//! cargo run -p tuile-mp4 --example spatial-remux -- <spatial.mov> <out.mp4>
//! ```

use mp4_atom::{Any, Codec, Decode, Encode, StszSamples};
use tuile_mp4::spatial::{SpatialMuxer, StereoInfo};

type Error = Box<dyn std::error::Error>;

fn body<T: Encode>(atom: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    atom.encode(&mut out)?;
    Ok(out[8..].to_vec())
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [from, to] = args.as_slice() else {
        return Err("usage: spatial-remux <spatial.mov> <out.mp4>".into());
    };
    let file = std::fs::read(from)?;
    let mut buf = file.as_slice();
    let mut moov = None;
    while !buf.is_empty() {
        let atom = Any::decode(&mut buf)?;
        if let Any::Moov(m) = atom {
            moov = Some(m);
        }
    }
    let moov = moov.ok_or("no moov")?;
    let trak = moov.trak.first().ok_or("no track")?;
    let stbl = &trak.mdia.minf.stbl;
    let Some(Codec::Hvc1(entry)) = stbl.stsd.codecs.first() else {
        return Err("not an hvc1 film".into());
    };
    let eyes = entry
        .vexu
        .as_ref()
        .and_then(|v| v.eyes.as_ref())
        .ok_or("not a spatial film")?;
    let stereo = StereoInfo {
        baseline_um: eyes
            .cams
            .as_ref()
            .and_then(|c| c.blin.as_ref())
            .map_or(0, |b| b.baseline_um),
        hfov_millidegrees: entry.hfov.as_ref().map_or(0, |h| h.field_of_view),
        disparity_adjustment: eyes
            .cmfy
            .as_ref()
            .and_then(|c| c.dadj.as_ref())
            .map_or(0, |d| d.adjustment),
    };
    let timescale = trak.mdia.mdhd.timescale;
    let delta = stbl.stts.entries.first().ok_or("no timing")?.sample_delta;
    let fps = (f64::from(timescale) / f64::from(delta)).round() as u32;
    let mut muxer = SpatialMuxer::new(
        entry.visual.width,
        entry.visual.height,
        fps,
        &body(&entry.hvcc)?,
        &body(entry.lhvc.as_ref().ok_or("no lhvC")?)?,
        stereo,
    )?;

    // Where each sample lies: chunks from stco/co64, samples a chunk from
    // stsc, sizes from stsz.
    let chunks: Vec<u64> = match (&stbl.co64, &stbl.stco) {
        (Some(c), _) => c.entries.clone(),
        (None, Some(c)) => c.entries.iter().map(|&o| u64::from(o)).collect(),
        _ => return Err("no chunk offsets".into()),
    };
    let StszSamples::Different { sizes } = &stbl.stsz.samples else {
        return Err("samples of one size".into());
    };
    let sync: std::collections::HashSet<u32> = stbl
        .stss
        .as_ref()
        .map_or_else(Default::default, |s| s.entries.iter().copied().collect());
    // Each sample's composition offset, in frames.
    let offsets: Vec<i64> = stbl.ctts.as_ref().map_or_else(Vec::new, |c| {
        c.entries
            .iter()
            .flat_map(|e| {
                // QuickTime writes signed offsets in a version-0 box.
                let offset = i64::from(e.sample_offset as u32 as i32);
                std::iter::repeat_n(offset / i64::from(delta), e.sample_count as usize)
            })
            .collect()
    });
    let runs = &stbl.stsc.entries;
    let mut sample = 0usize;
    for (c, &offset) in chunks.iter().enumerate() {
        let chunk = c as u32 + 1;
        let per = runs
            .iter()
            .rev()
            .find(|r| r.first_chunk <= chunk)
            .expect("a run")
            .samples_per_chunk;
        let mut at = offset as usize;
        for _ in 0..per {
            let size = *sizes.get(sample).ok_or("more samples than sizes")? as usize;
            let key = stbl.stss.is_none() || sync.contains(&(sample as u32 + 1));
            let offset = offsets.get(sample).copied().unwrap_or(0);
            muxer
                .push_reordered(&file[at..at + size], key, offset)
                .expect("a sample");
            at += size;
            sample += 1;
        }
    }
    std::fs::write(to, muxer.finish()?)?;
    println!(
        "{sample} samples, {fps} fps, {}×{} an eye → {to}",
        entry.visual.width, entry.visual.height
    );
    Ok(())
}
