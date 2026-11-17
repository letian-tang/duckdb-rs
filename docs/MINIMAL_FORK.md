# Minimal Nimbus Neo additions

Baseline: official duckdb-rs v1.20000.2, Neo 0.20000.2 (Beta), unchanged DuckDB 2.0.0 / d54cbbe3df.

Reuse upstream V2 connections, parsing/binding/preparation, values/vectors/collections, scalar callbacks and derived struct readers. Do not carry V1 OwnedRows, PREPARE, Arrow UDF or Linux -bundle patches: upstream #945 covers CMake linking.

Only retained gaps:

- native-frontend: safe extension host for application PEG/AST policy and real transaction state, with exception/panic containment and explicit string ownership.
- independent collection ownership: new_owned/to_owned_scan retain the backing instance/factory and every combined collection owner, without retaining an active query or session; owned scans can move between worker threads.
- direct-appender: append_collection validates logical column types (including DECIMAL scale) and names before constraints/LocalAppend in the caller's transaction. V2's official chunks/values encode input; no physical DECIMAL representation is exposed to the application. Collections with a known different session/instance origin are rejected.

The application reads one TableDescription for its chunk schema; flush never reinterprets buffered data after a target type change. Data and replication checkpoint commits remain atomic. MySQL algorithms and policy remain outside the driver.
