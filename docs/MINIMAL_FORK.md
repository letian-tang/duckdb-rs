# Minimal Nimbus Neo additions

Baseline: official duckdb-rs v1.20001.0 (e3b2d24), Neo 0.20001.0 (Beta), DuckDB 2.0.1 / 8d78114e90.

Reuse upstream V2 connections, parsing/binding/preparation, values/vectors/collections, scalar callbacks and derived struct readers. Do not carry V1 OwnedRows, PREPARE, Arrow UDF or Linux -bundle patches: upstream #945 covers CMake linking.

Only retained gaps:

- native-frontend: safe extension host for application PEG/AST policy and real transaction state, with exception/panic containment and explicit string ownership.
- independent collection ownership: new_owned/to_owned_scan retain the backing instance/factory and every combined collection owner, without retaining an active query or session; owned scans can move between worker threads.
- direct-appender: append_collection validates logical column types (including DECIMAL scale) and names before constraints/LocalAppend in the caller's transaction. V2's official chunks/values encode input; no physical DECIMAL representation is exposed to the application. Collections with a known different session/instance origin are rejected.

The application reads one TableDescription for its chunk schema; flush never reinterprets buffered data after a target type change. Data and replication checkpoint commits remain atomic. MySQL algorithms and policy remain outside the driver.

The 2.0.1 upgrade reuses upstream DataChunk ownership: owned collection scans return a non-owning DataChunk view whose lifetime is tied to the scan borrow. It does not destroy the scan's internal chunk. Applications preparing SQL for inspection should use upstream parse_inspect to avoid invalidating an existing transaction on syntax errors. No private libduckdb-sys patch is retained.

Upstream alternatives exist, but are not drop-in replacements for the retained paths: DataChunk::copy(&instance) can retain detached in-memory chunks; unlike the current collection, that storage is not managed through the collection's BufferManager. Statement::add_collection can feed INSERT statements; this is a SQL path rather than the application's required direct LocalAppend path. No unmeasured performance advantage is claimed for the custom path.

Validation on macOS arm64: cargo fmt --all --check and the Neo library suite with native-frontend,direct-appender,bundled-json,chrono,rust_decimal passed (427 tests). This does not establish Linux server runtime compatibility.
