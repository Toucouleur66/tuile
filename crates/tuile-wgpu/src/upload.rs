// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A frame's uploads, batched: one command encoder, one submit, and the
//! bytes copied through a staging belt whose chunks are reused from one
//! frame to the next.
//!
//! Every write the upload path makes — a mesh's buffers, a tile's uniform,
//! a texture's mip chain, a stitch, a rebase — used to be its own
//! `queue.write_buffer` or `write_texture`, or a buffer created with its
//! contents: each one a staging buffer allocated by the device and freed
//! again once the GPU had copied it. A frame of streaming made hundreds of
//! them. Here they are copies recorded into the frame's one encoder, from
//! sub-allocations of a few large staging chunks the belt maps again once
//! the GPU is done with them — no allocation on the hot path once the belt
//! has warmed up, and one submit for the whole frame
//! ([`crate::GpuContext::flush_uploads`]).
//!
//! # Order
//!
//! A write recorded here lands when its encoder is submitted. Every write
//! of the upload path goes through here, so their order among themselves is
//! program order; the pump flushes at the end of its upload step and at the
//! end of resolving a frame, before the host records the draw that reads
//! them.
//!
//! # Where the belt is not used
//!
//! On the browser's WebGPU and on GL the writes stay on the queue: the
//! browser already stages `writeBuffer` and `writeTexture` through its own
//! ring, and GL's mapped buffers are emulated — unmapping a chunk uploads the
//! whole chunk, used or not. The batching of the encoder and its single
//! submit holds there too.

use crate::ops::OpCounters;

/// Bytes of one staging chunk: a frame's uploads fit in a few.
pub(crate) const CHUNK_BYTES: u64 = 4 << 20;

/// What a frame has recorded and not yet submitted.
pub(crate) struct Uploads {
    /// `None` where writes stay on the queue (see the module).
    belt: Option<Belt>,
    cmd: Option<wgpu::CommandEncoder>,
}

impl Uploads {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let belt = match device.adapter_info().backend {
            wgpu::Backend::Metal | wgpu::Backend::Vulkan | wgpu::Backend::Dx12 => Some(Belt::new()),
            _ => None,
        };
        Self { belt, cmd: None }
    }

    /// The frame's encoder, opened on first use.
    pub(crate) fn encoder(
        &mut self,
        device: &wgpu::Device,
        ops: &OpCounters,
    ) -> &mut wgpu::CommandEncoder {
        self.cmd.get_or_insert_with(|| {
            OpCounters::add(&ops.encoders, 1);
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tuile uploads"),
            })
        })
    }

    /// `bytes` into `target` at `offset`. Both a multiple of four, as every
    /// buffer this crate writes is.
    pub(crate) fn write_buffer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ops: &OpCounters,
        target: &wgpu::Buffer,
        offset: u64,
        bytes: &[u8],
    ) {
        let Some(size) = wgpu::BufferSize::new(bytes.len() as u64) else {
            return;
        };
        if self.belt.is_none() || !size.get().is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT) {
            ops.staged(&ops.write_buffer, size.get());
            queue.write_buffer(target, offset, bytes);
            return;
        }
        let cmd = self.cmd.get_or_insert_with(|| {
            OpCounters::add(&ops.encoders, 1);
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tuile uploads"),
            })
        });
        let belt = self.belt.as_mut().expect("a belt");
        let (buffer, at) = belt.allocate(device, ops, size.get(), wgpu::MAP_ALIGNMENT);
        buffer
            .slice(at..at + size.get())
            .get_mapped_range_mut()
            .expect("a mapped belt chunk")
            .copy_from_slice(bytes);
        cmd.copy_buffer_to_buffer(&buffer, at, target, offset, size.get());
        OpCounters::add(&ops.belt_bytes, size.get());
    }

    /// One mip level of `texture`: `rows` rows of `row_bytes` each, packed in
    /// `bytes`, copied to `extent` (in texels, block-rounded for a block
    /// format).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ops: &OpCounters,
        texture: &wgpu::Texture,
        mip_level: u32,
        bytes: &[u8],
        row_bytes: u32,
        rows: u32,
        extent: wgpu::Extent3d,
    ) {
        let copy = wgpu::TexelCopyTextureInfo {
            texture,
            mip_level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        };
        if self.belt.is_none() {
            ops.staged(&ops.write_texture, bytes.len() as u64);
            queue.write_texture(
                copy,
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(rows),
                },
                extent,
            );
            return;
        }
        // A buffer-to-texture copy wants its rows on 256-byte strides.
        let stride = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let Some(size) = wgpu::BufferSize::new(u64::from(stride) * u64::from(rows)) else {
            return;
        };
        let cmd = self.cmd.get_or_insert_with(|| {
            OpCounters::add(&ops.encoders, 1);
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tuile uploads"),
            })
        });
        let belt = self.belt.as_mut().expect("a belt");
        let (buffer, at) = belt.allocate(
            device,
            ops,
            size.get(),
            u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
        );
        {
            let mut view = buffer
                .slice(at..at + size.get())
                .get_mapped_range_mut()
                .expect("a mapped belt chunk");
            if stride == row_bytes {
                view.copy_from_slice(&bytes[..size.get() as usize]);
            } else {
                let (row, stride) = (row_bytes as usize, stride as usize);
                for (r, src) in bytes.chunks_exact(row).take(rows as usize).enumerate() {
                    view.slice(r * stride..r * stride + row)
                        .copy_from_slice(src);
                }
            }
        }
        cmd.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: at,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(rows),
                },
            },
            copy,
            extent,
        );
        OpCounters::add(&ops.belt_bytes, size.get());
    }

    /// Submits what the frame recorded, if anything: one submit. The belt's
    /// chunks are mapped again once the GPU has copied out of them.
    pub(crate) fn flush(&mut self, queue: &wgpu::Queue, ops: &OpCounters) -> bool {
        let Some(cmd) = self.cmd.take() else {
            return false;
        };
        if let Some(belt) = self.belt.as_mut() {
            belt.finish();
        }
        OpCounters::add(&ops.submits, 1);
        queue.submit([cmd.finish()]);
        if let Some(belt) = self.belt.as_mut() {
            belt.recall();
        }
        true
    }
}

