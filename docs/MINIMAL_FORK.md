# Minimal Neo fork

Baseline: official duckdb/duckdb-rs `89e4b36` (instance builder rework #909, 2026-10-20).
The pinned DuckDB source is unchanged. The traditional `duckdb` crate and
`libduckdb-sys` match that baseline. Prior V1 additions remain in old commits/tags.

Use upstream APIs for opening instances/connections, parse/bind/prepare, query
execution, column collection append/scan, and scalar callbacks/builders. The fork
must not add parallel result, session, or UDF frameworks over those operations.

Remaining additions:

| Addition | Missing upstream safe interface / reason |
| --- | --- |
| `native-frontend` | Generic C++ PEG/AST extension and actual transaction flags are not exposed by the C API / Neo. Keeps raw handles, callbacks, panic/exception containment and release in the driver. |
| `ColumnDataCollection::new_owned` | Upstream collections borrow their factory. Independently pin the instance factory/database for results retained across session commands and worker tasks. |
| `OwnedColumnDataCollectionScan` | Upstream scan handles are not Send. Only a scan verified to own its instance factory can move between workers; a static borrowed factory is rejected too. |
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
