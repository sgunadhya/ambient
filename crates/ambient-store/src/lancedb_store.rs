use ambient_core::{CoreError, LensConfig, LensIndexStore, Result};
use arrow::{
    array::{FixedSizeListArray, Float32Array, StringArray},
    datatypes::Float32Type,
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use lancedb::connection::Connection;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;
use uuid::Uuid;

pub struct LanceDbStore {
    db: Connection,
    runtime: Arc<Runtime>,
}

impl LanceDbStore {
    pub fn new() -> Result<Self> {
        let path = resolve_lancedb_path()?;
        let runtime = Arc::new(
            Runtime::new()
                .map_err(|e| CoreError::Internal(format!("failed to create runtime: {e}")))?,
        );

        let db = runtime
            .block_on(async { lancedb::connect(path.to_str().unwrap()).execute().await })
            .map_err(|e| CoreError::Internal(format!("failed to connect lancedb: {e}")))?;

        Ok(Self { db, runtime })
    }

    fn table_name(lens: &LensConfig) -> String {
        format!("lens_{}", lens.dimensions)
    }

    fn schema(dimensions: i32) -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("unit_id", DataType::Utf8, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dimensions,
                ),
                false,
            ),
        ]))
    }

    fn run_blocking<F, T>(&self, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.runtime.block_on(fut))
        } else {
            self.runtime.block_on(fut)
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn resolve_lancedb_path() -> Result<PathBuf> {
    let preferred = expand_home("~/.ambient/lancedb");
    std::fs::create_dir_all(&preferred).map_err(|e| {
        CoreError::Internal(format!(
            "failed to create lancedb directory at {}: {e}",
            preferred.display()
        ))
    })?;
    Ok(preferred)
}

impl LensIndexStore for LanceDbStore {
    fn upsert_lens_vec(&self, unit_id: Uuid, lens: &LensConfig, vector: &[f32]) -> Result<()> {
        let table_name = Self::table_name(lens);
        let schema = Self::schema(lens.dimensions as i32);

        if vector.len() != lens.dimensions {
            return Err(CoreError::InvalidInput(format!(
                "vector length {} does not match lens dimensions {}",
                vector.len(),
                lens.dimensions
            )));
        }

        let unit_id_str = unit_id.to_string();
        let unit_id_array = StringArray::from(vec![unit_id_str.clone()]);
        let vector_array = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            std::iter::once(Some(vector.iter().copied().map(Some).collect::<Vec<_>>())),
            lens.dimensions as i32,
        );

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(unit_id_array), Arc::new(vector_array)],
        )
        .map_err(|e| CoreError::Internal(format!("failed to create record batch: {e}")))?;

        self.run_blocking(async {
            let table_names = self
                .db
                .table_names()
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to list tables: {e}")))?;

            if !table_names.contains(&table_name) {
                let batches = Box::new(arrow::record_batch::RecordBatchIterator::new(
                    vec![Ok(batch.clone())],
                    schema.clone(),
                ));
                self.db
                    .create_table(&table_name, batches)
                    .execute()
                    .await
                    .map_err(|e| CoreError::Internal(format!("failed to create table: {e}")))?;
            } else {
                let table = self
                    .db
                    .open_table(&table_name)
                    .execute()
                    .await
                    .map_err(|e| CoreError::Internal(format!("failed to open table: {e}")))?;
                table
                    .delete(&format!("unit_id = '{}'", unit_id_str))
                    .await
                    .map_err(|e| {
                        CoreError::Internal(format!("failed to delete prior unit vector: {e}"))
                    })?;
                let batches = Box::new(arrow::record_batch::RecordBatchIterator::new(
                    vec![Ok(batch)],
                    schema.clone(),
                ));
                table
                    .add(batches)
                    .execute()
                    .await
                    .map_err(|e| CoreError::Internal(format!("failed to add data: {e}")))?;
            }
            Ok::<(), CoreError>(())
        })?;

        Ok(())
    }

    fn search_lens_vec(&self, lens: &LensConfig, query_vec: &[f32], k: usize) -> Result<Vec<Uuid>> {
        let table_name = Self::table_name(lens);

        self.run_blocking(async {
            let table_names = self
                .db
                .table_names()
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to list tables: {e}")))?;

            if !table_names.contains(&table_name) {
                return Ok(Vec::new());
            }

            let table = self
                .db
                .open_table(&table_name)
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to open table: {e}")))?;

            let stream = table
                .query()
                .nearest_to(query_vec)
                .map_err(|e| CoreError::Internal(format!("failed to build vector query: {e}")))?
                .distance_type(DistanceType::Cosine)
                .limit(k)
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to execute search: {e}")))?;

            use futures::StreamExt;
            let mut results = Vec::new();
            let mut stream = stream;

            while let Some(batch_res) = stream.next().await {
                let batch =
                    batch_res.map_err(|e| CoreError::Internal(format!("batch error: {e}")))?;
                let unit_id_col = batch
                    .column_by_name("unit_id")
                    .ok_or_else(|| CoreError::Internal("missing unit_id column".to_string()))?;

                let unit_id_col = unit_id_col
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| CoreError::Internal("unit_id is not StringArray".to_string()))?;

                for i in 0..batch.num_rows() {
                    if let Ok(id) = Uuid::parse_str(unit_id_col.value(i)) {
                        results.push(id);
                    }
                }
            }
            Ok(results)
        })
    }

    fn get_all_lens_vectors(&self, lens: &LensConfig) -> Result<Vec<(Uuid, Vec<f32>)>> {
        let table_name = Self::table_name(lens);

        self.run_blocking(async {
            let table_names = self
                .db
                .table_names()
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to list tables: {e}")))?;

            if !table_names.contains(&table_name) {
                return Ok(Vec::new());
            }

            let table = self
                .db
                .open_table(&table_name)
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to open table: {e}")))?;

            let stream = table
                .query()
                .execute()
                .await
                .map_err(|e| CoreError::Internal(format!("failed to execute query: {e}")))?;

            use futures::StreamExt;
            let mut results = Vec::new();
            let mut stream = stream;

            while let Some(batch_res) = stream.next().await {
                let batch =
                    batch_res.map_err(|e| CoreError::Internal(format!("batch error: {e}")))?;
                let unit_id_col = batch
                    .column_by_name("unit_id")
                    .ok_or_else(|| CoreError::Internal("missing unit_id column".to_string()))?;
                let unit_id_col = unit_id_col
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| CoreError::Internal("unit_id is not StringArray".to_string()))?;

                let vec_col = batch
                    .column_by_name("vector")
                    .ok_or_else(|| CoreError::Internal("missing vector column".to_string()))?;

                let vec_col = vec_col
                    .as_any()
                    .downcast_ref::<FixedSizeListArray>()
                    .ok_or_else(|| {
                        CoreError::Internal("vector is not FixedSizeListArray".to_string())
                    })?;

                for i in 0..batch.num_rows() {
                    if let Ok(id) = Uuid::parse_str(unit_id_col.value(i)) {
                        let row_vec = vec_col.value(i);
                        let float_values = row_vec
                            .as_any()
                            .downcast_ref::<Float32Array>()
                            .ok_or_else(|| {
                                CoreError::Internal("inner values are not Float32Array".to_string())
                            })?;
                        results.push((id, float_values.values().to_vec()));
                    }
                }
            }
            Ok(results)
        })
    }
}