/// A ring of staging chunks, mapped for writing, sub-allocated by the
/// frame's writes and mapped again once the GPU has copied out of them.
struct Belt {
    /// Chunks taking this frame's writes, mapped.
    active: Vec<Chunk>,
    /// Chunks submitted, being copied out of.
    closed: Vec<Chunk>,
    /// Chunks mapped again, ready for a frame.
    free: Vec<Chunk>,
    back: std::sync::mpsc::Sender<Chunk>,
    returned: std::sync::mpsc::Receiver<Chunk>,
}

struct Chunk {
    buffer: wgpu::Buffer,
    used: u64,
}

impl Belt {
    fn new() -> Self {
        let (back, returned) = std::sync::mpsc::channel();
        Self {
            active: Vec::new(),
            closed: Vec::new(),
            free: Vec::new(),
            back,
            returned,
        }
    }

    /// `size` bytes at `align`: the chunk, and where in it.
    fn allocate(
        &mut self,
        device: &wgpu::Device,
        ops: &OpCounters,
        size: u64,
        align: u64,
    ) -> (wgpu::Buffer, u64) {
        let fits = |c: &Chunk| c.used.next_multiple_of(align) + size <= c.buffer.size();
        let at = match self.active.iter().position(fits) {
            Some(i) => i,
            None => {
                self.free.extend(self.returned.try_iter());
                let chunk = match self.free.iter().position(fits) {
                    Some(i) => self.free.swap_remove(i),
                    None => {
                        let bytes = CHUNK_BYTES.max(size.next_multiple_of(align));
                        OpCounters::add(&ops.buffers_created, 1);
                        OpCounters::add(&ops.staging_allocs, 1);
                        OpCounters::add(&ops.staging_bytes, bytes);
                        Chunk {
                            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                                label: Some("tuile staging"),
                                size: bytes,
                                usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                                mapped_at_creation: true,
                            }),
                            used: 0,
                        }
                    }
                };
                self.active.push(chunk);
                self.active.len() - 1
            }
        };
        let chunk = &mut self.active[at];
        let offset = chunk.used.next_multiple_of(align);
        chunk.used = offset + size;
        (chunk.buffer.clone(), offset)
    }

    fn finish(&mut self) {
        for chunk in self.active.drain(..) {
            chunk.buffer.unmap();
            self.closed.push(chunk);
        }
    }

    fn recall(&mut self) {
        for mut chunk in self.closed.drain(..) {
            let back = self.back.clone();
            let buffer = chunk.buffer.clone();
            chunk.used = 0;
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Write, move |done| {
                    // A chunk that could not be mapped again is let go of.
                    if done.is_ok() {
                        let _ = back.send(chunk);
                    }
                });
        }
    }
}
