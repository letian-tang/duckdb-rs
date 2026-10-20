//! Batched Arrow scalar callbacks through C API V2. All C Data ownership stays here.
use crate::{
    Result,
    arrow::{ArrowArray, ArrowExporter, ArrowImporter, ArrowSchema},
    connection::{Connection, Context},
    data_chunk::VectorCollection,
    enums::{FunctionProperty, FunctionPropertyStability},
    error::Error,
    logical_type::LogicalType,
    scalar::{ScalarCallbacks, ScalarFunctionBuilder},
    signature::{FunctionParameter, SignatureBuilder},
    vector::{Unknown, Vector},
};
use arrow_rs::{
    array::{Array, RecordBatch, StructArray},
    datatypes::{DataType, Field, Fields},
    ffi::{FFI_ArrowArray, FFI_ArrowSchema, from_ffi, to_ffi},
};
use std::{marker::PhantomData, sync::Arc};

#[derive(Debug)]
pub struct ArrowFunctionSignature {
    pub parameters: Vec<DataType>,
    pub return_type: DataType,
}
impl ArrowFunctionSignature {
    pub fn exact(parameters: Vec<DataType>, return_type: DataType) -> Self {
        Self {
            parameters,
            return_type,
        }
    }
}
pub trait VArrowScalar: 'static {
    type State: Default + Send + Sync + 'static;
    fn invoke(
        state: &Self::State,
        input: RecordBatch,
    ) -> std::result::Result<Arc<dyn Array>, Box<dyn std::error::Error>>;
    fn signatures() -> Vec<ArrowFunctionSignature>;
    fn volatile() -> bool {
        false
    }
}
fn sql_type(ty: &DataType) -> Result<String> {
    Ok(match ty {
        DataType::Date32 => "DATE".into(),
        DataType::Time64(arrow_rs::datatypes::TimeUnit::Microsecond) => "TIME".into(),
        DataType::Time64(arrow_rs::datatypes::TimeUnit::Nanosecond) => "TIME_NS".into(),
        DataType::Timestamp(arrow_rs::datatypes::TimeUnit::Second, None) => "TIMESTAMP_SEC".into(),
        DataType::Timestamp(arrow_rs::datatypes::TimeUnit::Millisecond, None) => "TIMESTAMP_MS".into(),
        DataType::Timestamp(arrow_rs::datatypes::TimeUnit::Nanosecond, None) => "TIMESTAMP_NS".into(),
        DataType::Timestamp(arrow_rs::datatypes::TimeUnit::Microsecond, Some(_)) => "TIMESTAMP WITH TIME ZONE".into(),
        DataType::UInt8 => "UTINYINT".into(),
        DataType::UInt16 => "USMALLINT".into(),
        DataType::UInt32 => "UINTEGER".into(),
        DataType::Decimal128(width, scale) if *scale >= 0 => format!("DECIMAL({width},{scale})"),
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Int8 => "TINYINT".into(),
        DataType::Int16 => "SMALLINT".into(),
        DataType::Int32 => "INTEGER".into(),
        DataType::Int64 => "BIGINT".into(),
        DataType::UInt64 => "UBIGINT".into(),
        DataType::Float32 => "FLOAT".into(),
        DataType::Float64 => "DOUBLE".into(),
        DataType::Utf8 => "VARCHAR".into(),
        DataType::Binary => "BLOB".into(),
        DataType::Timestamp(arrow_rs::datatypes::TimeUnit::Microsecond, None) => "TIMESTAMP".into(),
        DataType::Struct(fields) => format!(
            "STRUCT({})",
            fields
                .iter()
                .map(|field| Ok(format!(
                    "\"{}\" {}",
                    field.name().replace('"', "\"\""),
                    sql_type(field.data_type())?
                )))
                .collect::<Result<Vec<_>>>()?
                .join(",")
        ),
        DataType::List(field) => format!("{}[]", sql_type(field.data_type())?),
        other => {
            return Err(Error::invalid_input(format!(
                "unsupported Arrow scalar signature: {other}"
            )));
        }
    })
}
struct Callback<T: VArrowScalar> {
    state: Arc<T::State>,
    return_type: DataType,
    marker: PhantomData<fn() -> T>,
}
impl<T: VArrowScalar> ScalarCallbacks for Callback<T> {
    type BindData = ();
    type InitData = ();
    fn exec(
        &self,
        _: Option<&()>,
        _: Option<&mut ()>,
        context: &Context,
        vectors: &mut VectorCollection,
        output: Vector<'_, Unknown>,
    ) -> Result<()> {
        let input = if vectors.col_count() == 0 {
            RecordBatch::try_new_with_options(
                Arc::new(arrow_rs::datatypes::Schema::empty()),
                vec![],
                &arrow_rs::array::RecordBatchOptions::new().with_row_count(Some(vectors.row_count()?)),
            )
            .map_err(|e| Error::invalid_input(e.to_string()))?
        } else {
            let chunk = vectors.to_data_chunk(context)?;
            let cols = (0..chunk.col_count()?)
                .map(|i| {
                    Ok((
                        format!("arg{i}"),
                        chunk.get_untyped_vector_at(i)?.logical_type().clone(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let names = cols
                .iter()
                .map(|(name, ty)| (name.as_str(), ty.clone()))
                .collect::<Vec<_>>();
            let mut exporter = ArrowExporter::new(context, &names, None)?;
            exporter.append(&chunk, true)?;
            let schema = exporter.schema()?;
            let array = exporter
                .next_array()?
                .ok_or_else(|| Error::internal("missing Arrow input batch"))?;
            // Both representations implement the Arrow C Data ABI. Ownership is moved,
            // never copied: exactly one side invokes each release callback.
            let schema: FFI_ArrowSchema = unsafe { std::mem::transmute(schema.into_raw()) };
            let array: FFI_ArrowArray = unsafe { std::mem::transmute(array.into_raw()) };
            let data = unsafe { from_ffi(array, &schema) }.map_err(|e| Error::invalid_input(e.to_string()))?;
            RecordBatch::from(StructArray::from(data))
        };
        let count = input.num_rows();
        let array = T::invoke(&self.state, input).map_err(|e| Error::invalid_input(e.to_string()))?;
        if array.data_type() != &self.return_type {
            return Err(Error::invalid_input("Arrow scalar returned an incorrect data type"));
        }
        if array.len() != count {
            return Err(Error::invalid_input("Arrow scalar returned an incorrect row count"));
        }
        let fields: Fields = vec![Field::new("result", array.data_type().clone(), true)].into();
        let result = StructArray::new(fields, vec![array], None);
        let (array, schema) = to_ffi(&result.to_data()).map_err(|e| Error::invalid_input(e.to_string()))?;
        let schema =
            unsafe { ArrowSchema::from_raw(std::mem::transmute::<FFI_ArrowSchema, crate::ffi::ArrowSchema>(schema)) };
        let array =
            unsafe { ArrowArray::from_raw(std::mem::transmute::<FFI_ArrowArray, crate::ffi::ArrowArray>(array)) };
        let importer = ArrowImporter::new(context, schema, None)?;
        importer.append_consumed(array, true)?;
        let chunk = importer
            .chunk()?
            .ok_or_else(|| Error::internal("missing Arrow scalar output"))?;
        // Copy to fully owning native buffers before referencing into the output.
        // Vector::ConstReference retains the native buffer (including children),
        // so the output owns the copied storage after this callback returns.
        let owned = chunk.copy(context)?;
        unsafe {
            output.copy_from(owned.get_untyped_vector_at(0)?)?;
        }
        Ok(())
    }
}
impl Connection {
    pub fn register_arrow_scalar<T: VArrowScalar>(&self, name: &str) -> Result<()> {
        self.register_arrow_scalar_state::<T>(name, Arc::new(T::State::default()))
    }
    pub fn register_arrow_scalar_with_state<T: VArrowScalar>(&self, name: &str, state: &T::State) -> Result<()>
    where
        T::State: Clone,
    {
        self.register_arrow_scalar_state::<T>(name, Arc::new(state.clone()))
    }
    fn register_arrow_scalar_state<T: VArrowScalar>(&self, name: &str, state: Arc<T::State>) -> Result<()> {
        for signature in T::signatures() {
            let mut builder =
                SignatureBuilder::new().return_type(LogicalType::from_text(self, &sql_type(&signature.return_type)?)?);
            for (i, ty) in signature.parameters.iter().enumerate() {
                builder = builder.param(FunctionParameter::with_type(
                    format!("arg{i}"),
                    LogicalType::from_text(self, &sql_type(ty)?)?,
                ));
            }
            ScalarFunctionBuilder::new(
                name,
                builder,
                Callback::<T> {
                    state: Arc::clone(&state),
                    return_type: signature.return_type.clone(),
                    marker: PhantomData,
                },
            )
            .set_property(FunctionProperty::IsFallible(true))
            .set_property(FunctionProperty::HasSpecialNullHandling(true))
            .set_property(FunctionProperty::Stability(if T::volatile() {
                FunctionPropertyStability::Volatile
            } else {
                FunctionPropertyStability::Consistent
            }))
            .register(self)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Parameters,
        environment::{Environment, StorageLocation},
    };
    use arrow_rs::array::Int64Array;

    struct Echo;
    impl VArrowScalar for Echo {
        type State = ();
        fn signatures() -> Vec<ArrowFunctionSignature> {
            vec![ArrowFunctionSignature::exact(vec![DataType::Binary], DataType::Binary)]
        }
        fn invoke(_: &(), input: RecordBatch) -> std::result::Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            Ok(input.column(0).clone())
        }
    }
    struct Constant;
    impl VArrowScalar for Constant {
        type State = ();
        fn signatures() -> Vec<ArrowFunctionSignature> {
            vec![ArrowFunctionSignature::exact(vec![], DataType::Int64)]
        }
        fn invoke(_: &(), input: RecordBatch) -> std::result::Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            Ok(Arc::new(Int64Array::from(vec![7; input.num_rows()])))
        }
        fn volatile() -> bool {
            true
        }
    }
    struct WrongType;
    impl VArrowScalar for WrongType {
        type State = ();
        fn signatures() -> Vec<ArrowFunctionSignature> {
            vec![ArrowFunctionSignature::exact(vec![], DataType::Binary)]
        }
        fn invoke(_: &(), input: RecordBatch) -> std::result::Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
            Constant::invoke(&(), input)
        }
    }
    #[test]
    fn callbacks_preserve_null_binary_and_zero_argument_batch_lengths() -> Result<()> {
        let conn = Environment::new()?.open(StorageLocation::InMemory)?.connect()?;
        conn.register_arrow_scalar::<Echo>("arrow_echo")?;
        conn.register_arrow_scalar::<Constant>("arrow_constant")?;
        let mut rows = conn.query_materialized("SELECT i, arrow_echo(CASE WHEN i % 3 = 0 THEN NULL ELSE '\\x00\\xFF'::BLOB END), arrow_constant() FROM range(5000) t(i)", Parameters::None)?;
        let mut count = 0;
        while let Some(row) = rows.next_row()? {
            assert_eq!(row.chunk.get_vector_at::<i64>(0)?.get(row.index)?, Some(&count));
            assert_eq!(
                row.chunk.get_vector_at::<crate::types::BlobValue>(1)?.get(row.index)?,
                if count % 3 == 0 { None } else { Some(&[0, 255][..]) }
            );
            assert_eq!(row.chunk.get_vector_at::<i64>(2)?.get(row.index)?, Some(&7));
            count += 1;
        }
        assert_eq!(count, 5000);
        Ok(())
    }
    #[test]
    fn invalid_callback_output_is_an_error_and_session_remains_usable() -> Result<()> {
        let conn = Environment::new()?.open(StorageLocation::InMemory)?.connect()?;
        conn.register_arrow_scalar::<WrongType>("wrong_arrow_type")?;
        let error = conn.execute("SELECT wrong_arrow_type()", Parameters::None).unwrap_err();
        assert!(error.message.contains("incorrect data type"), "{error}");
        conn.execute("SELECT 42", Parameters::None)?;
        Ok(())
    }
}
