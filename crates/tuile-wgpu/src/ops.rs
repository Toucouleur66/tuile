// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the upload and release path asks of the device, counted.
//!
//! Every call this crate makes to bring content onto the GPU — or to keep
//! what is there up to date — goes through a counter here: buffers and
//! textures created, bind groups made, `write_buffer` and `write_texture`
//! calls, command encoders opened and submitted, the encodes' compute
//! passes, and anything that waits on the GPU or reads back from it. The
//! staging the device allocates behind each of those calls is counted with
//! them: every `write_buffer`, `write_texture` and initialised buffer is one
//! staging allocation inside wgpu, and a staging belt chunk created is one.
//!
//! The counters are the context's ([`crate::GpuContext::upload_ops`]): a
//! host or a bench reads them before and after a frame and subtracts. They
//! count what this crate asks for, not what the driver does with it.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! ops {
    ($( $(#[$doc:meta])* $field:ident ),* $(,)?) => {
        /// A snapshot of the counters: what was asked of the device since the
        /// context was made. Two snapshots subtract into what happened
        /// between them.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct UploadOps {
            $( $(#[$doc])* pub $field: u64, )*
        }

        /// The live counters behind [`UploadOps`].
        #[derive(Default)]
        pub(crate) struct OpCounters {
            $( pub(crate) $field: AtomicU64, )*
        }

        impl OpCounters {
            pub(crate) fn snapshot(&self) -> UploadOps {
                UploadOps {
                    $( $field: self.$field.load(Ordering::Relaxed), )*
                }
            }
        }

        impl std::ops::Sub for UploadOps {
            type Output = UploadOps;
            fn sub(self, earlier: UploadOps) -> UploadOps {
                UploadOps {
                    $( $field: self.$field.saturating_sub(earlier.$field), )*
                }
            }
        }

        impl std::ops::Add for UploadOps {
            type Output = UploadOps;
            fn add(self, other: UploadOps) -> UploadOps {
                UploadOps {
                    $( $field: self.$field + other.$field, )*
                }
            }
        }

        impl UploadOps {
            /// Each counter's name and value, in declaration order: what a
            /// bench prints a table from.
            pub fn named(&self) -> Vec<(&'static str, u64)> {
                vec![ $( (stringify!($field), self.$field), )* ]
            }
        }
    };
}

ops! {
    /// `queue.submit` calls this crate made.
    submits,
    /// Command encoders this crate opened.
    encoders,
    /// `queue.write_buffer` calls.
    write_buffer,
    /// `queue.write_texture` calls.
    write_texture,
    /// Buffers created, initialised or not.
    buffers_created,
    /// Textures created, the encoder's included.
    textures_created,
    /// Bind groups created.
    bind_groups_created,
    /// Staging allocations behind the calls above: one per `write_buffer`,
    /// `write_texture` and initialised buffer inside wgpu, one per staging
    /// belt chunk created.
    staging_allocs,
    /// Bytes those staging allocations held.
    staging_bytes,
    /// Bytes copied to the GPU through a staging belt, reusing its chunks.
    belt_bytes,
    /// Compute passes the imagery encoder recorded.
    encode_passes,
    /// Tiles encoded by them.
    encoded_tiles,
    /// GPU → CPU reads started (`map_async` for reading).
    readbacks,
    /// `device.poll` calls on the upload path.
    polls,
    /// CPU mip chains an upload had to build on the render thread, and
    /// those it found built off it (`crate::chains`).
    chains_inline,
    chains_ready,
}

impl OpCounters {
    pub(crate) fn add(field: &AtomicU64, n: u64) {
        field.fetch_add(n, Ordering::Relaxed);
    }

    /// A `write_buffer` or `write_texture` of `bytes`: the call, and the
    /// staging wgpu allocates for it.
    pub(crate) fn staged(&self, call: &AtomicU64, bytes: u64) {
        Self::add(call, 1);
        Self::add(&self.staging_allocs, 1);
        Self::add(&self.staging_bytes, bytes);
    }

    /// A buffer created with its contents: the buffer, and its staging.
    pub(crate) fn buffer_init(&self, bytes: u64) {
        Self::add(&self.buffers_created, 1);
        Self::add(&self.staging_allocs, 1);
        Self::add(&self.staging_bytes, bytes);
    }
}
