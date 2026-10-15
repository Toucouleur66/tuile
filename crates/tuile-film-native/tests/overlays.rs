// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A host's overlays through the whole render: a pack made of arithmetic in
//! a store on disk, rendered on the machine's adapter. Skipped when there
//! is none.

use std::sync::Arc;

use glam::DVec3;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::{BakedView, OverlayCamera, OverlayMesh, Overlays, Stereo};
use tuile_film_native::{render, Film, Nothing, Order, Sources, Tone};
use tuile_pack::{BakedTile, PackWriter, TextureFormat};

const FRAMES: u32 = 3;

/// One flat tile 1 km a side, a grey PNG, under a camera 600 m up looking
/// down at it: frame after frame, the same tile.
fn write_pack(dir: &std::path::Path) -> String {
    let at = Geodetic {
        lon: 0.5f64.to_radians(),
        lat: 42.8f64.to_radians(),
        height: 0.0,
    };
    let centre = geodetic_to_ecef(at);
    let enu = enu_frame(at);
    let mut p = Vec::new();
    for (x, y) in [
        (-500.0, -500.0),
        (500.0, -500.0),
        (500.0, 500.0),
        (-500.0, 500.0),
    ] {
        let q = enu.x_axis * x + enu.y_axis * y;
        p.extend([q.x as f32, q.y as f32, q.z as f32]);
    }
    let up = enu.z_axis.as_vec3();
    let n: Vec<f32> = (0..4).flat_map(|_| [up.x, up.y, up.z]).collect();
    let uv = [0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let idx = [0u32, 1, 2, 0, 2, 3];
    let le = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 110, 90, 255]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    let tile = BakedTile {
        id: 1,
        drape: 0,
        origin_ecef: centre.to_array(),
        positions: le(&p),
        normals: le(&n),
        uvs: le(&uv),
        indices: idx.iter().flat_map(|v| v.to_le_bytes()).collect(),
        vertex_count: 4,
        index_count: 6,
        base_color_factor: [1.0; 4],
        texture: Some(png),
        texture_format: TextureFormat::Png,
        refs: None,
    };
    let blob = dir.join("blob");
    let mut writer = PackWriter::new("overlays", centre.to_array(), blob).expect("writer");
    for f in 1..=FRAMES {
        let eye: DVec3 = centre + enu.z_axis * 600.0 + enu.x_axis * f64::from(f);
        let view = BakedView {
            position: eye.to_array(),
            direction: (-enu.z_axis).to_array(),
            up: enu.y_axis.to_array(),
            viewport_px: [64.0, 48.0],
            fovy_rad: 0.8,
        };
        let mut open = writer.begin_frame(f, view);
        if !open.push_known(1, 0) {
            open.push(tile.clone());
        }
        open.end();
    }
    let key = "packs/overlays/1-3.tuilepack";
    let path = dir.join(key);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("packs dir");
    writer.finish_to(&path).expect("finish");
    key.into()
}

/// What a host is asked: each frame, and the camera it is asked with.
#[derive(Default)]
struct Asked(Vec<(u32, OverlayCamera)>);

impl Overlays for Asked {
    fn frame(&mut self, frame: u32, camera: &OverlayCamera, out: &mut Vec<OverlayMesh>) {
        self.0.push((frame, *camera));
        // Something to draw, so that each eye has overlays to draw.
        out.push(OverlayMesh {
            origin_ecef: camera.camera.eye.to_array(),
            positions: vec![[0.0, 0.0, -100.0], [10.0, 0.0, -100.0], [0.0, 10.0, -100.0]],
            colors: vec![[1.0; 4]; 3],
            indices: vec![0, 1, 2],
            ..OverlayMesh::default()
        });
    }
}

fn has_adapter() -> bool {
    pollster::block_on(async {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .is_ok()
    })
}

/// A stereo film asks its host for a frame's overlays once — not once an
/// eye — with the frame's own camera between the eyes, and its pair.
#[tokio::test(flavor = "multi_thread")]
async fn a_stereo_film_asks_for_each_frames_overlays_once_with_the_whole_camera() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let key = write_pack(dir.path());
    std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
    let store: Arc<dyn tuile_repository::Objects> = Arc::new(
        tuile_farm::ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default())
            .expect("store"),
    );
    let sources = Sources::open(store.clone(), store, &dir.path().join("cache"))
        .await
        .expect("sources");
    let film = Film::of(sources.packs.objects.as_ref(), &[key.as_str()])
        .await
        .expect("film");
    for stereo in [None, Some(Stereo::default())] {
        let order = Order {
            supersample: 1,
            tone: Tone::Off,
            stereo,
            ..Order::default()
        };
        let mut asked = Asked::default();
        let done = render(&sources, &film, &order, &mut asked, &mut Nothing, &mut ())
            .await
            .expect("render");
        assert_eq!(done.frames, FRAMES);
        let frames: Vec<u32> = asked.0.iter().map(|(f, _)| *f).collect();
        assert_eq!(frames, (1..=FRAMES).collect::<Vec<_>>(), "{stereo:?}");
        for (frame, camera) in &asked.0 {
            // The frame's own camera: the eye the pack baked, not an eye
            // moved sideways.
            assert_eq!(
                camera.camera.eye.to_array(),
                camera.view.position,
                "{frame}"
            );
            assert_eq!(camera.eyes.is_some(), stereo.is_some(), "{frame}");
            assert!(camera.near() > 0.0);
            if let Some(stereo) = stereo {
                assert_eq!(camera.eyes, Some(stereo.eyes(&camera.camera)));
            }
        }
    }
}
