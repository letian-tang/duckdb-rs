use std::sync::{Arc, Mutex};

use arrow::array::{Array, StructArray};

use crate::{
    Connection, Error, Params, Result, Row, core::LogicalTypeHandle, executed_result::ExecutedResult, types::ValueRef,
};

/// A materialized result with no borrow of a statement or connection.
///
/// Retains a dedicated or shared connection owner and is Send, not Sync. Moving it between worker
/// threads does not keep a worker alive between reads. This does not bound the
/// memory used by DuckDB to materialize the query.
///
/// ```compile_fail
/// fn require_sync<T: Sync>() {}
/// require_sync::<duckdb::OwnedRows>();
/// ```
///
/// A borrowed row prevents advancing or dropping its result owner:
///
/// ```compile_fail
/// # fn example() -> duckdb::Result<()> {
/// let mut rows = duckdb::Connection::open_in_memory()?.query_owned("SELECT 1", [])?;
/// let row = rows.next()?.unwrap();
/// rows.next()?;
/// row.get_ref(0)?;
/// # Ok(()) }
/// ```
pub struct OwnedRows {
    // Declaration order is intentional: Arrow chunk, result, then connection.
    chunk: Option<StructArray>,
    result: ExecutedResult,
    _connection: Option<Connection>,
    _shared_connection: Option<Arc<Mutex<Connection>>>,
    position: usize,
    exhausted: bool,
}

/// A row borrowed from an owned result, valid until the next mutable access.
pub struct OwnedRow<'a> {
    rows: &'a OwnedRows,
    position: usize,
}

impl Connection {
    /// Execute once, materializing the result and transferring this connection
    /// into its owner. Errors release both statement and connection normally.
    ///
    /// This consumes the connection, including on error. Use a dedicated connection
    /// or [`Connection::try_clone`] if the caller needs to keep querying. A clone
    /// is a separate session; it does not inherit all connection-local state.
    /// SQL preparation/binding semantics are the same as [`Connection::prepare`]
    /// and [`crate::Statement::execute`], including multi-statement SQL handling.
    /// This is not a lazy query: execution finishes before this method returns.
    ///
    /// ```
    /// # fn example() -> duckdb::Result<()> {
    /// let conn = duckdb::Connection::open_in_memory()?;
    /// let mut rows = conn.query_owned("SELECT ?::BIGINT AS id", [42i64])?;
    /// assert_eq!(rows.column_name(0)?, "id");
    /// assert_eq!(rows.next()?.unwrap().get_ref(0)?, duckdb::types::ValueRef::BigInt(42));
    /// assert!(rows.next()?.is_none());
    /// # Ok(()) }
    /// # example().unwrap();
    /// ```
    pub fn query_owned<P: Params>(self, sql: &str, params: P) -> Result<OwnedRows> {
        let result = {
            let mut statement = self.prepare(sql)?;
            statement.execute(params)?;
            statement.stmt.take_result().ok_or(Error::InvalidQuery)?
        };
        Ok(OwnedRows {
            chunk: None,
            result,
            _connection: Some(self),
            _shared_connection: None,
            position: 0,
            exhausted: false,
        })
    }
}

impl OwnedRows {
    /// Materialize a single statement on an existing shared session connection.
    ///
    /// Execution holds the mutex; fetching the completed result does not. The
    /// result retains the connection owner, while the session can execute later
    /// statements or finish its transaction. No new connection is created.
    pub fn query_shared<P: Params>(connection: Arc<Mutex<Connection>>, sql: &str, params: P) -> Result<Self> {
        let result = {
            let guard = connection.lock().map_err(|_| Error::InvalidQuery)?;
            let mut statement = guard.prepare_single(sql)?;
            statement.execute(params)?;
            statement.stmt.take_result().ok_or(Error::InvalidQuery)?
        };
        Ok(Self {
            chunk: None,
            result,
            _connection: None,
            _shared_connection: Some(connection),
            position: 0,
            exhausted: false,
        })
    }
    /// Number of result columns.
    pub fn column_count(&self) -> usize {
        self.result.schema_ref().fields().len()
    }

    /// Column name from executed result metadata.
    pub fn column_name(&self, index: usize) -> Result<&str> {
        self.result
            .column_name(index)
            .map(String::as_str)
            .ok_or(Error::InvalidColumnIndex(index))
    }

    /// Logical type, preserving DuckDB tags not distinguishable from Arrow alone.
    pub fn column_logical_type(&self, index: usize) -> Result<LogicalTypeHandle> {
        if index >= self.column_count() {
            return Err(Error::InvalidColumnIndex(index));
        }
        self.result.try_column_logical_type(index)
    }

