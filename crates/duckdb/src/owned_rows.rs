use crate::{Error, Result, Row, core::LogicalTypeHandle, executed_result::ExecutedResult, types::ValueRef};
use arrow::array::{Array, StructArray};
use std::sync::{Arc, Mutex};

/// A completed materialized V1 result independent of the original statement and
/// connection. Holds the database owner, not a live query. Send, not Sync.
/// This does not impose a memory bound on DuckDB result materialization.
///
/// ```compile_fail
/// fn require_sync<T: Sync>() {}
/// require_sync::<duckdb::OwnedRows>();
/// ```
pub struct OwnedRows {
    // Drop Arrow chunk and native result before closing their database.
    chunk: Option<StructArray>,
    result: ExecutedResult,
    _database: Arc<Mutex<crate::inner_connection::DatabaseHandle>>,
    position: usize,
    exhausted: bool,
}
/// A row valid until the next mutable access to the result.
pub struct OwnedRow<'a> {
    rows: &'a OwnedRows,
    position: usize,
}
impl OwnedRows {
    pub(crate) fn from_statement(
        statement: &mut crate::Statement<'_>,
        database: Arc<Mutex<crate::inner_connection::DatabaseHandle>>,
    ) -> Result<Self> {
        Ok(Self {
            chunk: None,
            result: statement.stmt.take_result().ok_or(Error::InvalidQuery)?,
            _database: database,
            position: 0,
            exhausted: false,
        })
    }
    /// Whether this result contains an affected-row count rather than query rows.
    pub fn is_changed_rows(&self) -> bool {
        self.result.is_changed_rows()
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
    /// Decode a cell using the ordinary driver's conversion rules.
    pub fn get<T: crate::types::FromSql>(&self, index: usize) -> Result<T> {
        let value = self.get_ref(index)?;
        T::column_result(value).map_err(|error| match error {
            crate::types::FromSqlError::InvalidType => Error::InvalidColumnType(
                index,
                self.rows.column_name(index).unwrap_or("").into(),
                value.data_type(),
            ),
            crate::types::FromSqlError::OutOfRange(value) => Error::IntegralValueOutOfRange(index, value),
            crate::types::FromSqlError::OutOfRangeUnsigned(value) => {
                Error::UnsignedIntegralValueOutOfRange(index, value)
            }
            crate::types::FromSqlError::Other(error) => {
                Error::FromSqlConversionFailure(index, value.data_type(), error)
            }
            #[cfg(feature = "uuid")]
            crate::types::FromSqlError::InvalidUuidSize(_) => Error::InvalidColumnType(
                index,
                self.rows.column_name(index).unwrap_or("").into(),
                value.data_type(),
            ),
        })
    }

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
    use crate::Connection;
    fn query(connection: &Connection, sql: &str) -> Result<OwnedRows> {
        let mut statement = connection.prepare_single(sql)?;
        statement.execute([])?;
        statement.take_owned_result()
    }
    #[test]
    fn detached_result_outlives_session_and_moves_threads() -> Result<()> {
        let rows = query(
            &Connection::open_in_memory()?,
            "SELECT i, 'a' || chr(0) || '中' FROM range(10000) t(i)",
        )?;
        std::thread::spawn(move || -> Result<()> {
            let mut rows = rows;
            for i in 0_i64..10000 {
                let row = rows.next()?.unwrap();
                assert_eq!(row.get::<i64>(0)?, i);
                assert_eq!(row.get::<String>(1)?, "a\0中");
            }
            assert!(rows.next()?.is_none());
            Ok(())
        })
        .join()
        .unwrap()
    }
    #[test]
    fn materialization_uses_original_transaction_and_executes_once() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TEMP TABLE local(i BIGINT); CREATE SEQUENCE counted; BEGIN; INSERT INTO local VALUES (7)",
        )?;
        let mut rows = query(&connection, "SELECT i, nextval('counted') FROM local")?;
        connection.execute_batch("INSERT INTO local VALUES (8); ROLLBACK")?;
        assert_eq!(
            connection.query_row("SELECT count(*) FROM local", [], |row| row.get::<_, i64>(0))?,
            0
        );
        let row = rows.next()?.unwrap();
        assert_eq!(row.get::<i64>(0)?, 7);
        assert_eq!(row.get::<i64>(1)?, 1);
        assert!(rows.next()?.is_none());
        assert_eq!(
            connection.query_row("SELECT nextval('counted')", [], |row| row.get::<_, i64>(0))?,
            2
        );
        Ok(())
    }
    #[test]
    fn detached_values_keep_logical_tags_and_nulls() -> Result<()> {
        let mut rows = query(
            &Connection::open_in_memory()?,
            "SELECT NULL, 340282366920938463463374607431768211455::UHUGEINT, 12::DECIMAL(38,0)",
        )?;
        let row = rows.next()?.unwrap();
        assert_eq!(row.get_ref(0)?, ValueRef::Null);
        assert_eq!(row.get_ref(1)?, ValueRef::UHugeInt(u128::MAX));
        assert!(matches!(row.get_ref(2)?, ValueRef::Decimal(value) if value.to_string() == "12"));
        Ok(())
    }
    #[test]
    fn fetch_error_invalidates_current_row_and_exhausts() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        let mut rows = query(&connection, "SELECT 42")?;
        assert!(rows.next_with(|_| Err(Error::InvalidQuery)).is_err());
        assert!(rows.chunk.is_none());
        assert!(rows.next()?.is_none());
        assert_eq!(connection.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))?, 7);
        Ok(())
    }
}
