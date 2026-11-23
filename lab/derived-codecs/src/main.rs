// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::hint::black_box;
use std::time::Instant;
use image::RgbImage;
// The tables of `tuile-tile-server`'s `derived` module, copied: the bench
// stands alone, outside the workspace.
mod tables;
use tables::*;

/// `tuile_tile_server::derived::downsample`, as it is there (its output is
/// pinned by a digest in that crate's tests).
fn downsample(children: [&[u8]; 4], side: usize) -> Vec<u8> {
    let to_srgb = linear_to_srgb_table();
    let half = side / 2;
    let row = side * 3;
    let mut out = vec![0u8; side * row];
    let mut v = vec![0u32; row];
    for (q, child) in children.iter().enumerate() {
        let (ox, oy) = ((q & 1) * half, (q >> 1) * half);
        for r in 0..half {
            let r0 = &child[2 * r * row..][..row];
            let r1 = &child[(2 * r + 1) * row..][..row];
            for ((s, &a), &b) in v.iter_mut().zip(r0).zip(r1) {
                *s = u32::from(SRGB_TO_LINEAR[usize::from(a)]) + u32::from(SRGB_TO_LINEAR[usize::from(b)]);
            }
            let o = &mut out[((oy + r) * side + ox) * 3..][..half * 3];
            for (op, vp) in o.chunks_exact_mut(3).zip(v.chunks_exact(6)) {
                for ch in 0..3 {
                    op[ch] = to_srgb[((vp[ch] + vp[3 + ch] + 2) >> 2) as usize];
                }
            }
        }
    }
    out
}

