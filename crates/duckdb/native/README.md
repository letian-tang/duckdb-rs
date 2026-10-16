# Native SQL frontend extensions

The optional `native-frontend` feature provides the C ABI and safe Rust
`native_frontend::SqlFrontend` host for statically linked C++ dialect extensions.
It builds against the same bundled DuckDB headers and library as `Connection`.
It contains no MySQL rules, variables, catalog emulation or compatibility UDFs.

An application implements `duckdb_rs::SqlFrontendExtension` and registers an
instance with `RegisterSqlFrontend(name, instance)` in its C++ extension object.
Compile that object with the include paths exported by this crate:

- `DEP_DUCKDB_RUST_FRONTEND_INCLUDE`: the C++ extension contract.
- `DEP_DUCKDB_RUST_FRONTEND_DUCKDB_INCLUDE`: the matching DuckDB headers.

Link the application's extension archive with `+whole-archive` so registration
is retained. This is a static C++ interface tied to the selected DuckDB source;
it does not promise a stable ABI for separately built or dynamically loaded plugins.

Rust selects the named frontend through `SqlFrontend::new`, initializes it on the
root connection, and invokes parse/rewrite/read-only operations through safe
methods. Result payloads are owned UTF-8 strings whose schema belongs to the
extension. Rust policy callbacks can borrow stack state; they are synchronous
and never stored. Callback errors and panics become errors, and C++ exceptions
cannot cross the C ABI. Each allocator frees its own strings.

Extensions must support concurrent calls on distinct connections, with per-call
Parser/AST ownership and immutable shared grammar. The host does not create,
close, pool or retain connections. Engine callbacks and query execution remain
under the application's existing connection and session ownership.

Generic prepared metadata uses `Connection::prepare_single`,
`Statement::parameter_logical_type` and `Statement::try_column_logical_type`.
`prepare_single` rejects multiple statements before executing any of them;
returned logical type handles own their metadata and outlive the statement.

Rust UDFs should use the existing `vscalar`/`vscalar-arrow` APIs rather than
reimplement raw C callbacks in applications. The native frontend host is not
required for those APIs.

With `native-frontend`, the scalar registration helpers also mark Rust UDFs as
fallible through the matched C++ interface. The pinned v1 C API does not expose
this property; without it, DuckDB 2.0 treats an ordinary callback error as an
internal fault. The existing six scalar error/panic recovery tests run when
this bridge is enabled rather than remaining ignored. No kernel patch is needed.

`Connection::transaction_state()` reads actual engine autocommit/active/aborted flags without executing SQL. With this feature, `is_autocommit()` also reads the engine instead of the legacy placeholder. `OwnedRows::query_shared(Arc<Mutex<Connection>>, ...)` executes a single statement on the existing session, retains its owner, and permits later commands while reading the materialized result. It never creates a separate transaction or connection.
