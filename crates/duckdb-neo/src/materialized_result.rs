//! Detached results for protocols that retain cursors between commands.
//!
//! Execution finishes on the original connection before a result is returned.
//! Chunks are copied into DuckDB's buffer-managed column collection, rather
//! than retaining a live query or expanding every row into Rust values.

use crate::{
    Parameters, Result,
    column_data_collection::{ColumnDataCollection, ColumnDataCollectionScan},
    connection::{Connection, IntoStatement},
    data_chunk::DataChunkRef,
    logical_type::LogicalType,
    query_result::{QueryResult, ResultType, StatementType},
};

/// A completed query whose storage owns the database, but does not borrow or
/// reserve its session. Reading it never executes the SQL again.
#[derive(Debug)]
pub struct MaterializedResult {
    scan: Option<ColumnDataCollectionScan<'static>>,
    columns: Vec<(String, LogicalType)>,
    result_type: ResultType,
    statement_type: StatementType,
    changed_rows: usize,
    row_count: usize,
    remaining: usize,
}

// SAFETY: no query or connection factory is retained. The scan exclusively
// owns its states and chunk; its collection pins the database/instance factory.
// Moving between workers is supported; concurrent scan operations are not.
unsafe impl Send for MaterializedResult {}

impl MaterializedResult {
    /// Finish `result`, preserving its remaining rows in independent buffer-managed storage.
    /// Errors destroy both the partial collection and the running query.
    pub fn collect(connection: &Connection, mut result: QueryResult<'_>) -> Result<Self> {
        // Expanding statements can defer their schema/result shape until the
        // first execution step. Keep that first chunk rather than dropping it.
        let first = result.next_chunk()?;
        let result_type = result.result_type()?;
        let statement_type = result.statement_type()?;
        let mut output = Self {
            scan: None,
            columns: Vec::new(),
            result_type,
            statement_type,
            changed_rows: 0,
            row_count: 0,
            remaining: 0,
        };
        if output.result_type != ResultType::QueryResult {
            if output.result_type == ResultType::ChangedRows {
                if let Some(chunk) = first {
                    if chunk.row_count()? > 0 {
                        let count = chunk
                            .get_vector_at::<i64>(0)?
                            .get(0)?
                            .copied()
                            .ok_or_else(|| crate::error::Error::internal("missing changed-row count"))?;
                        output.changed_rows = usize::try_from(count)
                            .map_err(|_| crate::error::Error::internal("invalid changed-row count"))?;
                    }
                }
            }
            output.changed_rows = output
                .changed_rows
                .checked_add(result.drain()?)
                .ok_or_else(|| crate::error::Error::internal("changed-row count overflow"))?;
            return Ok(output);
        }
        // Empty results still expose their declared schema after reaching EOF.
        output.columns = result.schema()?.get_all()?;
        let mut collection = ColumnDataCollection::new_owned(
            connection,
            output.columns.iter().map(|(_, ty)| ty.clone()).collect::<Vec<_>>(),
        )?
        .to_append()?;
        if let Some(mut chunk) = first {
            collection.append(&mut chunk)?;
        }
        while let Some(mut chunk) = result.next_chunk()? {
            collection.append(&mut chunk)?;
        }
        output.row_count = collection.len()?;
        output.remaining = output.row_count;
        output.scan = Some(collection.to_scan()?);
        Ok(output)
    }

    /// Return column names and logical types in declaration order.
    pub fn columns(&self) -> &[(String, LogicalType)] {
        &self.columns
    }
    /// Return the output shape, rather than inferring it from SQL text.
    pub fn result_type(&self) -> ResultType {
        self.result_type.clone()
    }
    /// Return the original statement classification.
    pub fn statement_type(&self) -> StatementType {
        self.statement_type.clone()
    }
    /// Return the number of affected rows for a changed-rows result.
    pub fn changed_rows(&self) -> usize {
        self.changed_rows
    }
    /// Return the total number of materialized result rows.
    pub fn row_count(&self) -> usize {
        self.row_count
    }
    /// Read a borrowed chunk, valid until the next scan call or destruction.
    pub fn next_chunk(&mut self) -> Result<Option<DataChunkRef<'_>>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        match &mut self.scan {
            Some(scan) => {
                let chunk = scan.next_chunk()?;
                if let Some(chunk) = &chunk {
                    self.remaining = self.remaining.saturating_sub(chunk.row_count()?);
                } else {
                    self.remaining = 0;
                }
                Ok(chunk)
            }
            None => Ok(None),
        }
    }
}