fn time<T>(iters: u32, mut f: impl FnMut() -> T) -> f64 {
    // The fastest single call of `iters`: what a call costs uncontended,
    // robust to a loaded machine.
    for _ in 0..3 { black_box(f()); }
    (0..iters).map(|_| {
        let t = Instant::now();
        black_box(f());
        t.elapsed().as_nanos() as f64
    }).fold(f64::INFINITY, f64::min)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a.iter().zip(b).map(|(&x, &y)| { let d = f64::from(x) - f64::from(y); d * d }).sum::<f64>() / a.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

fn encode(rgb: &[u8], side: u32, q: f32, method: i32) -> Vec<u8> {
    let mut c = webp::WebPConfig::new().unwrap();
    c.lossless = 0; c.quality = q; c.thread_level = 0; c.method = method;
    webp::Encoder::from_rgb(rgb, side, side).encode_advanced(&c).unwrap().to_vec()
}

fn main() {
    let dir = std::env::args().nth(1).expect("tiles dir");
    let iters: u32 = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(200);
    let places = ["paris", "alps", "amazon", "sahara", "coast", "farm"];
    let tiles: Vec<[Vec<u8>; 4]> = places.iter().map(|p| std::array::from_fn(|i| std::fs::read(format!("{dir}/{p}-{i}.jpg")).unwrap())).collect();
    let n = tiles.len() as f64;
    let acc = |name: &str, f: &mut dyn FnMut(&[Vec<u8>; 4]) -> f64| {
        let t: f64 = tiles.iter().map(|t| f(t)).sum::<f64>() / n;
        println!("{name:<44} {:>10.1} us/tile", t / 1000.0);
    };
    println!("arch={} sse41_feature={}", std::env::consts::ARCH, cfg!(feature = "sse41"));
    // ── JPEG decode ×4
    acc("decode4 image load_from_memory+to_rgb8", &mut |t| time(iters, || t.iter().map(|b| image::load_from_memory(b).unwrap().to_rgb8()).collect::<Vec<_>>()));
    acc("decode4 image load_from_memory+into_rgb8", &mut |t| time(iters, || t.iter().map(|b| image::load_from_memory(b).unwrap().into_rgb8()).collect::<Vec<_>>()));
    acc("decode4 zune-jpeg direct RGB decode_into", &mut |t| {
        let mut bufs = vec![vec![0u8; 256 * 256 * 3]; 4];
        time(iters, || {
            for (b, o) in t.iter().zip(bufs.iter_mut()) {
                let opts = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
                let mut d = zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(b.as_slice()), opts);
                d.decode_into(o).unwrap();
            }
        })
    });
    acc("decode4 jpeg-decoder", &mut |t| time(iters, || t.iter().map(|b| jpeg_decoder::Decoder::new(b.as_slice()).decode().unwrap()).collect::<Vec<_>>()));
    acc("decode4 libjpeg-turbo (turbojpeg)", &mut |t| {
        let mut dec = turbojpeg::Decompressor::new().unwrap();
        let mut bufs = vec![vec![0u8; 256 * 256 * 3]; 4];
        time(iters, || {
            for (b, o) in t.iter().zip(bufs.iter_mut()) {
                let img = turbojpeg::Image { pixels: o.as_mut_slice(), width: 256, pitch: 256 * 3, height: 256, format: turbojpeg::PixelFormat::RGB };
                dec.decompress(b, img).unwrap();
            }
        })
    });
    // equality of decoders
    for t in &tiles {
        let a = image::load_from_memory(&t[0]).unwrap().to_rgb8().into_raw();
        let mut z = vec![0u8; a.len()];
        let opts = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
        zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(t[0].as_slice()), opts).decode_into(&mut z).unwrap();
        assert_eq!(a, z, "zune direct == image path");
    }
    let pics: Vec<[Vec<u8>; 4]> = tiles.iter().map(|t| std::array::from_fn(|i| image::load_from_memory(&t[i]).unwrap().to_rgb8().into_raw())).collect();
    // ── downsample
    let mut k = 0;
    acc("downsample (row-wise, as in tuile)", &mut |_| { let p = &pics[k % pics.len()]; k += 1; time(iters, || downsample([&p[0], &p[1], &p[2], &p[3]], 256)) });
    let parents: Vec<Vec<u8>> = pics.iter().map(|p| downsample([&p[0], &p[1], &p[2], &p[3]], 256)).collect();
    // ── encode
    for q in [85.0f32, 80.0] {
        for m in 0..=6 {
            let mut size = 0.0; let mut ps = 0.0; let mut ss = 0.0; let mut t = 0.0;
            for p in &parents {
                t += time(iters / 2, || encode(p, 256, q, m));
                let e = encode(p, 256, q, m);
                assert_eq!(e, encode(p, 256, q, m), "deterministic");
                size += e.len() as f64;
                let d = webp::Decoder::new(&e).decode().unwrap().to_vec();
                ps += psnr(p, &d);
                let a = RgbImage::from_raw(256, 256, p.clone()).unwrap();
                let b = RgbImage::from_raw(256, 256, d).unwrap();
                ss += image_compare::rgb_hybrid_compare(&a, &b).unwrap().score;
            }
            println!("encode libwebp q{q} m{m:<30} {:>10.1} us/tile  {:>7.0} B  PSNR {:.2} dB  SSIMh {:.4}", t / n / 1000.0, size / n, ps / n, ss / n);
        }
    }
    // ── WebP decode, of the outputs of each method
    for m in [0, 2, 4, 6] {
        let encs: Vec<Vec<u8>> = parents.iter().map(|p| encode(p, 256, 85.0, m)).collect();
        let mut k = 0;
        acc(&format!("webp decode libwebp (q85 m{m} output)"), &mut |_| { let e = &encs[k % encs.len()]; k += 1; time(iters, || webp::Decoder::new(e).decode().unwrap().to_vec()) });
        let mut k = 0;
        acc(&format!("webp decode image-webp (q85 m{m} output)"), &mut |_| { let e = &encs[k % encs.len()]; k += 1; time(iters, || {
            let mut d = image_webp::WebPDecoder::new(std::io::Cursor::new(e)).unwrap();
            let mut o = vec![0u8; d.output_buffer_size().unwrap()]; d.read_image(&mut o).unwrap(); o }) });
    }
    let encs: Vec<Vec<u8>> = parents.iter().map(|p| encode(p, 256, 85.0, 4)).collect();
    let digest: u64 = encs.iter().flat_map(|e| e.iter()).fold(0xcbf29ce484222325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    println!("fnv of q85 m4 outputs: {digest:016x}");
}
