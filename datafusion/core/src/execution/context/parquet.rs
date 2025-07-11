// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::sync::Arc;

use super::super::options::{ParquetReadOptions, ReadOptions};
use super::{DataFilePaths, DataFrame, ExecutionPlan, Result, SessionContext};
use datafusion_datasource_parquet::plan_to_parquet;

use datafusion_common::TableReference;
use parquet::file::properties::WriterProperties;

impl SessionContext {
    /// Creates a [`DataFrame`] for reading a Parquet data source.
    ///
    /// For more control such as reading multiple files, you can use
    /// [`read_table`](Self::read_table) with a [`super::ListingTable`].
    ///
    /// For an example, see [`read_csv`](Self::read_csv)
    ///
    /// # Note: Statistics
    ///
    /// NOTE: by default, statistics are collected when reading the Parquet
    /// files This can slow down the initial DataFrame creation while
    /// greatly accelerating queries with certain filters.
    ///
    /// To disable statistics collection, set the [config option]
    /// `datafusion.execution.collect_statistics` to `false`. See
    /// [`ConfigOptions`] and [`ExecutionOptions::collect_statistics`] for more
    /// details.
    ///
    /// [config option]: https://datafusion.apache.org/user-guide/configs.html
    /// [`ConfigOptions`]: crate::config::ConfigOptions
    /// [`ExecutionOptions::collect_statistics`]: crate::config::ExecutionOptions::collect_statistics
    pub async fn read_parquet<P: DataFilePaths>(
        &self,
        table_paths: P,
        options: ParquetReadOptions<'_>,
    ) -> Result<DataFrame> {
        self._read_type(table_paths, options).await
    }

    /// Registers a Parquet file as a table that can be referenced from SQL
    /// statements executed against this context.
    ///
    /// # Note: Statistics
    ///
    /// Statistics are not collected by default. See  [`read_parquet`] for more
    /// details and how to enable them.
    ///
    /// [`read_parquet`]: Self::read_parquet
    pub async fn register_parquet(
        &self,
        table_ref: impl Into<TableReference>,
        table_path: impl AsRef<str>,
        options: ParquetReadOptions<'_>,
    ) -> Result<()> {
        let listing_options = options
            .to_listing_options(&self.copied_config(), self.copied_table_options());

        self.register_type_check(table_path.as_ref(), &listing_options.file_extension)?;

        self.register_listing_table(
            table_ref,
            table_path,
            listing_options,
            options.schema.map(|s| Arc::new(s.to_owned())),
            None,
        )
        .await?;
        Ok(())
    }

