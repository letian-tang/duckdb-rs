# Minimal Neo fork

Baseline: official duckdb/duckdb-rs tag `v1.20000.0` (`1b8f7cc`, 2026-10-22).
DuckDB follows upstream to `d54cbbe3df17e8e5ce68312f483b4155836112aa` /
`2.0.0`. The traditional `duckdb` crate and
`libduckdb-sys` match that baseline. Prior V1 additions remain in old commits/tags.

Use upstream APIs for opening instances/connections, parse/bind/prepare, query
execution, column collection append/scan, and scalar callbacks/builders. The fork
must not add parallel result, session, or UDF frameworks over those operations.

Upstream now exposes static/loadable V2 extension entrypoints. These do not
expose native PEG/AST integration or actual transaction flags, so they do not
replace `native-frontend`. NimbusDB keeps using upstream scalar builders directly.

Remaining additions:

| Addition | Missing upstream safe interface / reason |
| --- | --- |
| `native-frontend` | Generic C++ PEG/AST extension and actual transaction flags are not exposed by the C API / Neo. Keeps raw handles, callbacks, panic/exception containment and release in the driver. |
| `ColumnDataCollection::new_owned` | Upstream collections borrow their creator. Independently pin the instance factory/database for results retained across session commands and worker tasks. |
| `OwnedColumnDataCollectionScan` | Upstream borrowed scans remain unchanged. `to_owned_scan` consumes independently owned storage; only that scan can move between workers. Borrowed storage is rejected even with a static borrow. |
| `current_chunk` | Borrow the current native scan chunk for row-at-a-time protocol adaptation without copying it. Cache invalidation follows native scan advancement. |
| 128-bit vector alignment fixes | Native storage can be aligned to 8 bytes, while Rust i128/u128 require 16. Return copied values with unaligned access, without creating invalid references. |

Removed redundant `bundled-cmake-json`: upstream `bundled-cmake` plus
`bundled-json` enables the same sys features without a fork alias.

Removed after #907: `DataChunkRef::get_untyped_vector_at`; upstream now accepts
`get_vector_at::<Unknown>()` for dynamic read-only access.

Previously removed: `MaterializedResult`, `query_materialized`, `new_session`, the Arrow
scalar framework, the TIME_TZ convenience decoder, old V1 fork APIs and the
shared V1/V2 frontend host. Query-to-collection orchestration, MySQL FETCH
positions/policy and compatibility algorithms belong to NimbusDB. Algorithms
register with upstream ScalarCallbacks and use safe typed vector reads/writes;
they do not require an Arrow C Data bridge or custom driver callback framework.

The long-term destination is upstream. Remove each remaining patch when the
same safe capability/fix is available there; do not keep aliases to preserve
internal call shapes.

#911 adds profiling/metrics, common types, macro functions, multi-file readers,
CopyTo statistics and chunk capacity. These do not replace the three remaining
patch classes above. DuckDB is a formal 2.0.0 release; Neo still labels its API
experimental. Validation: 291 unit, 24 integration and 50 documentation tests
passed (3 ignored); native-frontend/static extension features enabled.

The release includes #914: use upstream VectorReader/VectorWriter and borrowed
collection scans. Keep the alignment fix in the new typed reader/writer paths.
Neo is separately versioned as 0.20000.0; duckdb/sys/macros are 1.20000.0.
Release validation: 347 unit, 24 integration, 67 documentation tests passed
(3 ignored), plus loadable-extension compilation. The three patch classes remain.
