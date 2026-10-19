// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile store a desktop and a phone keep on disk between runs: opening
//! it, clearing a damaged one, and flushing it on the way out.
//!
//! A browser keeps none yet: its store comes with the graphics budget.

use std::sync::Arc;

use tuile_storage_foyer::SuspendableStore as FoyerStore;

/// Whether a store that would not open is to be cleared and opened afresh.
///
/// Only when the platform allows it — a phone, whose system purges caches and
/// may leave half of one behind — and only when the failure is damage. Never
/// over a passing condition: another process holding the files, the process
/// out of descriptors, a full disk. Clearing then would throw away gigabytes
/// of good cache for nothing, and the next run would start cold.
pub(crate) fn clear(clear_when_damaged: bool, error: &tuile_storage_foyer::StoreError) -> bool {
    clear_when_damaged && !error.is_transient()
}

/// The tile store an application keeps between runs, in `dir`, or `None`
/// with a line saying so.
///
/// One store, several callers: the terrain source and each imagery layer keep
/// their own bytes in it, keyed by tile rather than by URL. The server evicts
/// decoded tiles to stay inside its GPU budget, and this is what makes coming
/// back to them cheap. A store that fails to open is not worth failing the
/// application over: without one the session still runs, and every tile is
/// fetched again.
pub(crate) async fn open_at(
    dir: &std::path::Path,
    memory_bytes: usize,
    disk_bytes: usize,
    clear_when_damaged: bool,
) -> Option<Arc<FoyerStore>> {
    let config = || tuile_storage_foyer::StoreConfig {
        memory_bytes,
        disk_bytes,
        ..tuile_storage_foyer::StoreConfig::at(dir)
    };
    let first = match FoyerStore::open(config()).await {
        Ok(store) => {
            tracing::info!(dir = %dir.display(), "tile store open");
            return Some(Arc::new(store));
        }
        Err(e) => e,
    };
    if !clear(clear_when_damaged, &first) {
        tracing::warn!("no tile store ({first}); every tile will be re-fetched");
        return None;
    }
    tracing::warn!("the tile store is damaged ({first}); starting it afresh");
    let cleared = if dir.is_dir() {
        std::fs::remove_dir_all(dir)
    } else {
        std::fs::remove_file(dir)
    };
    if let Err(e) = cleared {
        tracing::warn!("could not clear {} ({e})", dir.display());
    }
    match FoyerStore::open(config()).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("no tile store ({e}); every tile will be re-fetched");
            None
        }
    }
}

/// Flushes the tile store, and says what it cost if it could not.
///
/// Foyer buffers its disk writes, so **a store that is merely dropped leaves
/// the disk exactly as cold as it found it** — a session killed without this
/// keeps nothing, however long it ran. That is not a hypothetical: several
/// sessions were killed today, and the next one re-fetched everything they had
/// downloaded, at megabytes a second, which reads from the outside as a cache
/// that does not work.
///
/// So it is reported either way, with the numbers that say whether the cache
/// earned its keep this session. `SIGKILL` still takes the buffer with it and
/// nothing can be done about that from inside the process.
///
/// `patience` bounds the wait: a phone on its way out has seconds, and a
/// flush that outlives them is ended by the system with nothing written.
pub(crate) fn close(
    store: Option<std::sync::Arc<FoyerStore>>,
    handle: &tokio::runtime::Handle,
    patience: std::time::Duration,
) {
    let m = tuile_core::metrics::metrics();
    let (hits, misses) = (m.store_hits.get(), m.store_misses.get());
    let served = m.store_bytes_served.get();
    let fetched = m.store_bytes_fetched.get();
    let asked = hits + misses;
    let hit_rate = if asked == 0 {
        0.0
    } else {
        100.0 * hits as f64 / asked as f64
    };
    let Some(store) = store else {
        tracing::warn!("no tile store this session; every tile came from the network");
        return;
    };
    let closed = handle.block_on(async {
        match tokio::time::timeout(patience, store.close()).await {
            Ok(closed) => closed.map_err(|e| e.to_string()),
            Err(_) => Err(format!("not closed within {patience:?}")),
        }
    });
    match closed {
        Ok(()) => tracing::info!(
            "tile store flushed: {hits}/{asked} served from cache ({hit_rate:.0}%), \
             {:.0} MiB served, {:.0} MiB fetched",
            served as f64 / (1024.0 * 1024.0),
            fetched as f64 / (1024.0 * 1024.0),
        ),
        Err(e) => tracing::error!(
            "TILE STORE NOT FLUSHED ({e}): the {:.0} MiB fetched this session are lost \
             and the next run starts cold",
            fetched as f64 / (1024.0 * 1024.0),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store directory the system half-purged, or a crash left unreadable,
    /// is started afresh rather than costing the session its store — here,
    /// the worst case: a plain file where the directory should be.
    #[test]
    fn a_damaged_store_is_started_afresh() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("tiles");
        std::fs::write(&path, b"not a store").expect("a file in the way");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        // Where the platform does not allow it — a desktop — nothing is
        // cleared: the file is still there, and the session runs without.
        assert!(runtime
            .block_on(open_at(&path, 1 << 20, 64 << 20, false))
            .is_none());
        assert!(path.is_file(), "a desktop's store was cleared");
        let store = runtime.block_on(open_at(&path, 1 << 20, 64 << 20, true));
        assert!(store.is_some(), "the store was given up on");
        assert!(path.is_dir());
        if let Some(store) = store {
            runtime.block_on(store.close()).expect("closed");
        }
    }

    /// A passing condition never clears a store, wherever it runs; damage
    /// clears it only where the platform allows.
    #[test]
    fn a_store_is_cleared_only_for_damage_and_only_where_allowed() {
        use std::io::{Error, ErrorKind};
        let busy = tuile_storage_foyer::StoreError::from(Error::from_raw_os_error(24));
        let damaged = tuile_storage_foyer::StoreError::from(Error::from(ErrorKind::InvalidData));
        assert!(!clear(true, &busy));
        assert!(!clear(false, &busy));
        assert!(!clear(false, &damaged));
        assert!(clear(true, &damaged));
    }
}
