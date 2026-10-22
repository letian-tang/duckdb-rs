//! Safe boundary for C++ SQL frontend extensions linked into the host program.
//!
//! Extensions implement `duckdb_rs::SqlFrontendExtension` from the header exported
//! as `DEP_DUCKDB_RUST_FRONTEND_INCLUDE`. Their ASTs and dialect-specific payloads
//! stay on the C++ side. Rust owns returned strings; callback requests are borrowed
//! only during one synchronous call. Exceptions, callback panics and allocation
//! ownership are handled here rather than in applications.
use std::ffi::{CStr, CString, c_char, c_void};

use crate::{Connection, Error, Result, ffi};

#[repr(C)]
struct RewriteReply {
    value: *mut c_char,
    status: std::ffi::c_int,
}

unsafe extern "C" {
    pub(crate) fn duckdb_rs_transaction_state(connection: ffi::duckdb_connection) -> u8;
    fn duckdb_rs_frontend_initialize(name: *const c_char, connection: ffi::duckdb_connection) -> *mut c_char;
    fn duckdb_rs_frontend_parse(
        name: *const c_char,
        sql: *const c_char,
        dialect: bool,
        neutralize: bool,
    ) -> *mut c_char;
    fn duckdb_rs_frontend_read_only(
        name: *const c_char,
        connection: ffi::duckdb_connection,
        sql: *const c_char,
    ) -> *mut c_char;
    fn duckdb_rs_frontend_rewrite(
        name: *const c_char,
        sql: *const c_char,
        dialect: bool,
        callback: unsafe extern "C" fn(*mut c_void, *const c_char) -> RewriteReply,
        free_reply: unsafe extern "C" fn(*mut c_char),
        context: *mut c_void,
    ) -> *mut c_char;
    fn duckdb_rs_frontend_free(value: *mut c_char);
    #[cfg(feature = "vscalar")]
    fn duckdb_rs_scalar_set_fallible(function: ffi::duckdb_scalar_function) -> *mut c_char;
}

#[cfg(feature = "vscalar")]
pub(crate) fn mark_scalar_fallible(function: ffi::duckdb_scalar_function) -> Result<()> {
    take_result(unsafe { duckdb_rs_scalar_set_fallible(function) })?;
    Ok(())
}

fn error(message: impl Into<String>) -> Error {
    Error::DuckDBFailure(ffi::Error::new(ffi::DuckDBError), Some(message.into()))
}

fn take_result(pointer: *mut c_char) -> Result<String> {
    if pointer.is_null() {
        return Err(error("native SQL frontend returned no result"));
    }
    let result = unsafe { CStr::from_ptr(pointer) }.to_str().map(str::to_owned);
    unsafe { duckdb_rs_frontend_free(pointer) };
    let result = result?;
    if let Some(message) = result.strip_prefix("ERROR:") {
        // DuckDB C++ exceptions may serialize diagnostic JSON. Return the useful
        // message without exposing stack addresses as the public error text.
        let value = serde_json::from_str::<serde_json::Value>(message).ok();
        let text = value
            .as_ref()
            .and_then(|value| value.get("exception_message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(message);
        return Err(error(text));
    }
    Ok(result)
}

unsafe extern "C" fn invoke<F, E>(context: *mut c_void, request: *const c_char) -> RewriteReply
where
    F: FnMut(&str) -> std::result::Result<Option<String>, E>,
    E: std::fmt::Display,
{
    // Format errors inside containment too: application Display implementations
    // can panic just as callbacks can.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let callback = unsafe { &mut *context.cast::<F>() };
        let request = unsafe { CStr::from_ptr(request) }
            .to_str()
            .map_err(|error| error.to_string())?;
        callback(request).map_err(|error| error.to_string())
    }));
    let (value, status) = match outcome {
        Ok(Ok(None)) => {
            return RewriteReply {
                value: std::ptr::null_mut(),
                status: 0,
            };
        }
        Ok(Ok(Some(value))) => (value, 0),
        Ok(Err(error)) => (error, 1),
        Err(_) => ("SQL rewrite callback panicked".to_string(), 1),
    };
    let (value, status) = match CString::new(value) {
        Ok(value) => (value, status),
        Err(_) => (CString::new("SQL rewrite contains NUL").unwrap(), 1),
    };
    RewriteReply {
        value: value.into_raw(),
        status,
    }
}