    /// Read the next row. Native fetch and Arrow conversion errors are returned.
    /// After an error or exhaustion, subsequent calls return `Ok(None)`.
    /// Metadata and the dedicated connection remain owned until this object is dropped.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<OwnedRow<'_>>> {
        self.next_with(ExecutedResult::step)
    }

    // 可注入读取错误以验证状态收敛，不需要构造无效的原生指针。
    fn next_with(
        &mut self,
        mut fetch: impl FnMut(&ExecutedResult) -> Result<Option<StructArray>>,
    ) -> Result<Option<OwnedRow<'_>>> {
        if self.exhausted {
            return Ok(None);
        }
        while self.chunk.as_ref().is_none_or(|chunk| self.position >= chunk.len()) {
            // Invalidate the previous chunk even when fetching fails.
            self.chunk = None;
            self.position = 0;
            match fetch(&self.result) {
                Ok(Some(chunk)) => self.chunk = Some(chunk),
                Ok(None) => {
                    self.exhausted = true;
                    return Ok(None);
                }
                Err(error) => {
                    self.exhausted = true;
                    return Err(error);
                }
            }
        }
        let position = self.position;
        self.position += 1;
        Ok(Some(OwnedRow { rows: self, position }))
    }
}

impl OwnedRow<'_> {
    /// Borrow a cell by zero-based index, using the ordinary row decoder.
    /// This shares the supported types and decoding limitations of [`Row::get_ref`].
    pub fn get_ref(&self, index: usize) -> Result<ValueRef<'_>> {
        if index >= self.rows.column_count() {
            return Err(Error::InvalidColumnIndex(index));
        }
        let chunk = self.rows.chunk.as_ref().ok_or(Error::InvalidQuery)?;
        Ok(Row::value_ref_with_type(
            self.position,
            chunk.column(index),
            Some(self.rows.result.result_column_logical_id(index)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    #[cfg(feature = "native-frontend")]
    fn shared_result_uses_original_transaction_and_survives_later_queries() -> Result<()> {
        let owner = Arc::new(Mutex::new(Connection::open_in_memory()?));
        let observer = owner.lock().unwrap().try_clone()?;
        owner
            .lock()
            .unwrap()
            .execute_batch("CREATE TABLE shared_tx(v INTEGER); BEGIN; INSERT INTO shared_tx VALUES (42)")?;
        assert!(!owner.lock().unwrap().is_autocommit());
        assert!(owner.lock().unwrap().transaction_state().active);
        let mut rows = OwnedRows::query_shared(Arc::clone(&owner), "SELECT v FROM shared_tx", [])?;
        assert_eq!(
            observer.query_row("SELECT count(*) FROM shared_tx", [], |r| r.get::<_, i64>(0))?,
            0
        );
        owner.lock().unwrap().execute_batch("ROLLBACK; SELECT 99")?;
        assert!(owner.lock().unwrap().is_autocommit());
        let weak = Arc::downgrade(&owner);
        drop(owner);
        assert!(weak.upgrade().is_some());
        assert_eq!(rows.next()?.unwrap().get_ref(0)?, ValueRef::Int(42));
        assert!(rows.next()?.is_none());
        drop(rows);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            observer.query_row("SELECT count(*) FROM shared_tx", [], |r| r.get::<_, i64>(0))?,
            0
        );
        Ok(())
    }

    #[test]
    #[cfg(feature = "native-frontend")]
    fn real_transaction_state_reports_abort_and_rollback() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        assert!(!connection.transaction_state().active);
        connection.execute_batch("BEGIN")?;
        assert!(connection.execute_batch("SELECT CAST('invalid' AS INTEGER)").is_err());
        assert!(connection.transaction_state().aborted);
        connection.execute_batch("ROLLBACK")?;
        assert!(!connection.transaction_state().active);
        Ok(())
    }

    #[test]
    fn executes_once_and_preserves_materialized_snapshot() -> Result<()> {
        let observer = Connection::open_in_memory()?;
        observer.execute_batch(
            "CREATE SEQUENCE owned_seq; CREATE TABLE items(v INTEGER); INSERT INTO items VALUES (10), (20)",
        )?;
        let mut rows = observer
            .try_clone()?
            .query_owned("SELECT nextval('owned_seq'), v FROM items ORDER BY v", [])?;
        observer.execute("UPDATE items SET v = 99", [])?;
        for (id, value) in [(1, 10), (2, 20)] {
            let row = rows.next()?.unwrap();
            assert_eq!(row.get_ref(0)?, ValueRef::BigInt(id));
            assert_eq!(row.get_ref(1)?, ValueRef::Int(value));
        }
        assert!(rows.next()?.is_none());
        let next: i64 = observer.query_row("SELECT nextval('owned_seq')", [], |r| r.get(0))?;
        assert_eq!(next, 3);
        Ok(())
    }

    #[test]
    fn consumes_original_connection_with_session_local_state() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TEMP TABLE owned_local(v INTEGER); INSERT INTO owned_local VALUES (42)")?;
        let mut rows = conn.query_owned("SELECT v FROM owned_local", [])?;
        assert_eq!(rows.column_name(0)?, "v");
        assert_eq!(rows.next()?.unwrap().get_ref(0)?, ValueRef::Int(42));
        assert!(rows.next()?.is_none());
        Ok(())
    }

    #[test]
    fn returning_executes_once_and_releases_statement() -> Result<()> {
        let observer = Connection::open_in_memory()?;
        observer.execute_batch("CREATE TABLE owned_writes(v INTEGER)")?;
        let mut rows = observer
            .try_clone()?
            .query_owned("INSERT INTO owned_writes VALUES (?) RETURNING v", [42])?;
        // The statement has already been dropped; another connection can query
        // the committed side effect while the owned result remains readable.
        let count: i64 = observer.query_row("SELECT count(*) FROM owned_writes", [], |r| r.get(0))?;
        assert_eq!(count, 1);
        assert_eq!(rows.next()?.unwrap().get_ref(0)?, ValueRef::Int(42));
        assert!(rows.next()?.is_none());
        drop(rows);
        let count: i64 = observer.query_row("SELECT count(*) FROM owned_writes", [], |r| r.get(0))?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[test]
    fn drops_connection_on_early_close_exhaustion_and_execution_error() -> Result<()> {
        for sql in ["SELECT i FROM range(4097) t(i)", "SELECT 1 WHERE false"] {
            let conn = Connection::open_in_memory()?;
            let weak = Arc::downgrade(&conn.interrupt_handle());
            let mut rows = conn.query_owned(sql, [])?;
            let _ = rows.next()?;
            assert!(weak.upgrade().is_some());
            drop(rows);
            assert!(weak.upgrade().is_none());
        }
        for sql in ["SELECT missing", "SELECT ?", "SELECT CAST('bad' AS INTEGER)"] {
            let conn = Connection::open_in_memory()?;
            let weak = Arc::downgrade(&conn.interrupt_handle());
            assert!(conn.query_owned(sql, []).is_err());
            assert!(weak.upgrade().is_none());
        }
        Ok(())
    }

    #[test]
    fn dropping_abandoned_worker_result_releases_connection() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        let weak = Arc::downgrade(&conn.interrupt_handle());
        let rows = conn.query_owned("SELECT i FROM range(4097) t(i)", [])?;
        let (send, recv) = std::sync::mpsc::channel();
        drop(recv); // 模拟 async 调用取消：工作已开始，但结果接收者不存在。
        std::thread::spawn(move || {
            let rejected = send.send(rows).unwrap_err();
            drop(rejected);
        })
        .join()
        .unwrap();
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn fetch_error_invalidates_chunk_and_is_terminal() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        let weak = Arc::downgrade(&conn.interrupt_handle());
        let mut rows = conn.query_owned("SELECT i FROM range(4097) t(i)", [])?;
        assert!(rows.next()?.is_some());
        rows.position = rows.chunk.as_ref().unwrap().len();
        assert!(matches!(
            rows.next_with(|_| Err(Error::InvalidQuery)),
            Err(Error::InvalidQuery)
        ));
        assert!(rows.chunk.is_none());
        assert!(rows.next_with(|_| panic!("must not fetch after error"))?.is_none());
        assert_eq!(rows.column_name(0)?, "i");
        drop(rows);
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn wide_numeric_temporal_and_nested_values_match_borrowed_rows() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        let sql = "SELECT true, (-128)::TINYINT, 65535::USMALLINT, 4294967295::UINTEGER,
            170141183460469231731687303715884105727::HUGEINT,
            340282366920938463463374607431768211455::UHUGEINT,
            12345678901234567890123456789012345678::DECIMAL(38,0),
            12.3456789::DECIMAL(18,7), 1.25::FLOAT, -2.5::DOUBLE,
            '\\x00\\xFF'::BLOB, TIMESTAMP_S '2024-01-02 03:04:05',
            TIMESTAMP_MS '2024-01-02 03:04:05.123',
            TIMESTAMP_NS '2024-01-02 03:04:05.123456789',
            [1, NULL, 3], {'name': '中文😀', 'n': 42}";
        let mut owned = conn.try_clone()?.query_owned(sql, [])?;
        let mut statement = conn.prepare(sql)?;
        let mut borrowed = statement.query([])?;
        let count = owned.column_count();
        let a = owned.next()?.unwrap();
        let b = borrowed.next()?.unwrap();
        for index in 0..count {
            assert_eq!(a.get_ref(index)?, b.get_ref(index)?, "column {index}");
        }
        Ok(())
    }

    #[test]
    fn values_and_metadata_match_borrowed_rows() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        let sql = "select (-9223372036854775808)::BIGINT as n, 18446744073709551615::UBIGINT as u, 12.34::DECIMAL(10,2) as d, DATE '1969-12-31' as dt, TIME '12:34:56.123456' as t, TIMESTAMP '1969-12-31 23:59:59.999999' as ts, NULL::INTEGER as nil, '中文😀' as s";
        let mut owned = conn.try_clone()?.query_owned(sql, [])?;
        let mut stmt = conn.prepare(sql)?;
        let mut borrowed = stmt.query([])?;
        let metadata = borrowed.as_ref().unwrap();
        for index in 0..owned.column_count() {
            assert_eq!(owned.column_name(index)?, metadata.column_name(index)?);
            assert_eq!(
                owned.column_logical_type(index)?.id(),
                metadata.column_logical_type(index).id()
            );
        }
        let a = owned.next()?.unwrap();
        let b = borrowed.next()?.unwrap();
        for index in 0..8 {
            assert_eq!(a.get_ref(index)?, b.get_ref(index)?);
        }
        assert!(owned.next()?.is_none());
        Ok(())
    }

    #[test]
    fn untyped_nulls_decode_in_owned_and_borrowed_rows_across_chunks() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        let sql = "SELECT NULL AS nil, NULL::INTEGER AS typed_nil, [NULL, NULL] AS nils FROM range(4097)";
        let mut owned = conn.try_clone()?.query_owned(sql, [])?;
        let mut stmt = conn.prepare(sql)?;
        let mut borrowed = stmt.query([])?;
        for _ in 0..4097 {
            let a = owned.next()?.unwrap();
            let b = borrowed.next()?.unwrap();
            for index in 0..2 {
                assert_eq!(a.get_ref(index)?, ValueRef::Null);
                assert_eq!(b.get_ref(index)?, ValueRef::Null);
            }
            let expected = crate::types::Value::List(vec![crate::types::Value::Null; 2]);
            assert_eq!(a.get_ref(2)?.to_owned(), expected);
            assert_eq!(b.get_ref(2)?.to_owned(), expected);
        }
        assert!(owned.next()?.is_none());
        assert!(borrowed.next()?.is_none());
        Ok(())
    }
    #[test]
    fn independent_results_move_between_threads() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        let mut a = connection
            .try_clone()?
            .query_owned("select i::BIGINT from range(4097) t(i)", [])?;
        let mut b = connection.query_owned("select ?::VARCHAR, NULL::INTEGER", ["中文😀"])?;
        assert_eq!(b.column_count(), 2);
        let row = b.next()?.unwrap();
        assert_eq!(row.get_ref(0)?, ValueRef::Text("中文😀".as_bytes()));
        assert_eq!(row.get_ref(1)?, ValueRef::Null);
        assert!(row.get_ref(2).is_err());
        drop(b);
        a = std::thread::spawn(move || -> Result<OwnedRows> {
            for i in 0..2049 {
                assert_eq!(a.next()?.unwrap().get_ref(0)?, ValueRef::BigInt(i));
            }
            Ok(a)
        })
        .join()
        .unwrap()?;
        for i in 2049..4097 {
            assert_eq!(a.next()?.unwrap().get_ref(0)?, ValueRef::BigInt(i));
        }
        assert!(a.next()?.is_none());
        assert!(a.next()?.is_none());
        Ok(())
    }

    #[test]
    fn errors_and_empty_results() -> Result<()> {
        assert!(Connection::open_in_memory()?.query_owned("select missing", []).is_err());
        assert!(Connection::open_in_memory()?.query_owned("select ?", []).is_err());
        let mut rows = Connection::open_in_memory()?.query_owned("select 1 where false", [])?;
        assert_eq!(rows.column_count(), 1);
        assert!(rows.column_logical_type(1).is_err());
        assert!(rows.next()?.is_none());
        Ok(())
    }
}