impl Connection {
    /// Execute once to completion, returning a result that can outlive this
    /// session and be read while the session executes subsequent commands.
    pub fn query_materialized<'a>(
        &'a self,
        statement: impl IntoStatement<'a>,
        params: Parameters<'_>,
    ) -> Result<MaterializedResult> {
        MaterializedResult::collect(self, self.query(statement, params)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::{Environment, StorageLocation};

    #[test]
    fn detached_cursor_preserves_session_transaction_and_runs_once() -> Result<()> {
        let db = Environment::new()?.open(StorageLocation::InMemory)?;
        let conn = db.connect()?;
        conn.execute("CREATE TEMP TABLE session_data(i INTEGER)", Parameters::None)?;
        conn.execute("BEGIN", Parameters::None)?;
        conn.execute("INSERT INTO session_data VALUES (7)", Parameters::None)?;
        conn.execute("CREATE SEQUENCE cursor_sequence", Parameters::None)?;
        let mut cursor = conn.query_materialized(
            "SELECT i, nextval('cursor_sequence') AS n FROM session_data",
            Parameters::None,
        )?;
        // This would fail with RESOURCE_IN_USE if the original query remained live.
        conn.execute("INSERT INTO session_data VALUES (8)", Parameters::None)?;
        let mut other = conn.query_materialized("SELECT count(*) FROM session_data", Parameters::None)?;
        assert_eq!(other.next_chunk()?.unwrap().get_vector_at::<i64>(0)?.get(0)?, Some(&2));
        conn.execute("ROLLBACK", Parameters::None)?;
        assert_eq!(cursor.row_count(), 1);
        assert_eq!(cursor.columns()[1].0, "n");
        let chunk = cursor.next_chunk()?.unwrap();
        assert_eq!(chunk.get_vector_at::<i32>(0)?.get(0)?, Some(&7));
        assert_eq!(chunk.get_vector_at::<i64>(1)?.get(0)?, Some(&1));
        assert!(cursor.next_chunk()?.is_none());
        Ok(())
    }

    #[test]
    fn detached_cursor_outlives_every_session_and_moves_to_another_worker() -> Result<()> {
        let cursor = {
            let env = Environment::new()?;
            let db = env.open(StorageLocation::InMemory)?;
            let conn = db.connect()?;
            conn.query_materialized(
                "SELECT i, 'a' || chr(0) || '中' AS text FROM range(10000) t(i)",
                Parameters::None,
            )?
        };
        std::thread::spawn(move || -> Result<()> {
            let mut cursor = cursor;
            let mut count = 0i64;
            while let Some(chunk) = cursor.next_chunk()? {
                let values = chunk.get_vector_at::<i64>(0)?;
                let strings = chunk.get_vector_at::<String>(1)?;
                for index in 0..chunk.row_count()? {
                    assert_eq!(values.get(index)?, Some(&count));
                    assert_eq!(strings.get(index)?, Some("a\0中"));
                    count += 1;
                }
            }
            assert_eq!(count, 10000);
            Ok(())
        })
        .join()
        .unwrap()
    }

    #[test]
    fn empty_changed_rows_and_error_release_connection() -> Result<()> {
        let conn = Environment::new()?.open(StorageLocation::InMemory)?.connect()?;
        let mut empty =
            conn.query_materialized("SELECT NULL::DECIMAL(38,8) AS amount WHERE false", Parameters::None)?;
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.columns()[0].0, "amount");
        assert!(empty.next_chunk()?.is_none());
        conn.execute("CREATE TABLE modified(i INTEGER)", Parameters::None)?;
        let changed = conn.query_materialized("INSERT INTO modified SELECT * FROM range(3)", Parameters::None)?;
        assert_eq!(changed.changed_rows(), 3);
        assert_eq!(changed.result_type(), ResultType::ChangedRows);
        assert!(
            conn.query_materialized(
                "SELECT CASE WHEN i < 8000 THEN i ELSE CAST(i::VARCHAR || 'x' AS INTEGER) END FROM range(10000) t(i)",
                Parameters::None
            )
            .is_err()
        );
        conn.execute("SELECT 42", Parameters::None)?;
        Ok(())
    }

    #[test]
    fn prepared_metadata_has_no_side_effects_and_parameters_keep_types() -> Result<()> {
        use crate::types::DecimalSignature;
        let conn = Environment::new()?.open(StorageLocation::InMemory)?.connect()?;
        conn.execute("CREATE SEQUENCE prepare_sequence", Parameters::None)?;
        let statement = conn
            .parse("SELECT nextval('prepare_sequence') AS n, $1::VARCHAR AS text, $2::DECIMAL(38,8) AS amount")?
            .next()
            .unwrap()?;
        let metadata = statement.bind(&conn)?;
        assert_eq!(metadata.parameters.len()?, 2);
        assert_eq!(metadata.schema.get(0)?.0, "n");
        assert_eq!(
            DecimalSignature::from_logical_type(&metadata.schema.get(2)?.1)?.scale,
            8
        );
        let prepared = statement.prepare(&conn, false)?;
        let mut first = MaterializedResult::collect(
            &conn,
            prepared.execute(Parameters::positional(&[
                &"中\0文",
                &"123456789012345678901234567890.12345678",
            ]))?,
        )?;
        assert_eq!(first.row_count(), 1);
        let chunk = first.next_chunk()?.unwrap();
        assert_eq!(chunk.get_vector_at::<i64>(0)?.get(0)?, Some(&1));
        assert_eq!(chunk.get_vector_at::<String>(1)?.get(0)?, Some("中\0文"));
        let amount = chunk.get_untyped_vector_at(2)?.get_value_slow(0)?;
        assert_eq!(amount.dbg_string()?, "123456789012345678901234567890.12345678");
        let mut second = MaterializedResult::collect(
            &conn,
            prepared.execute(Parameters::positional(&[&None::<String>, &None::<String>]))?,
        )?;
        let chunk = second.next_chunk()?.unwrap();
        assert_eq!(chunk.get_vector_at::<i64>(0)?.get(0)?, Some(&2));
        assert_eq!(chunk.get_vector_at::<String>(1)?.get(0)?, None);
        assert!(chunk.get_untyped_vector_at(2)?.get_value_slow(0)?.is_null()?);
        Ok(())
    }

    #[test]
    fn combined_owned_collections_keep_all_instances_alive() -> Result<()> {
        use crate::DuckDBType;
        fn collection(value: i64) -> Result<ColumnDataCollection<'static>> {
            let conn = Environment::new()?.open(StorageLocation::InMemory)?.connect()?;
            let ty = i64::logical_type(&conn)?;
            let mut collection = ColumnDataCollection::new_owned(&conn, [ty])?.to_append()?;
            let mut chunk = conn
                .query("SELECT $1::BIGINT", Parameters::positional(&[&value]))?
                .next_chunk()?
                .unwrap();
            collection.append(&mut chunk)?;
            Ok(collection.to_normal())
        }
        let mut combined = collection(7)?.to_append()?;
        combined.combine(collection(8)?)?;
        let mut scan = combined.to_scan()?;
        let mut values = Vec::new();
        while let Some(chunk) = scan.next_chunk()? {
            values.extend(chunk.get_vector_at::<i64>(0)?.iter()?.map(|value| *value.unwrap()));
        }
        assert_eq!(values, [7, 8]);
        Ok(())
    }

    #[test]
    fn collection_bulk_insert_and_checkpoint_share_one_transaction() -> Result<()> {
        use crate::{DuckDBType, data_chunk::DataChunk};
        let conn = Environment::new()?.open(StorageLocation::InMemory)?.connect()?;
        conn.execute("CREATE TABLE bulk_target(i BIGINT)", Parameters::None)?;
        conn.execute("CREATE TABLE checkpoint(position BIGINT)", Parameters::None)?;
        conn.execute("INSERT INTO checkpoint VALUES (0)", Parameters::None)?;
        let observer = conn.new_session()?;
        for commit in [false, true] {
            let ty = i64::logical_type(&conn)?;
            let mut chunk = DataChunk::create(&conn, [ty.clone()], true)?;
            let mut vector = chunk.get_vector_at_mut::<i64>(0)?;
            vector.set_size(2)?;
            vector.write(0, Some(7))?;
            vector.write(1, None)?;
            let mut collection = ColumnDataCollection::new(&conn, [ty])?.to_append()?;
            collection.append(&mut chunk)?;
            let statement = conn
                .parse("INSERT INTO bulk_target SELECT * FROM input_rows")?
                .next()
                .unwrap()?
                .add_collection(collection.to_normal(), "input_rows")?;
            conn.execute("BEGIN", Parameters::None)?;
            assert_eq!(conn.execute(statement, Parameters::None)?, 2);
            conn.execute("UPDATE checkpoint SET position = $1", Parameters::positional(&[&7_i64]))?;
            if commit {
                conn.execute("COMMIT", Parameters::None)?;
            } else {
                assert!(
                    conn.execute("SELECT CAST('invalid' AS INTEGER)", Parameters::None)
                        .is_err()
                );
                conn.execute("ROLLBACK", Parameters::None)?;
            }
            let mut result = observer.query_materialized(
                "SELECT (SELECT count(*) FROM bulk_target), (SELECT position FROM checkpoint)",
                Parameters::None,
            )?;
            let chunk = result.next_chunk()?.unwrap();
            assert_eq!(
                chunk.get_vector_at::<i64>(0)?.get(0)?,
                Some(if commit { &2 } else { &0 })
            );
            assert_eq!(
                chunk.get_vector_at::<i64>(1)?.get(0)?,
                Some(if commit { &7 } else { &0 })
            );
        }
        Ok(())
    }
}