unsafe extern "C" fn free_reply(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

/// A named, statically registered native SQL frontend. Does not own a connection
/// or retain ASTs. Dialect-specific payloads are returned as owned UTF-8 strings.
pub struct SqlFrontend {
    name: CString,
}

impl SqlFrontend {
    /// Select an extension by name. A missing extension fails when invoked;
    /// embedded NUL bytes are rejected immediately.
    pub fn new(name: &str) -> Result<Self> {
        Ok(Self {
            name: CString::new(name)?,
        })
    }

    /// Initialize extension grammar/functions using this borrowed connection.
    pub fn initialize(&self, connection: &Connection) -> Result<()> {
        let connection = connection.db.borrow_mut();
        take_result(unsafe { duckdb_rs_frontend_initialize(self.name.as_ptr(), connection.con) })?;
        Ok(())
    }

    /// Parse SQL with extension-defined dialect and parameter-neutralization
    /// options. The result format belongs to the selected extension.
    pub fn parse(&self, sql: &str, dialect: bool, neutralize: bool) -> Result<String> {
        let sql = CString::new(sql)?;
        take_result(unsafe { duckdb_rs_frontend_parse(self.name.as_ptr(), sql.as_ptr(), dialect, neutralize) })
    }

    /// Rewrite SQL using a synchronous Rust policy callback. Neither the callback
    /// nor its borrowed requests escape this invocation; panics become errors.
    pub fn rewrite<F, E>(&self, sql: &str, dialect: bool, mut callback: F) -> Result<String>
    where
        F: FnMut(&str) -> std::result::Result<Option<String>, E>,
        E: std::fmt::Display,
    {
        let sql = CString::new(sql)?;
        take_result(unsafe {
            duckdb_rs_frontend_rewrite(
                self.name.as_ptr(),
                sql.as_ptr(),
                dialect,
                invoke::<F, E>,
                free_reply,
                (&mut callback as *mut F).cast(),
            )
        })
    }

    /// Classify SQL through the extension, including binding when needed. Never
    /// executes the original statement or transfers ownership of the connection.
    pub fn is_read_only(&self, connection: &Connection, sql: &str) -> Result<bool> {
        let sql = CString::new(sql)?;
        let connection = connection.db.borrow_mut();
        Ok(
            take_result(unsafe { duckdb_rs_frontend_read_only(self.name.as_ptr(), connection.con, sql.as_ptr()) })?
                == "true",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_extension_returns_error_and_keeps_connection_usable() -> Result<()> {
        let connection = Connection::open_in_memory()?;
        let frontend = SqlFrontend::new("missing_test_extension")?;
        assert!(
            frontend
                .initialize(&connection)
                .unwrap_err()
                .to_string()
                .contains("unknown native SQL frontend")
        );
        assert!(frontend.parse("SELECT 1", false, false).is_err());
        assert!(frontend.is_read_only(&connection, "SELECT 1").is_err());
        assert_eq!(connection.query_row("SELECT 42", [], |row| row.get::<_, i32>(0))?, 42);
        assert!(SqlFrontend::new("invalid\0name").is_err());
        Ok(())
    }

    #[test]
    fn callback_panics_and_nul_are_contained() {
        fn call<F: FnMut(&str) -> std::result::Result<Option<String>, String>>(mut callback: F) -> String {
            let reply = unsafe { invoke::<F, String>((&mut callback as *mut F).cast(), c"request".as_ptr()) };
            assert_eq!(reply.status, 1);
            let value = unsafe { CStr::from_ptr(reply.value) }.to_str().unwrap().to_string();
            unsafe { free_reply(reply.value) };
            value
        }
        assert!(call(|_| panic!("policy panic")).contains("panicked"));
        assert!(call(|_| Ok(Some("invalid\0SQL".into()))).contains("NUL"));
    }
}

/// Actual engine transaction flags, independent of an application's autocommit setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionState {
    /// Whether the engine automatically commits each statement.
    pub autocommit: bool,
    /// Whether this connection currently owns a transaction.
    pub active: bool,
    /// Whether an error invalidated the active transaction.
    pub aborted: bool,
}

impl Connection {
    /// Read transaction state without executing a statement or changing its snapshot.
    pub fn transaction_state(&self) -> TransactionState {
        let connection = self.db.borrow();
        let flags = unsafe { duckdb_rs_transaction_state(connection.con) };
        TransactionState {
            autocommit: flags & 1 != 0,
            active: flags & 2 != 0,
            aborted: flags & 4 != 0,
        }
    }
}
