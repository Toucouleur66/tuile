<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-storage-foyer

A native [`ContentStore`]: a hybrid memory + disk cache backed by
[foyer](https://docs.rs/foyer), so content a session evicted comes back from
RAM or from the local disk instead of being fetched and decoded again.

This crate exists **because the core may not**. `tuile-core` states the
[`ContentStore`] contract and nothing more — it compiles to
`wasm32-unknown-unknown`, where foyer (tokio, threads, a filesystem) cannot
follow. A browser host implements the same trait over IndexedDB or the Cache
API, or passes no store at all; nothing above the trait changes.

```no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use std::sync::Arc;
use tuile_core::storage::ContentStore;
use tuile_storage_foyer::FoyerStore;

let store: Arc<dyn ContentStore> = Arc::new(FoyerStore::shared("tiles").await?);
let ttl = Some(std::time::Duration::from_secs(3600));
store.put("terrain/12/34/56", bytes::Bytes::from_static(b"..."), ttl).await;
# Ok(())
# }
```

Must run inside a tokio runtime: foyer is tokio-native.

## Where it sits

Depends on `tuile-core` (the `ContentStore` trait). Used by `tuile-viewer`,
`tuile-film-native`, `tuile-pack-api` and the `orbit-probe` and `warm-cache`
examples.

## Features

None. Native only.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