    /// Executes a query and writes the results to a partitioned Parquet file.
    pub async fn write_parquet(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        path: impl AsRef<str>,
        writer_properties: Option<WriterProperties>,
    ) -> Result<()> {
        plan_to_parquet(self.task_ctx(), plan, path, writer_properties).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrow::array::{Float32Array, Int32Array};
    use crate::arrow::datatypes::{DataType, Field, Schema};
    use crate::arrow::record_batch::RecordBatch;
    use crate::dataframe::DataFrameWriteOptions;
    use crate::parquet::basic::Compression;
    use crate::test_util::parquet_test_data;

    use arrow::util::pretty::pretty_format_batches;
    use datafusion_common::config::TableParquetOptions;
    use datafusion_common::{
        assert_batches_eq, assert_batches_sorted_eq, assert_contains,
    };
    use datafusion_execution::config::SessionConfig;

    use tempfile::{tempdir, TempDir};

    #[tokio::test]
    async fn read_with_glob_path() -> Result<()> {
        let ctx = SessionContext::new();

        let df = ctx
            .read_parquet(
                format!("{}/alltypes_plain*.parquet", parquet_test_data()),
                ParquetReadOptions::default(),
            )
            .await?;
        let results = df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        // alltypes_plain.parquet = 8 rows, alltypes_plain.snappy.parquet = 2 rows, alltypes_dictionary.parquet = 2 rows
        assert_eq!(total_rows, 10);
        Ok(())
    }

    #[tokio::test]
    async fn read_with_glob_path_issue_2465() -> Result<()> {
        let config =
            SessionConfig::from_string_hash_map(&std::collections::HashMap::from([(
                "datafusion.execution.listing_table_ignore_subdirectory".to_owned(),
                "false".to_owned(),
            )]))?;
        let ctx = SessionContext::new_with_config(config);
        let df = ctx
            .read_parquet(
                // it was reported that when a path contains // (two consecutive separator) no files were found
                // in this test, regardless of parquet_test_data() value, our path now contains a //
                format!("{}/..//*/alltypes_plain*.parquet", parquet_test_data()),
                ParquetReadOptions::default(),
            )
            .await?;
        let results = df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        // alltypes_plain.parquet = 8 rows, alltypes_plain.snappy.parquet = 2 rows, alltypes_dictionary.parquet = 2 rows
        assert_eq!(total_rows, 10);
        Ok(())
    }

    async fn explain_query_all_with_config(config: SessionConfig) -> Result<String> {
        let ctx = SessionContext::new_with_config(config);

        ctx.register_parquet(
            "test",
            &format!("{}/alltypes_plain*.parquet", parquet_test_data()),
            ParquetReadOptions::default(),
        )
        .await?;
        let df = ctx.sql("EXPLAIN SELECT * FROM test").await?;
        let results = df.collect().await?;
        let content = pretty_format_batches(&results).unwrap().to_string();
        Ok(content)
    }

    #[tokio::test]
    async fn register_parquet_respects_collect_statistics_config() -> Result<()> {
        // The default is true
        let mut config = SessionConfig::new();
        config.options_mut().explain.physical_plan_only = true;
        config.options_mut().explain.show_statistics = true;
        let content = explain_query_all_with_config(config).await?;
        assert_contains!(content, "statistics=[Rows=Exact(");

        // Explicitly set to true
        let mut config = SessionConfig::new();
        config.options_mut().explain.physical_plan_only = true;
        config.options_mut().explain.show_statistics = true;
        config.options_mut().execution.collect_statistics = true;
        let content = explain_query_all_with_config(config).await?;
        assert_contains!(content, "statistics=[Rows=Exact(");

        // Explicitly set to false
        let mut config = SessionConfig::new();
        config.options_mut().explain.physical_plan_only = true;
        config.options_mut().explain.show_statistics = true;
        config.options_mut().execution.collect_statistics = false;
        let content = explain_query_all_with_config(config).await?;
        assert_contains!(content, "statistics=[Rows=Absent,");

        Ok(())
    }

    #[tokio::test]
    async fn read_from_registered_table_with_glob_path() -> Result<()> {
        let ctx = SessionContext::new();

        ctx.register_parquet(
            "test",
            &format!("{}/alltypes_plain*.parquet", parquet_test_data()),
            ParquetReadOptions::default(),
        )
        .await?;
        let df = ctx.sql("SELECT * FROM test").await?;
        let results = df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        // alltypes_plain.parquet = 8 rows, alltypes_plain.snappy.parquet = 2 rows, alltypes_dictionary.parquet = 2 rows
        assert_eq!(total_rows, 10);
        Ok(())
    }

    #[tokio::test]
    async fn read_from_different_file_extension() -> Result<()> {
        let ctx = SessionContext::new();
        let sep = std::path::MAIN_SEPARATOR.to_string();

        // Make up a new dataframe.
        let write_df = ctx.read_batch(RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("purchase_id", DataType::Int32, false),
                Field::new("price", DataType::Float32, false),
                Field::new("quantity", DataType::Int32, false),
            ])),
            vec![
                Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5])),
                Arc::new(Float32Array::from(vec![1.12, 3.40, 2.33, 9.10, 6.66])),
                Arc::new(Int32Array::from(vec![1, 3, 2, 4, 3])),
            ],
        )?)?;

        let temp_dir = tempdir()?;
        let temp_dir_path = temp_dir.path();
        let path1 = temp_dir_path
            .join("output1.parquet")
            .to_str()
            .unwrap()
            .to_string();
        let path2 = temp_dir_path
            .join("output2.parquet.snappy")
            .to_str()
            .unwrap()
            .to_string();
        let path3 = temp_dir_path
            .join("output3.parquet.snappy.parquet")
            .to_str()
            .unwrap()
            .to_string();

        let path4 = temp_dir_path
            .join("output4.parquet".to_owned() + &sep)
            .to_str()
            .unwrap()
            .to_string();

        let path5 = temp_dir_path
            .join("bbb..bbb")
            .join("filename.parquet")
            .to_str()
            .unwrap()
            .to_string();
        let dir = temp_dir_path
            .join("bbb..bbb".to_owned() + &sep)
            .to_str()
            .unwrap()
            .to_string();
        std::fs::create_dir(dir).expect("create dir failed");

        let mut options = TableParquetOptions::default();
        options.global.compression = Some(Compression::SNAPPY.to_string());

        // Write the dataframe to a parquet file named 'output1.parquet'
        write_df
            .clone()
            .write_parquet(
                &path1,
                DataFrameWriteOptions::new().with_single_file_output(true),
                Some(options.clone()),
            )
            .await?;

        // Write the dataframe to a parquet file named 'output2.parquet.snappy'
        write_df
            .clone()
            .write_parquet(
                &path2,
                DataFrameWriteOptions::new().with_single_file_output(true),
                Some(options.clone()),
            )
            .await?;

        // Write the dataframe to a parquet file named 'output3.parquet.snappy.parquet'
        write_df
            .clone()
            .write_parquet(
                &path3,
                DataFrameWriteOptions::new().with_single_file_output(true),
                Some(options.clone()),
            )
            .await?;

        // Write the dataframe to a parquet file named 'bbb..bbb/filename.parquet'
        write_df
            .write_parquet(
                &path5,
                DataFrameWriteOptions::new().with_single_file_output(true),
                Some(options),
            )
            .await?;

        // Read the dataframe from 'output1.parquet' with the default file extension.
        let read_df = ctx
            .read_parquet(
                &path1,
                ParquetReadOptions {
                    ..Default::default()
                },
            )
            .await?;

        let results = read_df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        assert_eq!(total_rows, 5);

        // Read the dataframe from 'output2.parquet.snappy' with the correct file extension.
        let read_df = ctx
            .read_parquet(
                &path2,
                ParquetReadOptions {
                    file_extension: "snappy",
                    ..Default::default()
                },
            )
            .await?;
        let results = read_df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        assert_eq!(total_rows, 5);

        // Read the dataframe from 'output3.parquet.snappy.parquet' with the wrong file extension.
        let read_df = ctx
            .read_parquet(
                &path2,
                ParquetReadOptions {
                    ..Default::default()
                },
            )
            .await;
        let binding = DataFilePaths::to_urls(&path2).unwrap();
        let expected_path = binding[0].as_str();
        assert_eq!(
            read_df.unwrap_err().strip_backtrace(),
            format!("Execution error: File path '{expected_path}' does not match the expected extension '.parquet'")
        );

        // Read the dataframe from 'output3.parquet.snappy.parquet' with the correct file extension.
        let read_df = ctx
            .read_parquet(
                &path3,
                ParquetReadOptions {
                    ..Default::default()
                },
            )
            .await?;

        let results = read_df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        assert_eq!(total_rows, 5);

        // Read the dataframe from 'output4/'
        std::fs::create_dir(&path4)?;
        let read_df = ctx
            .read_parquet(
                &path4,
                ParquetReadOptions {
                    ..Default::default()
                },
            )
            .await?;

        let results = read_df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        assert_eq!(total_rows, 0);

        // Read the dataframe from double dot folder;
        let read_df = ctx
            .read_parquet(
                &path5,
                ParquetReadOptions {
                    ..Default::default()
                },
            )
            .await?;

        let results = read_df.collect().await?;
        let total_rows: usize = results.iter().map(|rb| rb.num_rows()).sum();
        assert_eq!(total_rows, 5);
        Ok(())
    }

    #[tokio::test]
    async fn read_from_parquet_folder() -> Result<()> {
        let ctx = SessionContext::new();
        let tmp_dir = TempDir::new()?;
        let test_path = tmp_dir.path().to_str().unwrap().to_string();

        ctx.sql("SELECT 1 a")
            .await?
            .write_parquet(&test_path, DataFrameWriteOptions::default(), None)
            .await?;

        ctx.sql("SELECT 2 a")
            .await?
            .write_parquet(&test_path, DataFrameWriteOptions::default(), None)
            .await?;

        // Adding CSV to check it is not read with Parquet reader
        ctx.sql("SELECT 3 a")
            .await?
            .write_csv(&test_path, DataFrameWriteOptions::default(), None)
            .await?;

        let actual = ctx
            .read_parquet(&test_path, ParquetReadOptions::default())
            .await?
            .collect()
            .await?;

        #[cfg_attr(any(), rustfmt::skip)]
        assert_batches_sorted_eq!(&[
            "+---+",
            "| a |",
            "+---+",
            "| 2 |",
            "| 1 |",
            "+---+",
        ], &actual);

        let actual = ctx
            .read_parquet(test_path, ParquetReadOptions::default())
            .await?
            .collect()
            .await?;

        #[cfg_attr(any(), rustfmt::skip)]
        assert_batches_sorted_eq!(&[
            "+---+",
            "| a |",
            "+---+",
            "| 2 |",
            "| 1 |",
            "+---+",
        ], &actual);

        Ok(())
    }

    #[tokio::test]
    async fn read_from_parquet_folder_table() -> Result<()> {
        let ctx = SessionContext::new();
        let tmp_dir = TempDir::new()?;
        let test_path = tmp_dir.path().to_str().unwrap().to_string();

        ctx.sql("SELECT 1 a")
            .await?
            .write_parquet(&test_path, DataFrameWriteOptions::default(), None)
            .await?;

        ctx.sql("SELECT 2 a")
            .await?
            .write_parquet(&test_path, DataFrameWriteOptions::default(), None)
            .await?;

        // Adding CSV to check it is not read with Parquet reader
        ctx.sql("SELECT 3 a")
            .await?
            .write_csv(&test_path, DataFrameWriteOptions::default(), None)
            .await?;

        ctx.sql(format!("CREATE EXTERNAL TABLE parquet_folder_t1 STORED AS PARQUET LOCATION '{test_path}'").as_ref())
            .await?;

        let actual = ctx
            .sql("select * from parquet_folder_t1")
            .await?
            .collect()
            .await?;
        #[cfg_attr(any(), rustfmt::skip)]
        assert_batches_sorted_eq!(&[
            "+---+",
            "| a |",
            "+---+",
            "| 2 |",
            "| 1 |",
            "+---+",
        ], &actual);

        Ok(())
    }

    #[tokio::test]
    async fn read_dummy_folder() -> Result<()> {
        let ctx = SessionContext::new();
        let test_path = "/foo/";

        let actual = ctx
            .read_parquet(test_path, ParquetReadOptions::default())
            .await?
            .collect()
            .await?;

        #[cfg_attr(any(), rustfmt::skip)]
        assert_batches_eq!(&[
            "++",
            "++",
        ], &actual);

        Ok(())
    }
}
