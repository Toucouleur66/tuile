// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A moving film of references through the whole render
//! (`support/moving.rs`), on the machine's adapter: skipped when there is
//! none.

mod support;

use support::moving::{self, Shape};
use tuile_film_native::{render, Ahead, Error, FrameOut, Observer, Order, ReadAhead, Sink, Tone};

fn has_adapter() -> bool {
    pollster::block_on(async {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .is_ok()
    })
}

/// Every picture's digest.
#[derive(Default)]
struct Digests(Vec<u64>);

impl Sink for Digests {
    fn picture(&mut self, _: u32, _: u32, rgba: &[u8], _: &[u8]) -> Result<(), Error> {
        self.0.push(moving::digest(rgba));
        Ok(())
    }
}

fn order() -> Order {
    Order {
        supersample: 1,
        tone: Tone::Off,
        ..Order::default()
    }
}

/// A film of references draws what its store holds, the same pictures
/// every time it is rendered.
#[tokio::test(flavor = "multi_thread")]
async fn a_moving_film_of_references_is_the_same_film_every_time() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    for (name, shape) in [("small", Shape::small()), ("regrowing", Shape::regrowing())] {
        let dir = tempfile::tempdir().expect("dir");
        let written = moving::write(dir.path(), shape).await;
        let mut first = Digests::default();
        let done = render(
            &written.sources,
            &written.film,
            &order(),
            &mut (),
            &mut first,
            &mut (),
        )
        .await
        .expect("render");
        assert_eq!(done.frames, shape.frames);
        // Each tile once, and those whose drape grew once more.
        assert_eq!(
            done.tiles_from_store,
            u64::from((shape.across + shape.frames - 1) * shape.down) + done.drapes_regrown,
            "{name}"
        );
        let mut again = Digests::default();
        render(
            &written.sources,
            &written.film,
            &order(),
            &mut (),
            &mut again,
            &mut (),
        )
        .await
        .expect("render");
        let hex: Vec<String> = first.0.iter().map(|d| format!("{d:#018x}")).collect();
        println!(
            "DIGESTS {name} ({} drapes reduced, {} regrown): {}",
            done.drapes_reduced,
            done.drapes_regrown,
            hex.join(", ")
        );
        assert_eq!(first.0, again.0, "{name}");
    }
}

/// Each frame's tiles entered, and what they took from the store's side.
#[derive(Default)]
struct Took(Vec<(u32, usize, Ahead)>);

impl Observer for Took {
    fn frame(&mut self, frame: &FrameOut<'_>) {
        self.0.push((frame.frame, frame.entered, frame.ahead));
    }
}

/// The plan says which tiles each frame enters: their meshes and their
/// imagery are made from the store off the render's thread, and those of
/// every frame after the first are started before that frame is — none is
/// made on the render's thread when the frame comes to need it.
#[tokio::test(flavor = "multi_thread")]
async fn what_the_next_frames_need_is_made_before_they_need_it() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let shape = Shape::small();
    let written = moving::write(dir.path(), shape).await;
    let mut took = Took::default();
    render(
        &written.sources,
        &written.film,
        &order(),
        &mut (),
        &mut Digests::default(),
        &mut took,
    )
    .await
    .expect("render");
    // A tile's mesh and each imagery tile of its drape: no two tiles of
    // the film share one.
    let per_tile = 1 + (shape.cells * shape.cells) as usize;
    assert_eq!(took.0.len(), shape.frames as usize);
    for (n, (frame, entered, ahead)) in took.0.iter().enumerate() {
        assert_eq!(ahead.here, 0, "frame {frame}: {ahead:?}");
        assert_eq!(
            ahead.ready + ahead.waited,
            entered * per_tile,
            "frame {frame}: {ahead:?}"
        );
        let early = if n == 0 { 0 } else { entered * per_tile };
        assert_eq!(ahead.early, early, "frame {frame}: {ahead:?}");
    }
}

/// The pictures of the regrowing film as the render made them before it
/// read ahead — every tile made on its thread when a frame needed it — on
/// the adapter the suite runs on (lavapipe): what reading ahead must give
/// back, bit for bit.
const BEFORE_READING_AHEAD: &[u64] = &[
    0x198d7266a0318d50,
    0x9da2531ee1d024fb,
    0x267d2633143734b7,
    0xfba715c07ae11254,
    0xacafde0f73cdf225,
    0x2c6e1c567e97ae31,
];

fn adapter_name() -> String {
    pollster::block_on(async {
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map(|a| a.get_info().name)
            .unwrap_or_default()
    })
}

/// Reading ahead changes when a tile is made, never what it is made into:
/// however far ahead — three depths — with room for a lot or for nearly
/// nothing, the pictures are the same — tiles whose drape grows as the
/// camera comes closer made the old way among them — and on the suite's
/// adapter they are those the render made before it read ahead.
#[tokio::test(flavor = "multi_thread")]
async fn reading_ahead_changes_no_picture() {
    if !has_adapter() {
        eprintln!("no GPU adapter: skipped");
        return;
    }
    let dir = tempfile::tempdir().expect("dir");
    let shape = Shape::regrowing();
    let written = moving::write(dir.path(), shape).await;
    let mut films = Vec::new();
    // Three depths, two caps — one that holds a frame's worth and more, one
    // that holds nearly nothing — and one worker or several.
    let settings = [0, ReadAhead::FRAMES, 16].into_iter().flat_map(|frames| {
        [ReadAhead::BYTES, 1]
            .into_iter()
            .map(move |bytes| ReadAhead {
                frames,
                bytes,
                workers: if frames == 0 { 1 } else { ReadAhead::WORKERS },
            })
    });
    for ahead in settings {
        let mut pictures = Digests::default();
        let mut took = Took::default();
        let order = Order { ahead, ..order() };
        let done = render(
            &written.sources,
            &written.film,
            &order,
            &mut (),
            &mut pictures,
            &mut took,
        )
        .await
        .expect("render");
        // Drapes regrown: their tiles entered again, made the old way.
        assert!(done.drapes_regrown > 0, "{ahead:?}: {done:?}");
        let here: usize = took.0.iter().map(|(_, _, a)| a.here).sum();
        assert!(here > 0, "{ahead:?}");
        // What tunes it: room for nearly nothing fills at once, and then
        // holds no more than what one need is made into.
        let full: usize = took.0.iter().map(|(_, _, a)| a.full).sum();
        let most = took.0.iter().map(|(_, _, a)| a.most_waiting).max();
        assert_eq!(done.ahead.full, full, "{ahead:?}");
        assert_eq!(Some(done.ahead.most_waiting), most, "{ahead:?}");
        assert!(done.ahead.most_waiting > 0, "{ahead:?}");
        if ahead.bytes == 1 {
            assert!(full > 0, "{ahead:?}: {:?}", done.ahead);
        }
        films.push((ahead, pictures.0));
    }
    assert_eq!(films.len(), 6);
    let (_, first) = &films[0];
    for (ahead, pictures) in &films {
        assert_eq!(pictures, first, "{ahead:?}");
    }
    if adapter_name().contains("llvmpipe") {
        assert_eq!(first.as_slice(), BEFORE_READING_AHEAD);
    }
}
