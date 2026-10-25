// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The sky-view tables made both ways — in compute, as a film and a viewer
//! with compute make them, and drawn by a render pass, as a viewer on
//! WebGL2 does — are the same tables: the same texel sums, to what two
//! shader stages may round apart.

#![cfg(feature = "wgpu")]

use tuile_atmosphere::gpu::{layout, AtmosphereGpu, AtmosphereUniform, SkyViewPass};
use tuile_atmosphere::hillaire::{Moment, SKY_VIEW_SIZE};
use tuile_atmosphere::{AtmosphereParams, LutAtmosphere, Moon, Moonlight, SkyLook, Sun};
use tuile_core::geo::{geodetic_to_ecef, Geodetic};

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    pollster::block_on(adapter.request_device(&Default::default())).ok()
}

fn read(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<f32> {
    let [w, h] = SKY_VIEW_SIZE.map(|v| v as u32);
    let row = (w * 16).div_ceil(256) * 256;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        texture.size(),
    );
    queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = buffer
        .slice(..)
        .get_mapped_range()
        .expect("mapped")
        .to_vec();
    let mut out = Vec::new();
    for y in 0..h as usize {
        let r = &data[y * row as usize..][..(w * 16) as usize];
        out.extend(r.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
    }
    out
}

#[test]
fn the_compute_and_the_render_pass_make_the_same_sky_view() {
    let Some((device, queue)) = device() else {
        eprintln!("skipping GPU test: no usable adapter");
        return;
    };
    if device.limits().max_compute_workgroups_per_dimension == 0 {
        eprintln!("skipping: no compute on this device");
        return;
    }
    // Twilight over the Pyrenees, the Moon up: both tables have light in
    // them, and the sun's is near its darkest.
    let instant = 1_782_072_000.0;
    let at = Geodetic {
        lon: 0.1418f64.to_radians(),
        lat: 42.9365f64.to_radians(),
        height: 2_877.0,
    };
    let eye = geodetic_to_ecef(at);
    let air = LutAtmosphere::today();
    let moon = Moon::seen_from(instant, at).map(|m| Moonlight::of(&m));
    let light = air.light(
        &air,
        Geodetic { height: 0.0, ..at },
        Moment {
            to_sun_ecef: Sun::at_unix_seconds(instant).direction_ecef,
            moon,
            compensation_ev: 0.0,
        },
        &[],
        SkyLook::default().calibration,
    );
    let uniform = AtmosphereUniform::new(
        &AtmosphereParams::default(),
        &light,
        eye,
        1.0,
        None,
        &SkyLook::default(),
    )
    .seen_from(&light, eye);
    let made = |pass| {
        let bgl = layout(&device, wgpu::ShaderStages::FRAGMENT, true);
        let mut gpu = AtmosphereGpu::new(&device, &bgl, pass);
        gpu.hold_with(&queue, AtmosphereParams::default(), LutAtmosphere::of);
        queue.write_buffer(&gpu.uniform, 0, bytemuck::bytes_of(&uniform));
        gpu.sky_view_stale = true;
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.record_sky_view(&mut encoder);
        queue.submit([encoder.finish()]);
        (
            read(&device, &queue, &gpu.sky_view),
            read(&device, &queue, &gpu.moon_view),
        )
    };
    let computed = made(SkyViewPass::Compute);
    let drawn = made(SkyViewPass::Render);
    for (name, a, b) in [
        ("sun", &computed.0, &drawn.0),
        ("moon", &computed.1, &drawn.1),
    ] {
        let peak = a.iter().fold(0f32, |m, &v| m.max(v.abs()));
        assert!(peak > 0.0, "{name}: an empty table");
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert!(
                (x - y).abs() <= 1e-4 * x.abs().max(y.abs()) + 1e-6 * peak,
                "{name} texel {}: computed {x}, drawn {y}",
                i / 4
            );
        }
    }
}
