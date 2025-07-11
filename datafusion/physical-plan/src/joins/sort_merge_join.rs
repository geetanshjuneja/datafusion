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

//! Defines the Sort-Merge join execution plan.
//! A Sort-Merge join plan consumes two sorted children plans and produces
//! joined output by given join type and other options.

use std::any::Any;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::fmt::Formatter;
use std::fs::File;
use std::io::BufReader;
use std::mem::size_of;
use std::ops::Range;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::execution_plan::{boundedness_from_children, EmissionType};
use crate::expressions::PhysicalSortExpr;
use crate::joins::utils::{
    build_join_schema, check_join_is_valid, estimate_join_statistics,
    reorder_output_after_swap, symmetric_join_output_partitioning, JoinFilter, JoinOn,
    JoinOnRef,
};
use crate::metrics::{
    BaselineMetrics, Count, ExecutionPlanMetricsSet, MetricBuilder, MetricsSet,
    SpillMetrics,
};
use crate::projection::{
    join_allows_pushdown, join_table_borders, new_join_children,
    physical_to_column_exprs, update_join_on, ProjectionExec,
};
use crate::spill::spill_manager::SpillManager;
use crate::{
    metrics, DisplayAs, DisplayFormatType, Distribution, ExecutionPlan,
    ExecutionPlanProperties, PhysicalExpr, PlanProperties, RecordBatchStream,
    SendableRecordBatchStream, Statistics,
};

use arrow::array::{types::UInt64Type, *};
use arrow::compute::{
    self, concat_batches, filter_record_batch, is_not_null, take, SortOptions,
};
use arrow::datatypes::{DataType, SchemaRef, TimeUnit};
use arrow::error::ArrowError;
use arrow::ipc::reader::StreamReader;
use datafusion_common::config::SpillCompression;
use datafusion_common::{
    exec_err, internal_err, not_impl_err, plan_err, DataFusionError, HashSet, JoinSide,
    JoinType, NullEquality, Result,
};
use datafusion_execution::disk_manager::RefCountedTempFile;
use datafusion_execution::memory_pool::{MemoryConsumer, MemoryReservation};
use datafusion_execution::runtime_env::RuntimeEnv;
use datafusion_execution::TaskContext;
use datafusion_physical_expr::equivalence::join_equivalence_properties;
use datafusion_physical_expr_common::physical_expr::{fmt_sql, PhysicalExprRef};
use datafusion_physical_expr_common::sort_expr::{LexOrdering, OrderingRequirements};

use futures::{Stream, StreamExt};

/// Join execution plan that executes equi-join predicates on multiple partitions using Sort-Merge
/// join algorithm and applies an optional filter post join. Can be used to join arbitrarily large
/// inputs where one or both of the inputs don't fit in the available memory.
///
/// # Join Expressions
///
/// Equi-join predicate (e.g. `<col1> = <col2>`) expressions are represented by [`Self::on`].
///
/// Non-equality predicates, which can not be pushed down to join inputs (e.g.
/// `<col1> != <col2>`) are known as "filter expressions" and are evaluated
/// after the equijoin predicates. They are represented by [`Self::filter`]. These are optional
/// expressions.
///
/// # Sorting
///
/// Assumes that both the left and right input to the join are pre-sorted. It is not the
/// responsibility of this execution plan to sort the inputs.
///
/// # "Streamed" vs "Buffered"
///
/// The number of record batches of streamed input currently present in the memory will depend
/// on the output batch size of the execution plan. There is no spilling support for streamed input.
/// The comparisons are performed from values of join keys in streamed input with the values of
/// join keys in buffered input. One row in streamed record batch could be matched with multiple rows in
/// buffered input batches. The streamed input is managed through the states in `StreamedState`
/// and streamed input batches are represented by `StreamedBatch`.
///
/// Buffered input is buffered for all record batches having the same value of join key.
/// If the memory limit increases beyond the specified value and spilling is enabled,
/// buffered batches could be spilled to disk. If spilling is disabled, the execution
/// will fail under the same conditions. Multiple record batches of buffered could currently reside
/// in memory/disk during the execution. The number of buffered batches residing in
/// memory/disk depends on the number of rows of buffered input having the same value
/// of join key as that of streamed input rows currently present in memory. Due to pre-sorted inputs,
/// the algorithm understands when it is not needed anymore, and releases the buffered batches
/// from memory/disk. The buffered input is managed through the states in `BufferedState`
/// and buffered input batches are represented by `BufferedBatch`.
///
/// Depending on the type of join, left or right input may be selected as streamed or buffered
/// respectively. For example, in a left-outer join, the left execution plan will be selected as
/// streamed input while in a right-outer join, the right execution plan will be selected as the
/// streamed input.
///
/// Reference for the algorithm:
/// <https://en.wikipedia.org/wiki/Sort-merge_join>.
///
/// Helpful short video demonstration:
/// <https://www.youtube.com/watch?v=jiWCPJtDE2c>.
#[derive(Debug, Clone)]
pub struct SortMergeJoinExec {
    /// Left sorted joining execution plan
    pub left: Arc<dyn ExecutionPlan>,
    /// Right sorting joining execution plan
    pub right: Arc<dyn ExecutionPlan>,
    /// Set of common columns used to join on
    pub on: JoinOn,
    /// Filters which are applied while finding matching rows
    pub filter: Option<JoinFilter>,
    /// How the join is performed
    pub join_type: JoinType,
    /// The schema once the join is applied
    schema: SchemaRef,
    /// Execution metrics
    metrics: ExecutionPlanMetricsSet,
    /// The left SortExpr
    left_sort_exprs: LexOrdering,
    /// The right SortExpr
    right_sort_exprs: LexOrdering,
    /// Sort options of join columns used in sorting left and right execution plans
    pub sort_options: Vec<SortOptions>,
    /// Defines the null equality for the join.
    pub null_equality: NullEquality,
    /// Cache holding plan properties like equivalences, output partitioning etc.
    cache: PlanProperties,
}

impl SortMergeJoinExec {
    /// Tries to create a new [SortMergeJoinExec].
    /// The inputs are sorted using `sort_options` are applied to the columns in the `on`
    /// # Error
    /// This function errors when it is not possible to join the left and right sides on keys `on`.
    pub fn try_new(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        filter: Option<JoinFilter>,
        join_type: JoinType,
        sort_options: Vec<SortOptions>,
        null_equality: NullEquality,
    ) -> Result<Self> {
        let left_schema = left.schema();
        let right_schema = right.schema();

        check_join_is_valid(&left_schema, &right_schema, &on)?;
        if sort_options.len() != on.len() {
            return plan_err!(
                "Expected number of sort options: {}, actual: {}",
                on.len(),
                sort_options.len()
            );
        }

        let (left_sort_exprs, right_sort_exprs): (Vec<_>, Vec<_>) = on
            .iter()
            .zip(sort_options.iter())
            .map(|((l, r), sort_op)| {
                let left = PhysicalSortExpr {
                    expr: Arc::clone(l),
                    options: *sort_op,
                };
                let right = PhysicalSortExpr {
                    expr: Arc::clone(r),
                    options: *sort_op,
                };
                (left, right)
            })
            .unzip();
        let Some(left_sort_exprs) = LexOrdering::new(left_sort_exprs) else {
            return plan_err!(
                "SortMergeJoinExec requires valid sort expressions for its left side"
            );
        };
        let Some(right_sort_exprs) = LexOrdering::new(right_sort_exprs) else {
            return plan_err!(
                "SortMergeJoinExec requires valid sort expressions for its right side"
            );
        };

        let schema =
            Arc::new(build_join_schema(&left_schema, &right_schema, &join_type).0);
        let cache =
            Self::compute_properties(&left, &right, Arc::clone(&schema), join_type, &on)?;
        Ok(Self {
            left,
            right,
            on,
            filter,
            join_type,
            schema,
            metrics: ExecutionPlanMetricsSet::new(),
            left_sort_exprs,
            right_sort_exprs,
            sort_options,
            null_equality,
            cache,
        })
    }

    /// Get probe side (e.g streaming side) information for this sort merge join.
    /// In current implementation, probe side is determined according to join type.
    pub fn probe_side(join_type: &JoinType) -> JoinSide {
        // When output schema contains only the right side, probe side is right.
        // Otherwise probe side is the left side.
        match join_type {
            // TODO: sort merge support for right mark (tracked here: https://github.com/apache/datafusion/issues/16226)
            JoinType::Right
            | JoinType::RightSemi
            | JoinType::RightAnti
            | JoinType::RightMark => JoinSide::Right,
            JoinType::Inner
            | JoinType::Left
            | JoinType::Full
            | JoinType::LeftAnti
            | JoinType::LeftSemi
            | JoinType::LeftMark => JoinSide::Left,
        }
    }

    /// Calculate order preservation flags for this sort merge join.
    fn maintains_input_order(join_type: JoinType) -> Vec<bool> {
        match join_type {
            JoinType::Inner => vec![true, false],
            JoinType::Left
            | JoinType::LeftSemi
            | JoinType::LeftAnti
            | JoinType::LeftMark => vec![true, false],
            JoinType::Right
            | JoinType::RightSemi
            | JoinType::RightAnti
            | JoinType::RightMark => {
                vec![false, true]
            }
            _ => vec![false, false],
        }
    }

    /// Set of common columns used to join on
    pub fn on(&self) -> &[(PhysicalExprRef, PhysicalExprRef)] {
        &self.on
    }

    /// Ref to right execution plan
    pub fn right(&self) -> &Arc<dyn ExecutionPlan> {
        &self.right
    }

    /// Join type
    pub fn join_type(&self) -> JoinType {
        self.join_type
    }

    /// Ref to left execution plan
    pub fn left(&self) -> &Arc<dyn ExecutionPlan> {
        &self.left
    }

    /// Ref to join filter
    pub fn filter(&self) -> &Option<JoinFilter> {
        &self.filter
    }

    /// Ref to sort options
    pub fn sort_options(&self) -> &[SortOptions] {
        &self.sort_options
    }

    /// Null equality
    pub fn null_equality(&self) -> NullEquality {
        self.null_equality
    }

    /// This function creates the cache object that stores the plan properties such as schema, equivalence properties, ordering, partitioning, etc.
    fn compute_properties(
        left: &Arc<dyn ExecutionPlan>,
        right: &Arc<dyn ExecutionPlan>,
        schema: SchemaRef,
        join_type: JoinType,
        join_on: JoinOnRef,
    ) -> Result<PlanProperties> {
        // Calculate equivalence properties:
        let eq_properties = join_equivalence_properties(
            left.equivalence_properties().clone(),
            right.equivalence_properties().clone(),
            &join_type,
            schema,
            &Self::maintains_input_order(join_type),
            Some(Self::probe_side(&join_type)),
            join_on,
        )?;

        let output_partitioning =
            symmetric_join_output_partitioning(left, right, &join_type)?;

        Ok(PlanProperties::new(
            eq_properties,
            output_partitioning,
            EmissionType::Incremental,
            boundedness_from_children([left, right]),
        ))
    }

    pub fn swap_inputs(&self) -> Result<Arc<dyn ExecutionPlan>> {
        let left = self.left();
        let right = self.right();
        let new_join = SortMergeJoinExec::try_new(
            Arc::clone(right),
            Arc::clone(left),
            self.on()
                .iter()
                .map(|(l, r)| (Arc::clone(r), Arc::clone(l)))
                .collect::<Vec<_>>(),
            self.filter().as_ref().map(JoinFilter::swap),
            self.join_type().swap(),
            self.sort_options.clone(),
            self.null_equality,
        )?;

        // TODO: OR this condition with having a built-in projection (like
        //       ordinary hash join) when we support it.
        if matches!(
            self.join_type(),
            JoinType::LeftSemi
                | JoinType::RightSemi
                | JoinType::LeftAnti
                | JoinType::RightAnti
        ) {
            Ok(Arc::new(new_join))
        } else {
            reorder_output_after_swap(Arc::new(new_join), &left.schema(), &right.schema())
        }
    }
}

impl DisplayAs for SortMergeJoinExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                let on = self
                    .on
                    .iter()
                    .map(|(c1, c2)| format!("({c1}, {c2})"))
                    .collect::<Vec<String>>()
                    .join(", ");
                write!(
                    f,
                    "SortMergeJoin: join_type={:?}, on=[{}]{}",
                    self.join_type,
                    on,
                    self.filter.as_ref().map_or("".to_string(), |f| format!(
                        ", filter={}",
                        f.expression()
                    ))
                )
            }
            DisplayFormatType::TreeRender => {
                let on = self
                    .on
                    .iter()
                    .map(|(c1, c2)| {
                        format!("({} = {})", fmt_sql(c1.as_ref()), fmt_sql(c2.as_ref()))
                    })
                    .collect::<Vec<String>>()
                    .join(", ");

                if self.join_type() != JoinType::Inner {
                    writeln!(f, "join_type={:?}", self.join_type)?;
                }
                writeln!(f, "on={on}")
            }
        }
    }
}

impl ExecutionPlan for SortMergeJoinExec {
    fn name(&self) -> &'static str {
        "SortMergeJoinExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.cache
    }

    fn required_input_distribution(&self) -> Vec<Distribution> {
        let (left_expr, right_expr) = self
            .on
            .iter()
            .map(|(l, r)| (Arc::clone(l), Arc::clone(r)))
            .unzip();
        vec![
            Distribution::HashPartitioned(left_expr),
            Distribution::HashPartitioned(right_expr),
        ]
    }

    fn required_input_ordering(&self) -> Vec<Option<OrderingRequirements>> {
        vec![
            Some(OrderingRequirements::from(self.left_sort_exprs.clone())),
            Some(OrderingRequirements::from(self.right_sort_exprs.clone())),
        ]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        Self::maintains_input_order(self.join_type)
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.left, &self.right]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        match &children[..] {
            [left, right] => Ok(Arc::new(SortMergeJoinExec::try_new(
                Arc::clone(left),
                Arc::clone(right),
                self.on.clone(),
                self.filter.clone(),
                self.join_type,
                self.sort_options.clone(),
                self.null_equality,
            )?)),
            _ => internal_err!("SortMergeJoin wrong number of children"),
        }
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let left_partitions = self.left.output_partitioning().partition_count();
        let right_partitions = self.right.output_partitioning().partition_count();
        if left_partitions != right_partitions {
            return internal_err!(
                "Invalid SortMergeJoinExec, partition count mismatch {left_partitions}!={right_partitions},\
                 consider using RepartitionExec"
            );
        }
        let (on_left, on_right) = self.on.iter().cloned().unzip();
        let (streamed, buffered, on_streamed, on_buffered) =
            if SortMergeJoinExec::probe_side(&self.join_type) == JoinSide::Left {
                (
                    Arc::clone(&self.left),
                    Arc::clone(&self.right),
                    on_left,
                    on_right,
                )
            } else {
                (
                    Arc::clone(&self.right),
                    Arc::clone(&self.left),
                    on_right,
                    on_left,
                )
            };

        // execute children plans
        let streamed = streamed.execute(partition, Arc::clone(&context))?;
        let buffered = buffered.execute(partition, Arc::clone(&context))?;

        // create output buffer
        let batch_size = context.session_config().batch_size();

        // create memory reservation
        let reservation = MemoryConsumer::new(format!("SMJStream[{partition}]"))
            .register(context.memory_pool());

        // create join stream
        Ok(Box::pin(SortMergeJoinStream::try_new(
            context.session_config().spill_compression(),
            Arc::clone(&self.schema),
            self.sort_options.clone(),
            self.null_equality,
            streamed,
            buffered,
            on_streamed,
            on_buffered,
            self.filter.clone(),
            self.join_type,
            batch_size,
            SortMergeJoinMetrics::new(partition, &self.metrics),
            reservation,
            context.runtime_env(),
        )?))
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Result<Statistics> {
        self.partition_statistics(None)
    }

    fn partition_statistics(&self, partition: Option<usize>) -> Result<Statistics> {
        if partition.is_some() {
            return Ok(Statistics::new_unknown(&self.schema()));
        }
        // TODO stats: it is not possible in general to know the output size of joins
        // There are some special cases though, for example:
        // - `A LEFT JOIN B ON A.col=B.col` with `COUNT_DISTINCT(B.col)=COUNT(B.col)`
        estimate_join_statistics(
            self.left.partition_statistics(None)?,
            self.right.partition_statistics(None)?,
            self.on.clone(),
            &self.join_type,
            &self.schema,
        )
    }

    /// Tries to swap the projection with its input [`SortMergeJoinExec`]. If it can be done,
    /// it returns the new swapped version having the [`SortMergeJoinExec`] as the top plan.
    /// Otherwise, it returns None.
    fn try_swapping_with_projection(
        &self,
        projection: &ProjectionExec,
    ) -> Result<Option<Arc<dyn ExecutionPlan>>> {
        // Convert projected PhysicalExpr's to columns. If not possible, we cannot proceed.
        let Some(projection_as_columns) = physical_to_column_exprs(projection.expr())
        else {
            return Ok(None);
        };

        let (far_right_left_col_ind, far_left_right_col_ind) = join_table_borders(
            self.left().schema().fields().len(),
            &projection_as_columns,
        );

        if !join_allows_pushdown(
            &projection_as_columns,
            &self.schema(),
            far_right_left_col_ind,
            far_left_right_col_ind,
        ) {
            return Ok(None);
        }

        let Some(new_on) = update_join_on(
            &projection_as_columns[0..=far_right_left_col_ind as _],
            &projection_as_columns[far_left_right_col_ind as _..],
            self.on(),
            self.left().schema().fields().len(),
        ) else {
            return Ok(None);
        };

        let (new_left, new_right) = new_join_children(
            &projection_as_columns,
            far_right_left_col_ind,
            far_left_right_col_ind,
            self.children()[0],
            self.children()[1],
        )?;

        Ok(Some(Arc::new(SortMergeJoinExec::try_new(
            Arc::new(new_left),
            Arc::new(new_right),
            new_on,
            self.filter.clone(),
            self.join_type,
            self.sort_options.clone(),
            self.null_equality,
        )?)))
    }
}

/// Metrics for SortMergeJoinExec
#[allow(dead_code)]
struct SortMergeJoinMetrics {
    /// Total time for joining probe-side batches to the build-side batches
    join_time: metrics::Time,
    /// Number of batches consumed by this operator
    input_batches: Count,
    /// Number of rows consumed by this operator
    input_rows: Count,
    /// Number of batches produced by this operator
    output_batches: Count,
    /// Execution metrics
    baseline_metrics: BaselineMetrics,
    /// Peak memory used for buffered data.
    /// Calculated as sum of peak memory values across partitions
    peak_mem_used: metrics::Gauge,
    /// Metrics related to spilling
    spill_metrics: SpillMetrics,
}

impl SortMergeJoinMetrics {
    #[allow(dead_code)]
    pub fn new(partition: usize, metrics: &ExecutionPlanMetricsSet) -> Self {
        let join_time = MetricBuilder::new(metrics).subset_time("join_time", partition);
        let input_batches =
            MetricBuilder::new(metrics).counter("input_batches", partition);
        let input_rows = MetricBuilder::new(metrics).counter("input_rows", partition);
        let output_batches =
            MetricBuilder::new(metrics).counter("output_batches", partition);
        let peak_mem_used = MetricBuilder::new(metrics).gauge("peak_mem_used", partition);
        let spill_metrics = SpillMetrics::new(metrics, partition);

        let baseline_metrics = BaselineMetrics::new(metrics, partition);

        Self {
            join_time,
            input_batches,
            input_rows,
            output_batches,
            baseline_metrics,
            peak_mem_used,
            spill_metrics,
        }
    }
}

/// State of SMJ stream
#[derive(Debug, PartialEq, Eq)]
enum SortMergeJoinState {
    /// Init joining with a new streamed row or a new buffered batches
    Init,
    /// Polling one streamed row or one buffered batch, or both
    Polling,
    /// Joining polled data and making output
    JoinOutput,
    /// No more output
    Exhausted,
}

/// State of streamed data stream
#[derive(Debug, PartialEq, Eq)]
enum StreamedState {
    /// Init polling
    Init,
    /// Polling one streamed row
    Polling,
    /// Ready to produce one streamed row
    Ready,
    /// No more streamed row
    Exhausted,
}

/// State of buffered data stream
#[derive(Debug, PartialEq, Eq)]
enum BufferedState {
    /// Init polling
    Init,
    /// Polling first row in the next batch
    PollingFirst,
    /// Polling rest rows in the next batch
    PollingRest,
    /// Ready to produce one batch
    Ready,
    /// No more buffered batches
    Exhausted,
}

/// Represents a chunk of joined data from streamed and buffered side
struct StreamedJoinedChunk {
    /// Index of batch in buffered_data
    buffered_batch_idx: Option<usize>,
    /// Array builder for streamed indices
    streamed_indices: UInt64Builder,
    /// Array builder for buffered indices
    /// This could contain nulls if the join is null-joined
    buffered_indices: UInt64Builder,
}

/// Represents a record batch from streamed input.
///
/// Also stores information of matching rows from buffered batches.
struct StreamedBatch {
    /// The streamed record batch
    pub batch: RecordBatch,
    /// The index of row in the streamed batch to compare with buffered batches
    pub idx: usize,
    /// The join key arrays of streamed batch which are used to compare with buffered batches
    /// and to produce output. They are produced by evaluating `on` expressions.
    pub join_arrays: Vec<ArrayRef>,
    /// Chunks of indices from buffered side (may be nulls) joined to streamed
    pub output_indices: Vec<StreamedJoinedChunk>,
    /// Index of currently scanned batch from buffered data
    pub buffered_batch_idx: Option<usize>,
    /// Indices that found a match for the given join filter
    /// Used for semi joins to keep track the streaming index which got a join filter match
    /// and already emitted to the output.
    pub join_filter_matched_idxs: HashSet<u64>,
}

impl StreamedBatch {
    fn new(batch: RecordBatch, on_column: &[Arc<dyn PhysicalExpr>]) -> Self {
        let join_arrays = join_arrays(&batch, on_column);
        StreamedBatch {
            batch,
            idx: 0,
            join_arrays,
            output_indices: vec![],
            buffered_batch_idx: None,
            join_filter_matched_idxs: HashSet::new(),
        }
    }

    fn new_empty(schema: SchemaRef) -> Self {
        StreamedBatch {
            batch: RecordBatch::new_empty(schema),
            idx: 0,
            join_arrays: vec![],
            output_indices: vec![],
            buffered_batch_idx: None,
            join_filter_matched_idxs: HashSet::new(),
        }
    }

    /// Appends new pair consisting of current streamed index and `buffered_idx`
    /// index of buffered batch with `buffered_batch_idx` index.
    fn append_output_pair(
        &mut self,
        buffered_batch_idx: Option<usize>,
        buffered_idx: Option<usize>,
    ) {
        // If no current chunk exists or current chunk is not for current buffered batch,
        // create a new chunk
        if self.output_indices.is_empty() || self.buffered_batch_idx != buffered_batch_idx
        {
            self.output_indices.push(StreamedJoinedChunk {
                buffered_batch_idx,
                streamed_indices: UInt64Builder::with_capacity(1),
                buffered_indices: UInt64Builder::with_capacity(1),
            });
            self.buffered_batch_idx = buffered_batch_idx;
        };
        let current_chunk = self.output_indices.last_mut().unwrap();

        // Append index of streamed batch and index of buffered batch into current chunk
        current_chunk.streamed_indices.append_value(self.idx as u64);
        if let Some(idx) = buffered_idx {
            current_chunk.buffered_indices.append_value(idx as u64);
        } else {
            current_chunk.buffered_indices.append_null();
        }
    }
}

/// A buffered batch that contains contiguous rows with same join key
#[derive(Debug)]
struct BufferedBatch {
    /// The buffered record batch
    /// None if the batch spilled to disk th
    pub batch: Option<RecordBatch>,
    /// The range in which the rows share the same join key
    pub range: Range<usize>,
    /// Array refs of the join key
    pub join_arrays: Vec<ArrayRef>,
    /// Buffered joined index (null joining buffered)
    pub null_joined: Vec<usize>,
    /// Size estimation used for reserving / releasing memory
    pub size_estimation: usize,
    /// The indices of buffered batch that the join filter doesn't satisfy.
    /// This is a map between right row index and a boolean value indicating whether all joined row
    /// of the right row does not satisfy the filter .
    /// When dequeuing the buffered batch, we need to produce null joined rows for these indices.
    pub join_filter_not_matched_map: HashMap<u64, bool>,
    /// Current buffered batch number of rows. Equal to batch.num_rows()
    /// but if batch is spilled to disk this property is preferable
    /// and less expensive
    pub num_rows: usize,
    /// An optional temp spill file name on the disk if the batch spilled
    /// None by default
    /// Some(fileName) if the batch spilled to the disk
    pub spill_file: Option<RefCountedTempFile>,
}

impl BufferedBatch {
    fn new(
        batch: RecordBatch,
        range: Range<usize>,
        on_column: &[PhysicalExprRef],
    ) -> Self {
        let join_arrays = join_arrays(&batch, on_column);

        // Estimation is calculated as
        //   inner batch size
        // + join keys size
        // + worst case null_joined (as vector capacity * element size)
        // + Range size
        // + size of this estimation
        let size_estimation = batch.get_array_memory_size()
            + join_arrays
                .iter()
                .map(|arr| arr.get_array_memory_size())
                .sum::<usize>()
            + batch.num_rows().next_power_of_two() * size_of::<usize>()
            + size_of::<Range<usize>>()
            + size_of::<usize>();

        let num_rows = batch.num_rows();
        BufferedBatch {
            batch: Some(batch),
            range,
            join_arrays,
            null_joined: vec![],
            size_estimation,
            join_filter_not_matched_map: HashMap::new(),
            num_rows,
            spill_file: None,
        }
    }
}

/// Sort-Merge join stream that consumes streamed and buffered data streams
/// and produces joined output stream.
struct SortMergeJoinStream {
    // ========================================================================
    // PROPERTIES:
    // These fields are initialized at the start and remain constant throughout
    // the execution.
    // ========================================================================
    /// Output schema
    pub schema: SchemaRef,
    /// Defines the null equality for the join.
    pub null_equality: NullEquality,
    /// Sort options of join columns used to sort streamed and buffered data stream
    pub sort_options: Vec<SortOptions>,
    /// optional join filter
    pub filter: Option<JoinFilter>,
    /// How the join is performed
    pub join_type: JoinType,
    /// Target output batch size
    pub batch_size: usize,

    // ========================================================================
    // STREAMED FIELDS:
    // These fields manage the properties and state of the streamed input.
    // ========================================================================
    /// Input schema of streamed
    pub streamed_schema: SchemaRef,
    /// Streamed data stream
    pub streamed: SendableRecordBatchStream,
    /// Current processing record batch of streamed
    pub streamed_batch: StreamedBatch,
    /// (used in outer join) Is current streamed row joined at least once?
    pub streamed_joined: bool,
    /// State of streamed
    pub streamed_state: StreamedState,
    /// Join key columns of streamed
    pub on_streamed: Vec<PhysicalExprRef>,

    // ========================================================================
    // BUFFERED FIELDS:
    // These fields manage the properties and state of the buffered input.
    // ========================================================================
    /// Input schema of buffered
    pub buffered_schema: SchemaRef,
    /// Buffered data stream
    pub buffered: SendableRecordBatchStream,
    /// Current buffered data
    pub buffered_data: BufferedData,
    /// (used in outer join) Is current buffered batches joined at least once?
    pub buffered_joined: bool,
    /// State of buffered
    pub buffered_state: BufferedState,
    /// Join key columns of buffered
    pub on_buffered: Vec<PhysicalExprRef>,

    // ========================================================================
    // MERGE JOIN STATES:
    // These fields track the execution state of merge join and are updated
    // during the execution.
    // ========================================================================
    /// Current state of the stream
    pub state: SortMergeJoinState,
    /// Staging output array builders
    pub staging_output_record_batches: JoinedRecordBatches,
    /// Output buffer. Currently used by filtering as it requires double buffering
    /// to avoid small/empty batches. Non-filtered join outputs directly from `staging_output_record_batches.batches`
    pub output: RecordBatch,
    /// Staging output size, including output batches and staging joined results.
    /// Increased when we put rows into buffer and decreased after we actually output batches.
    /// Used to trigger output when sufficient rows are ready
    pub output_size: usize,
    /// The comparison result of current streamed row and buffered batches
    pub current_ordering: Ordering,
    /// Manages the process of spilling and reading back intermediate data
    pub spill_manager: SpillManager,

    // ========================================================================
    // EXECUTION RESOURCES:
    // Fields related to managing execution resources and monitoring performance.
    // ========================================================================
    /// Metrics
    pub join_metrics: SortMergeJoinMetrics,
    /// Memory reservation
    pub reservation: MemoryReservation,
    /// Runtime env
    pub runtime_env: Arc<RuntimeEnv>,
    /// A unique number for each batch
    pub streamed_batch_counter: AtomicUsize,
}

/// Joined batches with attached join filter information
struct JoinedRecordBatches {
    /// Joined batches. Each batch is already joined columns from left and right sources
    pub batches: Vec<RecordBatch>,
    /// Filter match mask for each row(matched/non-matched)
    pub filter_mask: BooleanBuilder,
    /// Left row indices to glue together rows in `batches` and `filter_mask`
    pub row_indices: UInt64Builder,
    /// Which unique batch id the row belongs to
    /// It is necessary to differentiate rows that are distributed the way when they point to the same
    /// row index but in not the same batches
    pub batch_ids: Vec<usize>,
}

impl JoinedRecordBatches {
    fn clear(&mut self) {
        self.batches.clear();
        self.batch_ids.clear();
        self.filter_mask = BooleanBuilder::new();
        self.row_indices = UInt64Builder::new();
    }
}
impl RecordBatchStream for SortMergeJoinStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

/// True if next index refers to either:
/// - another batch id
/// - another row index within same batch id
/// - end of row indices
#[inline(always)]
fn last_index_for_row(
    row_index: usize,
    indices: &UInt64Array,
    batch_ids: &[usize],
    indices_len: usize,
) -> bool {
    row_index == indices_len - 1
        || batch_ids[row_index] != batch_ids[row_index + 1]
        || indices.value(row_index) != indices.value(row_index + 1)
}

// Returns a corrected boolean bitmask for the given join type
// Values in the corrected bitmask can be: true, false, null
// `true` - the row found its match and sent to the output
// `null` - the row ignored, no output
// `false` - the row sent as NULL joined row
fn get_corrected_filter_mask(
    join_type: JoinType,
    row_indices: &UInt64Array,
    batch_ids: &[usize],
    filter_mask: &BooleanArray,
    expected_size: usize,
) -> Option<BooleanArray> {
    let row_indices_length = row_indices.len();
    let mut corrected_mask: BooleanBuilder =
        BooleanBuilder::with_capacity(row_indices_length);
    let mut seen_true = false;

    match join_type {
        JoinType::Left | JoinType::Right => {
            for i in 0..row_indices_length {
                let last_index =
                    last_index_for_row(i, row_indices, batch_ids, row_indices_length);
                if filter_mask.value(i) {
                    seen_true = true;
                    corrected_mask.append_value(true);
                } else if seen_true || !filter_mask.value(i) && !last_index {
                    corrected_mask.append_null(); // to be ignored and not set to output
                } else {
                    corrected_mask.append_value(false); // to be converted to null joined row
                }

                if last_index {
                    seen_true = false;
                }
            }

            // Generate null joined rows for records which have no matching join key
            corrected_mask.append_n(expected_size - corrected_mask.len(), false);
            Some(corrected_mask.finish())
        }
        JoinType::LeftMark => {
            for i in 0..row_indices_length {
                let last_index =
                    last_index_for_row(i, row_indices, batch_ids, row_indices_length);
                if filter_mask.value(i) && !seen_true {
                    seen_true = true;
                    corrected_mask.append_value(true);
                } else if seen_true || !filter_mask.value(i) && !last_index {
                    corrected_mask.append_null(); // to be ignored and not set to output
                } else {
                    corrected_mask.append_value(false); // to be converted to null joined row
                }

                if last_index {
                    seen_true = false;
                }
            }

            // Generate null joined rows for records which have no matching join key
            corrected_mask.append_n(expected_size - corrected_mask.len(), false);
            Some(corrected_mask.finish())
        }
        JoinType::LeftSemi | JoinType::RightSemi => {
            for i in 0..row_indices_length {
                let last_index =
                    last_index_for_row(i, row_indices, batch_ids, row_indices_length);
                if filter_mask.value(i) && !seen_true {
                    seen_true = true;
                    corrected_mask.append_value(true);
                } else {
                    corrected_mask.append_null(); // to be ignored and not set to output
                }

                if last_index {
                    seen_true = false;
                }
            }

            Some(corrected_mask.finish())
        }
        JoinType::LeftAnti | JoinType::RightAnti => {
            for i in 0..row_indices_length {
                let last_index =
                    last_index_for_row(i, row_indices, batch_ids, row_indices_length);

                if filter_mask.value(i) {
                    seen_true = true;
                }

                if last_index {
                    if !seen_true {
                        corrected_mask.append_value(true);
                    } else {
                        corrected_mask.append_null();
                    }

                    seen_true = false;
                } else {
                    corrected_mask.append_null();
                }
            }
            // Generate null joined rows for records which have no matching join key,
            // for LeftAnti non-matched considered as true
            corrected_mask.append_n(expected_size - corrected_mask.len(), true);
            Some(corrected_mask.finish())
        }
        JoinType::Full => {
            let mut mask: Vec<Option<bool>> = vec![Some(true); row_indices_length];
            let mut last_true_idx = 0;
            let mut first_row_idx = 0;
            let mut seen_false = false;

            for i in 0..row_indices_length {
                let last_index =
                    last_index_for_row(i, row_indices, batch_ids, row_indices_length);
                let val = filter_mask.value(i);
                let is_null = filter_mask.is_null(i);

                if val {
                    // memoize the first seen matched row
                    if !seen_true {
                        last_true_idx = i;
                    }
                    seen_true = true;
                }

                if is_null || val {
                    mask[i] = Some(true);
                } else if !is_null && !val && (seen_true || seen_false) {
                    mask[i] = None;
                } else {
                    mask[i] = Some(false);
                }

                if !is_null && !val {
                    seen_false = true;
                }

                if last_index {
                    // If the left row seen as true its needed to output it once
                    // To do that we mark all other matches for same row as null to avoid the output
                    if seen_true {
                        #[allow(clippy::needless_range_loop)]
                        for j in first_row_idx..last_true_idx {
                            mask[j] = None;
                        }
                    }

                    seen_true = false;
                    seen_false = false;
                    last_true_idx = 0;
                    first_row_idx = i + 1;
                }
            }

            Some(BooleanArray::from(mask))
        }
        // Only outer joins needs to keep track of processed rows and apply corrected filter mask
        _ => None,
    }
}

impl Stream for SortMergeJoinStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let join_time = self.join_metrics.join_time.clone();
        let _timer = join_time.timer();
        loop {
            match &self.state {
                SortMergeJoinState::Init => {
                    let streamed_exhausted =
                        self.streamed_state == StreamedState::Exhausted;
                    let buffered_exhausted =
                        self.buffered_state == BufferedState::Exhausted;
                    self.state = if streamed_exhausted && buffered_exhausted {
                        SortMergeJoinState::Exhausted
                    } else {
                        match self.current_ordering {
                            Ordering::Less | Ordering::Equal => {
                                if !streamed_exhausted {
                                    if self.filter.is_some()
                                        && matches!(
                                            self.join_type,
                                            JoinType::Left
                                                | JoinType::LeftSemi
                                                | JoinType::LeftMark
                                                | JoinType::Right
                                                | JoinType::RightSemi
                                                | JoinType::LeftAnti
                                                | JoinType::RightAnti
                                                | JoinType::Full
                                        )
                                    {
                                        self.freeze_all()?;

                                        // If join is filtered and there is joined tuples waiting
                                        // to be filtered
                                        if !self
                                            .staging_output_record_batches
                                            .batches
                                            .is_empty()
                                        {
                                            // Apply filter on joined tuples and get filtered batch
                                            let out_filtered_batch =
                                                self.filter_joined_batch()?;

                                            // Append filtered batch to the output buffer
                                            self.output = concat_batches(
                                                &self.schema(),
                                                vec![&self.output, &out_filtered_batch],
                                            )?;

                                            // Send to output if the output buffer surpassed the `batch_size`
                                            if self.output.num_rows() >= self.batch_size {
                                                let record_batch = std::mem::replace(
                                                    &mut self.output,
                                                    RecordBatch::new_empty(
                                                        out_filtered_batch.schema(),
                                                    ),
                                                );
                                                return Poll::Ready(Some(Ok(
                                                    record_batch,
                                                )));
                                            }
                                        }
                                    }

                                    self.streamed_joined = false;
                                    self.streamed_state = StreamedState::Init;
                                }
                            }
                            Ordering::Greater => {
                                if !buffered_exhausted {
                                    self.buffered_joined = false;
                                    self.buffered_state = BufferedState::Init;
                                }
                            }
                        }
                        SortMergeJoinState::Polling
                    };
                }
                SortMergeJoinState::Polling => {
                    if ![StreamedState::Exhausted, StreamedState::Ready]
                        .contains(&self.streamed_state)
                    {
                        match self.poll_streamed_row(cx)? {
                            Poll::Ready(_) => {}
                            Poll::Pending => return Poll::Pending,
                        }
                    }

                    if ![BufferedState::Exhausted, BufferedState::Ready]
                        .contains(&self.buffered_state)
                    {
                        match self.poll_buffered_batches(cx)? {
                            Poll::Ready(_) => {}
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    let streamed_exhausted =
                        self.streamed_state == StreamedState::Exhausted;
                    let buffered_exhausted =
                        self.buffered_state == BufferedState::Exhausted;
                    if streamed_exhausted && buffered_exhausted {
                        self.state = SortMergeJoinState::Exhausted;
                        continue;
                    }
                    self.current_ordering = self.compare_streamed_buffered()?;
                    self.state = SortMergeJoinState::JoinOutput;
                }
                SortMergeJoinState::JoinOutput => {
                    self.join_partial()?;

                    if self.output_size < self.batch_size {
                        if self.buffered_data.scanning_finished() {
                            self.buffered_data.scanning_reset();
                            self.state = SortMergeJoinState::Init;
                        }
                    } else {
                        self.freeze_all()?;
                        if !self.staging_output_record_batches.batches.is_empty() {
                            let record_batch = self.output_record_batch_and_reset()?;
                            // For non-filtered join output whenever the target output batch size
                            // is hit. For filtered join its needed to output on later phase
                            // because target output batch size can be hit in the middle of
                            // filtering causing the filtering to be incomplete and causing
                            // correctness issues
                            if self.filter.is_some()
                                && matches!(
                                    self.join_type,
                                    JoinType::Left
                                        | JoinType::LeftSemi
                                        | JoinType::Right
                                        | JoinType::RightSemi
                                        | JoinType::LeftAnti
                                        | JoinType::RightAnti
                                        | JoinType::LeftMark
                                        | JoinType::Full
                                )
                            {
                                continue;
                            }

                            return Poll::Ready(Some(Ok(record_batch)));
                        }
                        return Poll::Pending;
                    }
                }
                SortMergeJoinState::Exhausted => {
                    self.freeze_all()?;

                    // if there is still something not processed
                    if !self.staging_output_record_batches.batches.is_empty() {
                        if self.filter.is_some()
                            && matches!(
                                self.join_type,
                                JoinType::Left
                                    | JoinType::LeftSemi
                                    | JoinType::Right
                                    | JoinType::RightSemi
                                    | JoinType::LeftAnti
                                    | JoinType::RightAnti
                                    | JoinType::Full
                                    | JoinType::LeftMark
                            )
                        {
                            let record_batch = self.filter_joined_batch()?;
                            return Poll::Ready(Some(Ok(record_batch)));
                        } else {
                            let record_batch = self.output_record_batch_and_reset()?;
                            return Poll::Ready(Some(Ok(record_batch)));
                        }
                    } else if self.output.num_rows() > 0 {
                        // if processed but still not outputted because it didn't hit batch size before
                        let schema = self.output.schema();
                        let record_batch = std::mem::replace(
                            &mut self.output,
                            RecordBatch::new_empty(schema),
                        );
                        return Poll::Ready(Some(Ok(record_batch)));
                    } else {
                        return Poll::Ready(None);
                    }
                }
            }
        }
    }
}

impl SortMergeJoinStream {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        // Configured via `datafusion.execution.spill_compression`.
        spill_compression: SpillCompression,
        schema: SchemaRef,
        sort_options: Vec<SortOptions>,
        null_equality: NullEquality,
        streamed: SendableRecordBatchStream,
        buffered: SendableRecordBatchStream,
        on_streamed: Vec<Arc<dyn PhysicalExpr>>,
        on_buffered: Vec<Arc<dyn PhysicalExpr>>,
        filter: Option<JoinFilter>,
        join_type: JoinType,
        batch_size: usize,
        join_metrics: SortMergeJoinMetrics,
        reservation: MemoryReservation,
        runtime_env: Arc<RuntimeEnv>,
    ) -> Result<Self> {
        let streamed_schema = streamed.schema();
        let buffered_schema = buffered.schema();
        let spill_manager = SpillManager::new(
            Arc::clone(&runtime_env),
            join_metrics.spill_metrics.clone(),
            Arc::clone(&buffered_schema),
        )
        .with_compression_type(spill_compression);
        Ok(Self {
            state: SortMergeJoinState::Init,
            sort_options,
            null_equality,
            schema: Arc::clone(&schema),
            streamed_schema: Arc::clone(&streamed_schema),
            buffered_schema,
            streamed,
            buffered,
            streamed_batch: StreamedBatch::new_empty(streamed_schema),
            buffered_data: BufferedData::default(),
            streamed_joined: false,
            buffered_joined: false,
            streamed_state: StreamedState::Init,
            buffered_state: BufferedState::Init,
            current_ordering: Ordering::Equal,
            on_streamed,
            on_buffered,
            filter,
            staging_output_record_batches: JoinedRecordBatches {
                batches: vec![],
                filter_mask: BooleanBuilder::new(),
                row_indices: UInt64Builder::new(),
                batch_ids: vec![],
            },
            output: RecordBatch::new_empty(schema),
            output_size: 0,
            batch_size,
            join_type,
            join_metrics,
            reservation,
            runtime_env,
            spill_manager,
            streamed_batch_counter: AtomicUsize::new(0),
        })
    }

    /// Poll next streamed row
    fn poll_streamed_row(&mut self, cx: &mut Context) -> Poll<Option<Result<()>>> {
        loop {
            match &self.streamed_state {
                StreamedState::Init => {
                    if self.streamed_batch.idx + 1 < self.streamed_batch.batch.num_rows()
                    {
                        self.streamed_batch.idx += 1;
                        self.streamed_state = StreamedState::Ready;
                        return Poll::Ready(Some(Ok(())));
                    } else {
                        self.streamed_state = StreamedState::Polling;
                    }
                }
                StreamedState::Polling => match self.streamed.poll_next_unpin(cx)? {
                    Poll::Pending => {
                        return Poll::Pending;
                    }
                    Poll::Ready(None) => {
                        self.streamed_state = StreamedState::Exhausted;
                    }
                    Poll::Ready(Some(batch)) => {
                        if batch.num_rows() > 0 {
                            self.freeze_streamed()?;
                            self.join_metrics.input_batches.add(1);
                            self.join_metrics.input_rows.add(batch.num_rows());
                            self.streamed_batch =
                                StreamedBatch::new(batch, &self.on_streamed);
                            // Every incoming streaming batch should have its unique id
                            // Check `JoinedRecordBatches.self.streamed_batch_counter` documentation
                            self.streamed_batch_counter
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            self.streamed_state = StreamedState::Ready;
                        }
                    }
                },
                StreamedState::Ready => {
                    return Poll::Ready(Some(Ok(())));
                }
                StreamedState::Exhausted => {
                    return Poll::Ready(None);
                }
            }
        }
    }

    fn free_reservation(&mut self, buffered_batch: BufferedBatch) -> Result<()> {
        // Shrink memory usage for in-memory batches only
        if buffered_batch.spill_file.is_none() && buffered_batch.batch.is_some() {
            self.reservation
                .try_shrink(buffered_batch.size_estimation)?;
        }

        Ok(())
    }

    fn allocate_reservation(&mut self, mut buffered_batch: BufferedBatch) -> Result<()> {
        match self.reservation.try_grow(buffered_batch.size_estimation) {
            Ok(_) => {
                self.join_metrics
                    .peak_mem_used
                    .set_max(self.reservation.size());
                Ok(())
            }
            Err(_) if self.runtime_env.disk_manager.tmp_files_enabled() => {
                // Spill buffered batch to disk
                if let Some(batch) = buffered_batch.batch {
                    let spill_file = self
                        .spill_manager
                        .spill_record_batch_and_finish(
                            &[batch],
                            "sort_merge_join_buffered_spill",
                        )?
                        .unwrap(); // Operation only return None if no batches are spilled, here we ensure that at least one batch is spilled

                    buffered_batch.spill_file = Some(spill_file);
                    buffered_batch.batch = None;

                    Ok(())
                } else {
                    internal_err!("Buffered batch has empty body")
                }
            }
            Err(e) => exec_err!("{}. Disk spilling disabled.", e.message()),
        }?;

        self.buffered_data.batches.push_back(buffered_batch);
        Ok(())
    }

    /// Poll next buffered batches
    fn poll_buffered_batches(&mut self, cx: &mut Context) -> Poll<Option<Result<()>>> {
        loop {
            match &self.buffered_state {
                BufferedState::Init => {
                    // pop previous buffered batches
                    while !self.buffered_data.batches.is_empty() {
                        let head_batch = self.buffered_data.head_batch();
                        // If the head batch is fully processed, dequeue it and produce output of it.
                        if head_batch.range.end == head_batch.num_rows {
                            self.freeze_dequeuing_buffered()?;
                            if let Some(mut buffered_batch) =
                                self.buffered_data.batches.pop_front()
                            {
                                self.produce_buffered_not_matched(&mut buffered_batch)?;
                                self.free_reservation(buffered_batch)?;
                            }
                        } else {
                            // If the head batch is not fully processed, break the loop.
                            // Streamed batch will be joined with the head batch in the next step.
                            break;
                        }
                    }
                    if self.buffered_data.batches.is_empty() {
                        self.buffered_state = BufferedState::PollingFirst;
                    } else {
                        let tail_batch = self.buffered_data.tail_batch_mut();
                        tail_batch.range.start = tail_batch.range.end;
                        tail_batch.range.end += 1;
                        self.buffered_state = BufferedState::PollingRest;
                    }
                }
                BufferedState::PollingFirst => match self.buffered.poll_next_unpin(cx)? {
                    Poll::Pending => {
                        return Poll::Pending;
                    }
                    Poll::Ready(None) => {
                        self.buffered_state = BufferedState::Exhausted;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Some(batch)) => {
                        self.join_metrics.input_batches.add(1);
                        self.join_metrics.input_rows.add(batch.num_rows());

                        if batch.num_rows() > 0 {
                            let buffered_batch =
                                BufferedBatch::new(batch, 0..1, &self.on_buffered);

                            self.allocate_reservation(buffered_batch)?;
                            self.buffered_state = BufferedState::PollingRest;
                        }
                    }
                },
                BufferedState::PollingRest => {
                    if self.buffered_data.tail_batch().range.end
                        < self.buffered_data.tail_batch().num_rows
                    {
                        while self.buffered_data.tail_batch().range.end
                            < self.buffered_data.tail_batch().num_rows
                        {
                            if is_join_arrays_equal(
                                &self.buffered_data.head_batch().join_arrays,
                                self.buffered_data.head_batch().range.start,
                                &self.buffered_data.tail_batch().join_arrays,
                                self.buffered_data.tail_batch().range.end,
                            )? {
                                self.buffered_data.tail_batch_mut().range.end += 1;
                            } else {
                                self.buffered_state = BufferedState::Ready;
                                return Poll::Ready(Some(Ok(())));
                            }
                        }
                    } else {
                        match self.buffered.poll_next_unpin(cx)? {
                            Poll::Pending => {
                                return Poll::Pending;
                            }
                            Poll::Ready(None) => {
                                self.buffered_state = BufferedState::Ready;
                            }
                            Poll::Ready(Some(batch)) => {
                                // Polling batches coming concurrently as multiple partitions
                                self.join_metrics.input_batches.add(1);
                                self.join_metrics.input_rows.add(batch.num_rows());
                                if batch.num_rows() > 0 {
                                    let buffered_batch = BufferedBatch::new(
                                        batch,
                                        0..0,
                                        &self.on_buffered,
                                    );
                                    self.allocate_reservation(buffered_batch)?;
                                }
                            }
                        }
                    }
                }
                BufferedState::Ready => {
                    return Poll::Ready(Some(Ok(())));
                }
                BufferedState::Exhausted => {
                    return Poll::Ready(None);
                }
            }
        }
    }

    /// Get comparison result of streamed row and buffered batches
    fn compare_streamed_buffered(&self) -> Result<Ordering> {
        if self.streamed_state == StreamedState::Exhausted {
            return Ok(Ordering::Greater);
        }
        if !self.buffered_data.has_buffered_rows() {
            return Ok(Ordering::Less);
        }

        compare_join_arrays(
            &self.streamed_batch.join_arrays,
            self.streamed_batch.idx,
            &self.buffered_data.head_batch().join_arrays,
            self.buffered_data.head_batch().range.start,
            &self.sort_options,
            self.null_equality,
        )
    }

    /// Produce join and fill output buffer until reaching target batch size
    /// or the join is finished
    fn join_partial(&mut self) -> Result<()> {
        // Whether to join streamed rows
        let mut join_streamed = false;
        // Whether to join buffered rows
        let mut join_buffered = false;
        // For Mark join we store a dummy id to indicate the the row has a match
        let mut mark_row_as_match = false;

        // determine whether we need to join streamed/buffered rows
        match self.current_ordering {
            Ordering::Less => {
                if matches!(
                    self.join_type,
                    JoinType::Left
                        | JoinType::Right
                        | JoinType::Full
                        | JoinType::LeftAnti
                        | JoinType::RightAnti
                        | JoinType::LeftMark
                ) {
                    join_streamed = !self.streamed_joined;
                }
            }
            Ordering::Equal => {
                if matches!(
                    self.join_type,
                    JoinType::LeftSemi | JoinType::LeftMark | JoinType::RightSemi
                ) {
                    mark_row_as_match = matches!(self.join_type, JoinType::LeftMark);
                    // if the join filter is specified then its needed to output the streamed index
                    // only if it has not been emitted before
                    // the `join_filter_matched_idxs` keeps track on if streamed index has a successful
                    // filter match and prevents the same index to go into output more than once
                    if self.filter.is_some() {
                        join_streamed = !self
                            .streamed_batch
                            .join_filter_matched_idxs
                            .contains(&(self.streamed_batch.idx as u64))
                            && !self.streamed_joined;
                        // if the join filter specified there can be references to buffered columns
                        // so buffered columns are needed to access them
                        join_buffered = join_streamed;
                    } else {
                        join_streamed = !self.streamed_joined;
                    }
                }
                if matches!(
                    self.join_type,
                    JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Full
                ) {
                    join_streamed = true;
                    join_buffered = true;
                };

                if matches!(self.join_type, JoinType::LeftAnti | JoinType::RightAnti)
                    && self.filter.is_some()
                {
                    join_streamed = !self.streamed_joined;
                    join_buffered = join_streamed;
                }
            }
            Ordering::Greater => {
                if matches!(self.join_type, JoinType::Full) {
                    join_buffered = !self.buffered_joined;
                };
            }
        }
        if !join_streamed && !join_buffered {
            // no joined data
            self.buffered_data.scanning_finish();
            return Ok(());
        }

        if join_buffered {
            // joining streamed/nulls and buffered
            while !self.buffered_data.scanning_finished()
                && self.output_size < self.batch_size
            {
                let scanning_idx = self.buffered_data.scanning_idx();
                if join_streamed {
                    // Join streamed row and buffered row
                    self.streamed_batch.append_output_pair(
                        Some(self.buffered_data.scanning_batch_idx),
                        Some(scanning_idx),
                    );
                } else {
                    // Join nulls and buffered row for FULL join
                    self.buffered_data
                        .scanning_batch_mut()
                        .null_joined
                        .push(scanning_idx);
                }
                self.output_size += 1;
                self.buffered_data.scanning_advance();

                if self.buffered_data.scanning_finished() {
                    self.streamed_joined = join_streamed;
                    self.buffered_joined = true;
                }
            }
        } else {
            // joining streamed and nulls
            let scanning_batch_idx = if self.buffered_data.scanning_finished() {
                None
            } else {
                Some(self.buffered_data.scanning_batch_idx)
            };
            // For Mark join we store a dummy id to indicate the the row has a match
            let scanning_idx = mark_row_as_match.then_some(0);

            self.streamed_batch
                .append_output_pair(scanning_batch_idx, scanning_idx);
            self.output_size += 1;
            self.buffered_data.scanning_finish();
            self.streamed_joined = true;
        }
        Ok(())
    }

    fn freeze_all(&mut self) -> Result<()> {
        self.freeze_buffered(self.buffered_data.batches.len())?;
        self.freeze_streamed()?;
        Ok(())
    }

    // Produces and stages record batches to ensure dequeued buffered batch
    // no longer needed:
    //   1. freezes all indices joined to streamed side
    //   2. freezes NULLs joined to dequeued buffered batch to "release" it
    fn freeze_dequeuing_buffered(&mut self) -> Result<()> {
        self.freeze_streamed()?;
        // Only freeze and produce the first batch in buffered_data as the batch is fully processed
        self.freeze_buffered(1)?;
        Ok(())
    }

    // Produces and stages record batch from buffered indices with corresponding
    // NULLs on streamed side.
    //
    // Applicable only in case of Full join.
    //
    fn freeze_buffered(&mut self, batch_count: usize) -> Result<()> {
        if !matches!(self.join_type, JoinType::Full) {
            return Ok(());
        }
        for buffered_batch in self.buffered_data.batches.range_mut(..batch_count) {
            let buffered_indices = UInt64Array::from_iter_values(
                buffered_batch.null_joined.iter().map(|&index| index as u64),
            );
            if let Some(record_batch) = produce_buffered_null_batch(
                &self.schema,
                &self.streamed_schema,
                &buffered_indices,
                buffered_batch,
            )? {
                let num_rows = record_batch.num_rows();
                self.staging_output_record_batches
                    .filter_mask
                    .append_nulls(num_rows);
                self.staging_output_record_batches
                    .row_indices
                    .append_nulls(num_rows);
                self.staging_output_record_batches.batch_ids.resize(
                    self.staging_output_record_batches.batch_ids.len() + num_rows,
                    0,
                );

                self.staging_output_record_batches
                    .batches
                    .push(record_batch);
            }
            buffered_batch.null_joined.clear();
        }
        Ok(())
    }

    fn produce_buffered_not_matched(
        &mut self,
        buffered_batch: &mut BufferedBatch,
    ) -> Result<()> {
        if !matches!(self.join_type, JoinType::Full) {
            return Ok(());
        }

        // For buffered row which is joined with streamed side rows but all joined rows
        // don't satisfy the join filter
        let not_matched_buffered_indices = buffered_batch
            .join_filter_not_matched_map
            .iter()
            .filter_map(|(idx, failed)| if *failed { Some(*idx) } else { None })
            .collect::<Vec<_>>();

        let buffered_indices =
            UInt64Array::from_iter_values(not_matched_buffered_indices.iter().copied());

        if let Some(record_batch) = produce_buffered_null_batch(
            &self.schema,
            &self.streamed_schema,
            &buffered_indices,
            buffered_batch,
        )? {
            let num_rows = record_batch.num_rows();

            self.staging_output_record_batches
                .filter_mask
                .append_nulls(num_rows);
            self.staging_output_record_batches
                .row_indices
                .append_nulls(num_rows);
            self.staging_output_record_batches.batch_ids.resize(
                self.staging_output_record_batches.batch_ids.len() + num_rows,
                0,
            );
            self.staging_output_record_batches
                .batches
                .push(record_batch);
        }
        buffered_batch.join_filter_not_matched_map.clear();

        Ok(())
    }

    // Produces and stages record batch for all output indices found
    // for current streamed batch and clears staged output indices.
    fn freeze_streamed(&mut self) -> Result<()> {
        for chunk in self.streamed_batch.output_indices.iter_mut() {
            // The row indices of joined streamed batch
            let left_indices = chunk.streamed_indices.finish();

            if left_indices.is_empty() {
                continue;
            }

            let mut left_columns = self
                .streamed_batch
                .batch
                .columns()
                .iter()
                .map(|column| take(column, &left_indices, None))
                .collect::<Result<Vec<_>, ArrowError>>()?;

            // The row indices of joined buffered batch
            let right_indices: UInt64Array = chunk.buffered_indices.finish();
            let mut right_columns = if matches!(self.join_type, JoinType::LeftMark) {
                vec![Arc::new(is_not_null(&right_indices)?) as ArrayRef]
            } else if matches!(
                self.join_type,
                JoinType::LeftSemi
                    | JoinType::LeftAnti
                    | JoinType::RightAnti
                    | JoinType::RightSemi
            ) {
                vec![]
            } else if let Some(buffered_idx) = chunk.buffered_batch_idx {
                fetch_right_columns_by_idxs(
                    &self.buffered_data,
                    buffered_idx,
                    &right_indices,
                )?
            } else {
                // If buffered batch none, meaning it is null joined batch.
                // We need to create null arrays for buffered columns to join with streamed rows.
                create_unmatched_columns(
                    self.join_type,
                    &self.buffered_schema,
                    right_indices.len(),
                )
            };

            // Prepare the columns we apply join filter on later.
            // Only for joined rows between streamed and buffered.
            let filter_columns = if chunk.buffered_batch_idx.is_some() {
                if !matches!(self.join_type, JoinType::Right) {
                    if matches!(
                        self.join_type,
                        JoinType::LeftSemi | JoinType::LeftAnti | JoinType::LeftMark
                    ) {
                        let right_cols = fetch_right_columns_by_idxs(
                            &self.buffered_data,
                            chunk.buffered_batch_idx.unwrap(),
                            &right_indices,
                        )?;

                        get_filter_column(&self.filter, &left_columns, &right_cols)
                    } else if matches!(
                        self.join_type,
                        JoinType::RightAnti | JoinType::RightSemi
                    ) {
                        let right_cols = fetch_right_columns_by_idxs(
                            &self.buffered_data,
                            chunk.buffered_batch_idx.unwrap(),
                            &right_indices,
                        )?;

                        get_filter_column(&self.filter, &right_cols, &left_columns)
                    } else {
                        get_filter_column(&self.filter, &left_columns, &right_columns)
                    }
                } else {
                    get_filter_column(&self.filter, &right_columns, &left_columns)
                }
            } else {
                // This chunk is totally for null joined rows (outer join), we don't need to apply join filter.
                // Any join filter applied only on either streamed or buffered side will be pushed already.
                vec![]
            };

            let columns = if !matches!(self.join_type, JoinType::Right) {
                left_columns.extend(right_columns);
                left_columns
            } else {
                right_columns.extend(left_columns);
                right_columns
            };

            let output_batch = RecordBatch::try_new(Arc::clone(&self.schema), columns)?;
            // Apply join filter if any
            if !filter_columns.is_empty() {
                if let Some(f) = &self.filter {
                    // Construct batch with only filter columns
                    let filter_batch =
                        RecordBatch::try_new(Arc::clone(f.schema()), filter_columns)?;

                    let filter_result = f
                        .expression()
                        .evaluate(&filter_batch)?
                        .into_array(filter_batch.num_rows())?;

                    // The boolean selection mask of the join filter result
                    let pre_mask =
                        datafusion_common::cast::as_boolean_array(&filter_result)?;

                    // If there are nulls in join filter result, exclude them from selecting
                    // the rows to output.
                    let mask = if pre_mask.null_count() > 0 {
                        compute::prep_null_mask_filter(
                            datafusion_common::cast::as_boolean_array(&filter_result)?,
                        )
                    } else {
                        pre_mask.clone()
                    };

                    // Push the filtered batch which contains rows passing join filter to the output
                    if matches!(
                        self.join_type,
                        JoinType::Left
                            | JoinType::LeftSemi
                            | JoinType::Right
                            | JoinType::RightSemi
                            | JoinType::LeftAnti
                            | JoinType::RightAnti
                            | JoinType::LeftMark
                            | JoinType::Full
                    ) {
                        self.staging_output_record_batches
                            .batches
                            .push(output_batch);
                    } else {
                        let filtered_batch = filter_record_batch(&output_batch, &mask)?;
                        self.staging_output_record_batches
                            .batches
                            .push(filtered_batch);
                    }

                    if !matches!(self.join_type, JoinType::Full) {
                        self.staging_output_record_batches.filter_mask.extend(&mask);
                    } else {
                        self.staging_output_record_batches
                            .filter_mask
                            .extend(pre_mask);
                    }
                    self.staging_output_record_batches
                        .row_indices
                        .extend(&left_indices);
                    self.staging_output_record_batches.batch_ids.resize(
                        self.staging_output_record_batches.batch_ids.len()
                            + left_indices.len(),
                        self.streamed_batch_counter.load(Relaxed),
                    );

                    // For outer joins, we need to push the null joined rows to the output if
                    // all joined rows are failed on the join filter.
                    // I.e., if all rows joined from a streamed row are failed with the join filter,
                    // we need to join it with nulls as buffered side.
                    if matches!(self.join_type, JoinType::Full) {
                        let buffered_batch = &mut self.buffered_data.batches
                            [chunk.buffered_batch_idx.unwrap()];

                        for i in 0..pre_mask.len() {
                            // If the buffered row is not joined with streamed side,
                            // skip it.
                            if right_indices.is_null(i) {
                                continue;
                            }

                            let buffered_index = right_indices.value(i);

                            buffered_batch.join_filter_not_matched_map.insert(
                                buffered_index,
                                *buffered_batch
                                    .join_filter_not_matched_map
                                    .get(&buffered_index)
                                    .unwrap_or(&true)
                                    && !pre_mask.value(i),
                            );
                        }
                    }
                } else {
                    self.staging_output_record_batches
                        .batches
                        .push(output_batch);
                }
            } else {
                self.staging_output_record_batches
                    .batches
                    .push(output_batch);
            }
        }

        self.streamed_batch.output_indices.clear();

        Ok(())
    }

    fn output_record_batch_and_reset(&mut self) -> Result<RecordBatch> {
        let record_batch =
            concat_batches(&self.schema, &self.staging_output_record_batches.batches)?;
        self.join_metrics.output_batches.add(1);
        self.join_metrics
            .baseline_metrics
            .record_output(record_batch.num_rows());
        // If join filter exists, `self.output_size` is not accurate as we don't know the exact
        // number of rows in the output record batch. If streamed row joined with buffered rows,
        // once join filter is applied, the number of output rows may be more than 1.
        // If `record_batch` is empty, we should reset `self.output_size` to 0. It could be happened
        // when the join filter is applied and all rows are filtered out.
        if record_batch.num_rows() == 0 || record_batch.num_rows() > self.output_size {
            self.output_size = 0;
        } else {
            self.output_size -= record_batch.num_rows();
        }

        if !(self.filter.is_some()
            && matches!(
                self.join_type,
                JoinType::Left
                    | JoinType::LeftSemi
                    | JoinType::Right
                    | JoinType::RightSemi
                    | JoinType::LeftAnti
                    | JoinType::RightAnti
                    | JoinType::LeftMark
                    | JoinType::Full
            ))
        {
            self.staging_output_record_batches.batches.clear();
        }

        Ok(record_batch)
    }

    fn filter_joined_batch(&mut self) -> Result<RecordBatch> {
        let record_batch =
            concat_batches(&self.schema, &self.staging_output_record_batches.batches)?;
        let mut out_indices = self.staging_output_record_batches.row_indices.finish();
        let mut out_mask = self.staging_output_record_batches.filter_mask.finish();
        let mut batch_ids = &self.staging_output_record_batches.batch_ids;
        let default_batch_ids = vec![0; record_batch.num_rows()];

        // If only nulls come in and indices sizes doesn't match with expected record batch count
        // generate missing indices
        // Happens for null joined batches for Full Join
        if out_indices.null_count() == out_indices.len()
            && out_indices.len() != record_batch.num_rows()
        {
            out_mask = BooleanArray::from(vec![None; record_batch.num_rows()]);
            out_indices = UInt64Array::from(vec![None; record_batch.num_rows()]);
            batch_ids = &default_batch_ids;
        }

        if out_mask.is_empty() {
            self.staging_output_record_batches.batches.clear();
            return Ok(record_batch);
        }

        let maybe_corrected_mask = get_corrected_filter_mask(
            self.join_type,
            &out_indices,
            batch_ids,
            &out_mask,
            record_batch.num_rows(),
        );

        let corrected_mask = if let Some(ref filtered_join_mask) = maybe_corrected_mask {
            filtered_join_mask
        } else {
            &out_mask
        };

        self.filter_record_batch_by_join_type(record_batch, corrected_mask)
    }

    fn filter_record_batch_by_join_type(
        &mut self,
        record_batch: RecordBatch,
        corrected_mask: &BooleanArray,
    ) -> Result<RecordBatch> {
        let mut filtered_record_batch =
            filter_record_batch(&record_batch, corrected_mask)?;
        let left_columns_length = self.streamed_schema.fields.len();
        let right_columns_length = self.buffered_schema.fields.len();

        if matches!(
            self.join_type,
            JoinType::Left | JoinType::LeftMark | JoinType::Right
        ) {
            let null_mask = compute::not(corrected_mask)?;
            let null_joined_batch = filter_record_batch(&record_batch, &null_mask)?;

            let mut right_columns = create_unmatched_columns(
                self.join_type,
                &self.buffered_schema,
                null_joined_batch.num_rows(),
            );

            let columns = if !matches!(self.join_type, JoinType::Right) {
                let mut left_columns = null_joined_batch
                    .columns()
                    .iter()
                    .take(right_columns_length)
                    .cloned()
                    .collect::<Vec<_>>();

                left_columns.extend(right_columns);
                left_columns
            } else {
                let left_columns = null_joined_batch
                    .columns()
                    .iter()
                    .skip(left_columns_length)
                    .cloned()
                    .collect::<Vec<_>>();

                right_columns.extend(left_columns);
                right_columns
            };

            // Push the streamed/buffered batch joined nulls to the output
            let null_joined_streamed_batch =
                RecordBatch::try_new(Arc::clone(&self.schema), columns)?;

            filtered_record_batch = concat_batches(
                &self.schema,
                &[filtered_record_batch, null_joined_streamed_batch],
            )?;
        } else if matches!(self.join_type, JoinType::LeftSemi | JoinType::LeftAnti) {
            let output_column_indices = (0..left_columns_length).collect::<Vec<_>>();
            filtered_record_batch =
                filtered_record_batch.project(&output_column_indices)?;
        } else if matches!(self.join_type, JoinType::RightAnti | JoinType::RightSemi) {
            let output_column_indices = (0..right_columns_length).collect::<Vec<_>>();
            filtered_record_batch =
                filtered_record_batch.project(&output_column_indices)?;
        } else if matches!(self.join_type, JoinType::Full)
            && corrected_mask.false_count() > 0
        {
            // Find rows which joined by key but Filter predicate evaluated as false
            let joined_filter_not_matched_mask = compute::not(corrected_mask)?;
            let joined_filter_not_matched_batch =
                filter_record_batch(&record_batch, &joined_filter_not_matched_mask)?;

            // Add left unmatched rows adding the right side as nulls
            let right_null_columns = self
                .buffered_schema
                .fields()
                .iter()
                .map(|f| {
                    new_null_array(
                        f.data_type(),
                        joined_filter_not_matched_batch.num_rows(),
                    )
                })
                .collect::<Vec<_>>();

            let mut result_joined = joined_filter_not_matched_batch
                .columns()
                .iter()
                .take(left_columns_length)
                .cloned()
                .collect::<Vec<_>>();

            result_joined.extend(right_null_columns);

            let left_null_joined_batch =
                RecordBatch::try_new(Arc::clone(&self.schema), result_joined)?;

            // Add right unmatched rows adding the left side as nulls
            let mut result_joined = self
                .streamed_schema
                .fields()
                .iter()
                .map(|f| {
                    new_null_array(
                        f.data_type(),
                        joined_filter_not_matched_batch.num_rows(),
                    )
                })
                .collect::<Vec<_>>();

            let right_data = joined_filter_not_matched_batch
                .columns()
                .iter()
                .skip(left_columns_length)
                .cloned()
                .collect::<Vec<_>>();

            result_joined.extend(right_data);

            filtered_record_batch = concat_batches(
                &self.schema,
                &[filtered_record_batch, left_null_joined_batch],
            )?;
        }

        self.staging_output_record_batches.clear();

        Ok(filtered_record_batch)
    }
}

fn create_unmatched_columns(
    join_type: JoinType,
    schema: &SchemaRef,
    size: usize,
) -> Vec<ArrayRef> {
    if matches!(join_type, JoinType::LeftMark) {
        vec![Arc::new(BooleanArray::from(vec![false; size])) as ArrayRef]
    } else {
        schema
            .fields()
            .iter()
            .map(|f| new_null_array(f.data_type(), size))
            .collect::<Vec<_>>()
    }
}

/// Gets the arrays which join filters are applied on.
fn get_filter_column(
    join_filter: &Option<JoinFilter>,
    streamed_columns: &[ArrayRef],
    buffered_columns: &[ArrayRef],
) -> Vec<ArrayRef> {
    let mut filter_columns = vec![];

    if let Some(f) = join_filter {
        let left_columns = f
            .column_indices()
            .iter()
            .filter(|col_index| col_index.side == JoinSide::Left)
            .map(|i| Arc::clone(&streamed_columns[i.index]))
            .collect::<Vec<_>>();

        let right_columns = f
            .column_indices()
            .iter()
            .filter(|col_index| col_index.side == JoinSide::Right)
            .map(|i| Arc::clone(&buffered_columns[i.index]))
            .collect::<Vec<_>>();

        filter_columns.extend(left_columns);
        filter_columns.extend(right_columns);
    }

    filter_columns
}

fn produce_buffered_null_batch(
    schema: &SchemaRef,
    streamed_schema: &SchemaRef,
    buffered_indices: &PrimitiveArray<UInt64Type>,
    buffered_batch: &BufferedBatch,
) -> Result<Option<RecordBatch>> {
    if buffered_indices.is_empty() {
        return Ok(None);
    }

    // Take buffered (right) columns
    let right_columns =
        fetch_right_columns_from_batch_by_idxs(buffered_batch, buffered_indices)?;

    // Create null streamed (left) columns
    let mut left_columns = streamed_schema
        .fields()
        .iter()
        .map(|f| new_null_array(f.data_type(), buffered_indices.len()))
        .collect::<Vec<_>>();

    left_columns.extend(right_columns);

    Ok(Some(RecordBatch::try_new(
        Arc::clone(schema),
        left_columns,
    )?))
}

/// Get `buffered_indices` rows for `buffered_data[buffered_batch_idx]` by specific column indices
#[inline(always)]
fn fetch_right_columns_by_idxs(
    buffered_data: &BufferedData,
    buffered_batch_idx: usize,
    buffered_indices: &UInt64Array,
) -> Result<Vec<ArrayRef>> {
    fetch_right_columns_from_batch_by_idxs(
        &buffered_data.batches[buffered_batch_idx],
        buffered_indices,
    )
}

#[inline(always)]
fn fetch_right_columns_from_batch_by_idxs(
    buffered_batch: &BufferedBatch,
    buffered_indices: &UInt64Array,
) -> Result<Vec<ArrayRef>> {
    match (&buffered_batch.spill_file, &buffered_batch.batch) {
        // In memory batch
        (None, Some(batch)) => Ok(batch
            .columns()
            .iter()
            .map(|column| take(column, &buffered_indices, None))
            .collect::<Result<Vec<_>, ArrowError>>()
            .map_err(Into::<DataFusionError>::into)?),
        // If the batch was spilled to disk, less likely
        (Some(spill_file), None) => {
            let mut buffered_cols: Vec<ArrayRef> =
                Vec::with_capacity(buffered_indices.len());

            let file = BufReader::new(File::open(spill_file.path())?);
            let reader = StreamReader::try_new(file, None)?;

            for batch in reader {
                batch?.columns().iter().for_each(|column| {
                    buffered_cols.extend(take(column, &buffered_indices, None))
                });
            }

                Ok(buffered_cols)
            }
        // Invalid combination
        (spill, batch) => internal_err!("Unexpected buffered batch spill status. Spill exists: {}. In-memory exists: {}", spill.is_some(), batch.is_some()),
    }
}

/// Buffered data contains all buffered batches with one unique join key
#[derive(Debug, Default)]
struct BufferedData {
    /// Buffered batches with the same key
    pub batches: VecDeque<BufferedBatch>,
    /// current scanning batch index used in join_partial()
    pub scanning_batch_idx: usize,
    /// current scanning offset used in join_partial()
    pub scanning_offset: usize,
}

impl BufferedData {
    pub fn head_batch(&self) -> &BufferedBatch {
        self.batches.front().unwrap()
    }

    pub fn tail_batch(&self) -> &BufferedBatch {
        self.batches.back().unwrap()
    }

    pub fn tail_batch_mut(&mut self) -> &mut BufferedBatch {
        self.batches.back_mut().unwrap()
    }

    pub fn has_buffered_rows(&self) -> bool {
        self.batches.iter().any(|batch| !batch.range.is_empty())
    }

    pub fn scanning_reset(&mut self) {
        self.scanning_batch_idx = 0;
        self.scanning_offset = 0;
    }

    pub fn scanning_advance(&mut self) {
        self.scanning_offset += 1;
        while !self.scanning_finished() && self.scanning_batch_finished() {
            self.scanning_batch_idx += 1;
            self.scanning_offset = 0;
        }
    }

    pub fn scanning_batch(&self) -> &BufferedBatch {
        &self.batches[self.scanning_batch_idx]
    }

    pub fn scanning_batch_mut(&mut self) -> &mut BufferedBatch {
        &mut self.batches[self.scanning_batch_idx]
    }

    pub fn scanning_idx(&self) -> usize {
        self.scanning_batch().range.start + self.scanning_offset
    }

    pub fn scanning_batch_finished(&self) -> bool {
        self.scanning_offset == self.scanning_batch().range.len()
    }

    pub fn scanning_finished(&self) -> bool {
        self.scanning_batch_idx == self.batches.len()
    }

    pub fn scanning_finish(&mut self) {
        self.scanning_batch_idx = self.batches.len();
        self.scanning_offset = 0;
    }
}

/// Get join array refs of given batch and join columns
fn join_arrays(batch: &RecordBatch, on_column: &[PhysicalExprRef]) -> Vec<ArrayRef> {
    on_column
        .iter()
        .map(|c| {
            let num_rows = batch.num_rows();
            let c = c.evaluate(batch).unwrap();
            c.into_array(num_rows).unwrap()
        })
        .collect()
}

/// Get comparison result of two rows of join arrays
fn compare_join_arrays(
    left_arrays: &[ArrayRef],
    left: usize,
    right_arrays: &[ArrayRef],
    right: usize,
    sort_options: &[SortOptions],
    null_equality: NullEquality,
) -> Result<Ordering> {
    let mut res = Ordering::Equal;
    for ((left_array, right_array), sort_options) in
        left_arrays.iter().zip(right_arrays).zip(sort_options)
    {
        macro_rules! compare_value {
            ($T:ty) => {{
                let left_array = left_array.as_any().downcast_ref::<$T>().unwrap();
                let right_array = right_array.as_any().downcast_ref::<$T>().unwrap();
                match (left_array.is_null(left), right_array.is_null(right)) {
                    (false, false) => {
                        let left_value = &left_array.value(left);
                        let right_value = &right_array.value(right);
                        res = left_value.partial_cmp(right_value).unwrap();
                        if sort_options.descending {
                            res = res.reverse();
                        }
                    }
                    (true, false) => {
                        res = if sort_options.nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        };
                    }
                    (false, true) => {
                        res = if sort_options.nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        };
                    }
                    _ => {
                        res = match null_equality {
                            NullEquality::NullEqualsNothing => Ordering::Less,
                            NullEquality::NullEqualsNull => Ordering::Equal,
                        };
                    }
                }
            }};
        }

        match left_array.data_type() {
            DataType::Null => {}
            DataType::Boolean => compare_value!(BooleanArray),
            DataType::Int8 => compare_value!(Int8Array),
            DataType::Int16 => compare_value!(Int16Array),
            DataType::Int32 => compare_value!(Int32Array),
            DataType::Int64 => compare_value!(Int64Array),
            DataType::UInt8 => compare_value!(UInt8Array),
            DataType::UInt16 => compare_value!(UInt16Array),
            DataType::UInt32 => compare_value!(UInt32Array),
            DataType::UInt64 => compare_value!(UInt64Array),
            DataType::Float32 => compare_value!(Float32Array),
            DataType::Float64 => compare_value!(Float64Array),
            DataType::Utf8 => compare_value!(StringArray),
            DataType::Utf8View => compare_value!(StringViewArray),
            DataType::LargeUtf8 => compare_value!(LargeStringArray),
            DataType::Decimal128(..) => compare_value!(Decimal128Array),
            DataType::Timestamp(time_unit, None) => match time_unit {
                TimeUnit::Second => compare_value!(TimestampSecondArray),
                TimeUnit::Millisecond => compare_value!(TimestampMillisecondArray),
                TimeUnit::Microsecond => compare_value!(TimestampMicrosecondArray),
                TimeUnit::Nanosecond => compare_value!(TimestampNanosecondArray),
            },
            DataType::Date32 => compare_value!(Date32Array),
            DataType::Date64 => compare_value!(Date64Array),
            dt => {
                return not_impl_err!(
                    "Unsupported data type in sort merge join comparator: {}",
                    dt
                );
            }
        }
        if !res.is_eq() {
            break;
        }
    }
    Ok(res)
}

/// A faster version of compare_join_arrays() that only output whether
/// the given two rows are equal
fn is_join_arrays_equal(
    left_arrays: &[ArrayRef],
    left: usize,
    right_arrays: &[ArrayRef],
    right: usize,
) -> Result<bool> {
    let mut is_equal = true;
    for (left_array, right_array) in left_arrays.iter().zip(right_arrays) {
        macro_rules! compare_value {
            ($T:ty) => {{
                match (left_array.is_null(left), right_array.is_null(right)) {
                    (false, false) => {
                        let left_array =
                            left_array.as_any().downcast_ref::<$T>().unwrap();
                        let right_array =
                            right_array.as_any().downcast_ref::<$T>().unwrap();
                        if left_array.value(left) != right_array.value(right) {
                            is_equal = false;
                        }
                    }
                    (true, false) => is_equal = false,
                    (false, true) => is_equal = false,
                    _ => {}
                }
            }};
        }

        match left_array.data_type() {
            DataType::Null => {}
            DataType::Boolean => compare_value!(BooleanArray),
            DataType::Int8 => compare_value!(Int8Array),
            DataType::Int16 => compare_value!(Int16Array),
            DataType::Int32 => compare_value!(Int32Array),
            DataType::Int64 => compare_value!(Int64Array),
            DataType::UInt8 => compare_value!(UInt8Array),
            DataType::UInt16 => compare_value!(UInt16Array),
            DataType::UInt32 => compare_value!(UInt32Array),
            DataType::UInt64 => compare_value!(UInt64Array),
            DataType::Float32 => compare_value!(Float32Array),
            DataType::Float64 => compare_value!(Float64Array),
            DataType::Utf8 => compare_value!(StringArray),
            DataType::Utf8View => compare_value!(StringViewArray),
            DataType::LargeUtf8 => compare_value!(LargeStringArray),
            DataType::Decimal128(..) => compare_value!(Decimal128Array),
            DataType::Timestamp(time_unit, None) => match time_unit {
                TimeUnit::Second => compare_value!(TimestampSecondArray),
                TimeUnit::Millisecond => compare_value!(TimestampMillisecondArray),
                TimeUnit::Microsecond => compare_value!(TimestampMicrosecondArray),
                TimeUnit::Nanosecond => compare_value!(TimestampNanosecondArray),
            },
            DataType::Date32 => compare_value!(Date32Array),
            DataType::Date64 => compare_value!(Date64Array),
            dt => {
                return not_impl_err!(
                    "Unsupported data type in sort merge join comparator: {}",
                    dt
                );
            }
        }
        if !is_equal {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        builder::{BooleanBuilder, UInt64Builder},
        BooleanArray, Date32Array, Date64Array, Int32Array, RecordBatch, UInt64Array,
    };
    use arrow::compute::{concat_batches, filter_record_batch, SortOptions};
    use arrow::datatypes::{DataType, Field, Schema};

    use datafusion_common::JoinType::*;
    use datafusion_common::{
        assert_batches_eq, assert_contains, JoinType, NullEquality, Result,
    };
    use datafusion_common::{
        test_util::{batches_to_sort_string, batches_to_string},
        JoinSide,
    };
    use datafusion_execution::config::SessionConfig;
    use datafusion_execution::disk_manager::{DiskManagerBuilder, DiskManagerMode};
    use datafusion_execution::runtime_env::RuntimeEnvBuilder;
    use datafusion_execution::TaskContext;
    use datafusion_expr::Operator;
    use datafusion_physical_expr::expressions::BinaryExpr;
    use insta::{allow_duplicates, assert_snapshot};

    use crate::expressions::Column;
    use crate::joins::sort_merge_join::{get_corrected_filter_mask, JoinedRecordBatches};
    use crate::joins::utils::{ColumnIndex, JoinFilter, JoinOn};
    use crate::joins::SortMergeJoinExec;
    use crate::test::TestMemoryExec;
    use crate::test::{build_table_i32, build_table_i32_two_cols};
    use crate::{common, ExecutionPlan};

    fn build_table(
        a: (&str, &Vec<i32>),
        b: (&str, &Vec<i32>),
        c: (&str, &Vec<i32>),
    ) -> Arc<dyn ExecutionPlan> {
        let batch = build_table_i32(a, b, c);
        let schema = batch.schema();
        TestMemoryExec::try_new_exec(&[vec![batch]], schema, None).unwrap()
    }

    fn build_table_from_batches(batches: Vec<RecordBatch>) -> Arc<dyn ExecutionPlan> {
        let schema = batches.first().unwrap().schema();
        TestMemoryExec::try_new_exec(&[batches], schema, None).unwrap()
    }

    fn build_date_table(
        a: (&str, &Vec<i32>),
        b: (&str, &Vec<i32>),
        c: (&str, &Vec<i32>),
    ) -> Arc<dyn ExecutionPlan> {
        let schema = Schema::new(vec![
            Field::new(a.0, DataType::Date32, false),
            Field::new(b.0, DataType::Date32, false),
            Field::new(c.0, DataType::Date32, false),
        ]);

        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Date32Array::from(a.1.clone())),
                Arc::new(Date32Array::from(b.1.clone())),
                Arc::new(Date32Array::from(c.1.clone())),
            ],
        )
        .unwrap();

        let schema = batch.schema();
        TestMemoryExec::try_new_exec(&[vec![batch]], schema, None).unwrap()
    }

    fn build_date64_table(
        a: (&str, &Vec<i64>),
        b: (&str, &Vec<i64>),
        c: (&str, &Vec<i64>),
    ) -> Arc<dyn ExecutionPlan> {
        let schema = Schema::new(vec![
            Field::new(a.0, DataType::Date64, false),
            Field::new(b.0, DataType::Date64, false),
            Field::new(c.0, DataType::Date64, false),
        ]);

        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Date64Array::from(a.1.clone())),
                Arc::new(Date64Array::from(b.1.clone())),
                Arc::new(Date64Array::from(c.1.clone())),
            ],
        )
        .unwrap();

        let schema = batch.schema();
        TestMemoryExec::try_new_exec(&[vec![batch]], schema, None).unwrap()
    }

    /// returns a table with 3 columns of i32 in memory
    pub fn build_table_i32_nullable(
        a: (&str, &Vec<Option<i32>>),
        b: (&str, &Vec<Option<i32>>),
        c: (&str, &Vec<Option<i32>>),
    ) -> Arc<dyn ExecutionPlan> {
        let schema = Arc::new(Schema::new(vec![
            Field::new(a.0, DataType::Int32, true),
            Field::new(b.0, DataType::Int32, true),
            Field::new(c.0, DataType::Int32, true),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(a.1.clone())),
                Arc::new(Int32Array::from(b.1.clone())),
                Arc::new(Int32Array::from(c.1.clone())),
            ],
        )
        .unwrap();
        TestMemoryExec::try_new_exec(&[vec![batch]], schema, None).unwrap()
    }

    pub fn build_table_two_cols(
        a: (&str, &Vec<i32>),
        b: (&str, &Vec<i32>),
    ) -> Arc<dyn ExecutionPlan> {
        let batch = build_table_i32_two_cols(a, b);
        let schema = batch.schema();
        TestMemoryExec::try_new_exec(&[vec![batch]], schema, None).unwrap()
    }

    fn join(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        join_type: JoinType,
    ) -> Result<SortMergeJoinExec> {
        let sort_options = vec![SortOptions::default(); on.len()];
        SortMergeJoinExec::try_new(
            left,
            right,
            on,
            None,
            join_type,
            sort_options,
            NullEquality::NullEqualsNothing,
        )
    }

    fn join_with_options(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        join_type: JoinType,
        sort_options: Vec<SortOptions>,
        null_equality: NullEquality,
    ) -> Result<SortMergeJoinExec> {
        SortMergeJoinExec::try_new(
            left,
            right,
            on,
            None,
            join_type,
            sort_options,
            null_equality,
        )
    }

    fn join_with_filter(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        filter: JoinFilter,
        join_type: JoinType,
        sort_options: Vec<SortOptions>,
        null_equality: NullEquality,
    ) -> Result<SortMergeJoinExec> {
        SortMergeJoinExec::try_new(
            left,
            right,
            on,
            Some(filter),
            join_type,
            sort_options,
            null_equality,
        )
    }

    async fn join_collect(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        join_type: JoinType,
    ) -> Result<(Vec<String>, Vec<RecordBatch>)> {
        let sort_options = vec![SortOptions::default(); on.len()];
        join_collect_with_options(
            left,
            right,
            on,
            join_type,
            sort_options,
            NullEquality::NullEqualsNothing,
        )
        .await
    }

    async fn join_collect_with_filter(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        filter: JoinFilter,
        join_type: JoinType,
    ) -> Result<(Vec<String>, Vec<RecordBatch>)> {
        let sort_options = vec![SortOptions::default(); on.len()];

        let task_ctx = Arc::new(TaskContext::default());
        let join = join_with_filter(
            left,
            right,
            on,
            filter,
            join_type,
            sort_options,
            NullEquality::NullEqualsNothing,
        )?;
        let columns = columns(&join.schema());

        let stream = join.execute(0, task_ctx)?;
        let batches = common::collect(stream).await?;
        Ok((columns, batches))
    }

    async fn join_collect_with_options(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        join_type: JoinType,
        sort_options: Vec<SortOptions>,
        null_equality: NullEquality,
    ) -> Result<(Vec<String>, Vec<RecordBatch>)> {
        let task_ctx = Arc::new(TaskContext::default());
        let join =
            join_with_options(left, right, on, join_type, sort_options, null_equality)?;
        let columns = columns(&join.schema());

        let stream = join.execute(0, task_ctx)?;
        let batches = common::collect(stream).await?;
        Ok((columns, batches))
    }

    async fn join_collect_batch_size_equals_two(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        on: JoinOn,
        join_type: JoinType,
    ) -> Result<(Vec<String>, Vec<RecordBatch>)> {
        let task_ctx = TaskContext::default()
            .with_session_config(SessionConfig::new().with_batch_size(2));
        let task_ctx = Arc::new(task_ctx);
        let join = join(left, right, on, join_type)?;
        let columns = columns(&join.schema());

        let stream = join.execute(0, task_ctx)?;
        let batches = common::collect(stream).await?;
        Ok((columns, batches))
    }

    #[tokio::test]
    async fn join_inner_one() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![4, 5, 5]), // this has a repetition
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );

        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Inner).await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b1 | c2 |
            +----+----+----+----+----+----+
            | 1  | 4  | 7  | 10 | 4  | 70 |
            | 2  | 5  | 8  | 20 | 5  | 80 |
            | 3  | 5  | 9  | 20 | 5  | 80 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_inner_two() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b2", &vec![1, 2, 2]),
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a1", &vec![1, 2, 3]),
            ("b2", &vec![1, 2, 2]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b2", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
            ),
        ];

        let (_columns, batches) = join_collect(left, right, on, Inner).await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b2 | c1 | a1 | b2 | c2 |
            +----+----+----+----+----+----+
            | 1  | 1  | 7  | 1  | 1  | 70 |
            | 2  | 2  | 8  | 2  | 2  | 80 |
            | 2  | 2  | 9  | 2  | 2  | 80 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_inner_two_two() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 1, 2]),
            ("b2", &vec![1, 1, 2]),
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a1", &vec![1, 1, 3]),
            ("b2", &vec![1, 1, 2]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b2", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
            ),
        ];

        let (_columns, batches) = join_collect(left, right, on, Inner).await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b2 | c1 | a1 | b2 | c2 |
            +----+----+----+----+----+----+
            | 1  | 1  | 7  | 1  | 1  | 70 |
            | 1  | 1  | 7  | 1  | 1  | 80 |
            | 1  | 1  | 8  | 1  | 1  | 70 |
            | 1  | 1  | 8  | 1  | 1  | 80 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_inner_with_nulls() -> Result<()> {
        let left = build_table_i32_nullable(
            ("a1", &vec![Some(1), Some(1), Some(2), Some(2)]),
            ("b2", &vec![None, Some(1), Some(2), Some(2)]), // null in key field
            ("c1", &vec![Some(1), None, Some(8), Some(9)]), // null in non-key field
        );
        let right = build_table_i32_nullable(
            ("a1", &vec![Some(1), Some(1), Some(2), Some(3)]),
            ("b2", &vec![None, Some(1), Some(2), Some(2)]),
            ("c2", &vec![Some(10), Some(70), Some(80), Some(90)]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b2", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect(left, right, on, Inner).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b2 | c1 | a1 | b2 | c2 |
            +----+----+----+----+----+----+
            | 1  | 1  |    | 1  | 1  | 70 |
            | 2  | 2  | 8  | 2  | 2  | 80 |
            | 2  | 2  | 9  | 2  | 2  | 80 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_inner_with_nulls_with_options() -> Result<()> {
        let left = build_table_i32_nullable(
            ("a1", &vec![Some(2), Some(2), Some(1), Some(1)]),
            ("b2", &vec![Some(2), Some(2), Some(1), None]), // null in key field
            ("c1", &vec![Some(9), Some(8), None, Some(1)]), // null in non-key field
        );
        let right = build_table_i32_nullable(
            ("a1", &vec![Some(3), Some(2), Some(1), Some(1)]),
            ("b2", &vec![Some(2), Some(2), Some(1), None]),
            ("c2", &vec![Some(90), Some(80), Some(70), Some(10)]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b2", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
            ),
        ];
        let (_, batches) = join_collect_with_options(
            left,
            right,
            on,
            Inner,
            vec![
                SortOptions {
                    descending: true,
                    nulls_first: false,
                };
                2
            ],
            NullEquality::NullEqualsNull,
        )
        .await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b2 | c1 | a1 | b2 | c2 |
            +----+----+----+----+----+----+
            | 2  | 2  | 9  | 2  | 2  | 80 |
            | 2  | 2  | 8  | 2  | 2  | 80 |
            | 1  | 1  |    | 1  | 1  | 70 |
            | 1  |    | 1  | 1  |    | 10 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_inner_output_two_batches() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b2", &vec![1, 2, 2]),
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a1", &vec![1, 2, 3]),
            ("b2", &vec![1, 2, 2]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b2", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
            ),
        ];

        let (_, batches) =
            join_collect_batch_size_equals_two(left, right, on, Inner).await?;
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].num_rows(), 2);
        assert_eq!(batches[1].num_rows(), 1);
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b2 | c1 | a1 | b2 | c2 |
            +----+----+----+----+----+----+
            | 1  | 1  | 7  | 1  | 1  | 70 |
            | 2  | 2  | 8  | 2  | 2  | 80 |
            | 2  | 2  | 9  | 2  | 2  | 80 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_left_one() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![4, 5, 7]), // 7 does not exist on the right
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Left).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b1 | c2 |
            +----+----+----+----+----+----+
            | 1  | 4  | 7  | 10 | 4  | 70 |
            | 2  | 5  | 8  | 20 | 5  | 80 |
            | 3  | 7  | 9  |    |    |    |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_one() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![4, 5, 7]),
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]), // 6 does not exist on the left
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Right).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b1 | c2 |
            +----+----+----+----+----+----+
            | 1  | 4  | 7  | 10 | 4  | 70 |
            | 2  | 5  | 8  | 20 | 5  | 80 |
            |    |    |    | 30 | 6  | 90 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_full_one() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![4, 5, 7]), // 7 does not exist on the right
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b2", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema()).unwrap()) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema()).unwrap()) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Full).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_sort_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b2 | c2 |
            +----+----+----+----+----+----+
            |    |    |    | 30 | 6  | 90 |
            | 1  | 4  | 7  | 10 | 4  | 70 |
            | 2  | 5  | 8  | 20 | 5  | 80 |
            | 3  | 7  | 9  |    |    |    |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_left_anti() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2, 3, 5]),
            ("b1", &vec![4, 5, 5, 7, 7]), // 7 does not exist on the right
            ("c1", &vec![7, 8, 8, 9, 11]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, LeftAnti).await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+
            | a1 | b1 | c1 |
            +----+----+----+
            | 3  | 7  | 9  |
            | 5  | 7  | 11 |
            +----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_anti_one_one() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b1", &vec![4, 5, 5]),
            ("c1", &vec![7, 8, 8]),
        );
        let right =
            build_table_two_cols(("a2", &vec![10, 20, 30]), ("b1", &vec![4, 5, 6]));
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, RightAnti).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+
            | a2 | b1 |
            +----+----+
            | 30 | 6  |
            +----+----+
            "#);

        let left2 = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b1", &vec![4, 5, 5]),
            ("c1", &vec![7, 8, 8]),
        );
        let right2 = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );

        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left2.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right2.schema())?) as _,
        )];

        let (_, batches2) = join_collect(left2, right2, on, RightAnti).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches2), @r#"
            +----+----+----+
            | a2 | b1 | c2 |
            +----+----+----+
            | 30 | 6  | 90 |
            +----+----+----+
            "#);

        Ok(())
    }

    #[tokio::test]
    async fn join_right_anti_two_two() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b1", &vec![4, 5, 5]),
            ("c1", &vec![7, 8, 8]),
        );
        let right =
            build_table_two_cols(("a2", &vec![10, 20, 30]), ("b1", &vec![4, 5, 6]));
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a2", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect(left, right, on, RightAnti).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+
            | a2 | b1 |
            +----+----+
            | 10 | 4  |
            | 20 | 5  |
            | 30 | 6  |
            +----+----+
            "#);

        let left = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b1", &vec![4, 5, 5]),
            ("c1", &vec![7, 8, 8]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );

        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a2", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect(left, right, on, RightAnti).await?;
        let expected = [
            "+----+----+----+",
            "| a2 | b1 | c2 |",
            "+----+----+----+",
            "| 10 | 4  | 70 |",
            "| 20 | 5  | 80 |",
            "| 30 | 6  | 90 |",
            "+----+----+----+",
        ];
        // The output order is important as SMJ preserves sortedness
        assert_batches_eq!(expected, &batches);

        Ok(())
    }

    #[tokio::test]
    async fn join_right_anti_two_with_filter() -> Result<()> {
        let left = build_table(("a1", &vec![1]), ("b1", &vec![10]), ("c1", &vec![30]));
        let right = build_table(("a1", &vec![1]), ("b1", &vec![10]), ("c2", &vec![20]));
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];
        let filter = JoinFilter::new(
            Arc::new(BinaryExpr::new(
                Arc::new(Column::new("c2", 1)),
                Operator::Gt,
                Arc::new(Column::new("c1", 0)),
            )),
            vec![
                ColumnIndex {
                    index: 2,
                    side: JoinSide::Left,
                },
                ColumnIndex {
                    index: 2,
                    side: JoinSide::Right,
                },
            ],
            Arc::new(Schema::new(vec![
                Field::new("c1", DataType::Int32, true),
                Field::new("c2", DataType::Int32, true),
            ])),
        );
        let (_, batches) =
            join_collect_with_filter(left, right, on, filter, RightAnti).await?;
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+
            | a1 | b1 | c2 |
            +----+----+----+
            | 1  | 10 | 20 |
            +----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_anti_with_nulls() -> Result<()> {
        let left = build_table_i32_nullable(
            ("a1", &vec![Some(0), Some(1), Some(2), Some(2), Some(3)]),
            ("b1", &vec![Some(3), Some(4), Some(5), None, Some(6)]),
            ("c2", &vec![Some(60), None, Some(80), Some(85), Some(90)]),
        );
        let right = build_table_i32_nullable(
            ("a1", &vec![Some(1), Some(2), Some(2), Some(3)]),
            ("b1", &vec![Some(4), Some(5), None, Some(6)]), // null in key field
            ("c2", &vec![Some(7), Some(8), Some(8), None]), // null in non-key field
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect(left, right, on, RightAnti).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+
            | a1 | b1 | c2 |
            +----+----+----+
            | 2  |    | 8  |
            +----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_anti_with_nulls_with_options() -> Result<()> {
        let left = build_table_i32_nullable(
            ("a1", &vec![Some(1), Some(2), Some(1), Some(0), Some(2)]),
            ("b1", &vec![Some(4), Some(5), Some(5), None, Some(5)]),
            ("c1", &vec![Some(7), Some(8), Some(8), Some(60), None]),
        );
        let right = build_table_i32_nullable(
            ("a1", &vec![Some(3), Some(2), Some(2), Some(1)]),
            ("b1", &vec![None, Some(5), Some(5), Some(4)]), // null in key field
            ("c2", &vec![Some(9), None, Some(8), Some(7)]), // null in non-key field
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect_with_options(
            left,
            right,
            on,
            RightAnti,
            vec![
                SortOptions {
                    descending: true,
                    nulls_first: false,
                };
                2
            ],
            NullEquality::NullEqualsNull,
        )
        .await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+
            | a1 | b1 | c2 |
            +----+----+----+
            | 3  |    | 9  |
            | 2  | 5  |    |
            | 2  | 5  | 8  |
            +----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_anti_output_two_batches() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2]),
            ("b1", &vec![4, 5, 5]),
            ("c1", &vec![7, 8, 8]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]),
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a2", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) =
            join_collect_batch_size_equals_two(left, right, on, LeftAnti).await?;
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].num_rows(), 2);
        assert_eq!(batches[1].num_rows(), 1);
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+
            | a1 | b1 | c1 |
            +----+----+----+
            | 1  | 4  | 7  |
            | 2  | 5  | 8  |
            | 2  | 5  | 8  |
            +----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_left_semi() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 7]), // 7 does not exist on the right
            ("c1", &vec![7, 8, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![4, 5, 6]), // 5 is double on the right
            ("c2", &vec![70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, LeftSemi).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+
            | a1 | b1 | c1 |
            +----+----+----+
            | 1  | 4  | 7  |
            | 2  | 5  | 8  |
            | 2  | 5  | 8  |
            +----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_semi_one() -> Result<()> {
        let left = build_table(
            ("a1", &vec![10, 20, 30, 40]),
            ("b1", &vec![4, 5, 5, 6]),
            ("c1", &vec![70, 80, 90, 100]),
        );
        let right = build_table(
            ("a2", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 7]),
            ("c2", &vec![7, 8, 8, 9]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, RightSemi).await?;
        let expected = [
            "+----+----+----+",
            "| a2 | b1 | c2 |",
            "+----+----+----+",
            "| 1  | 4  | 7  |",
            "| 2  | 5  | 8  |",
            "| 2  | 5  | 8  |",
            "+----+----+----+",
        ];
        assert_batches_eq!(expected, &batches);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_semi_two() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 6]),
            ("c1", &vec![70, 80, 90, 100]),
        );
        let right = build_table(
            ("a1", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 7]),
            ("c2", &vec![7, 8, 8, 9]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect(left, right, on, RightSemi).await?;
        let expected = [
            "+----+----+----+",
            "| a1 | b1 | c2 |",
            "+----+----+----+",
            "| 1  | 4  | 7  |",
            "| 2  | 5  | 8  |",
            "| 2  | 5  | 8  |",
            "+----+----+----+",
        ];
        assert_batches_eq!(expected, &batches);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_semi_two_with_filter() -> Result<()> {
        let left = build_table(("a1", &vec![1]), ("b1", &vec![10]), ("c1", &vec![30]));
        let right = build_table(("a1", &vec![1]), ("b1", &vec![10]), ("c2", &vec![20]));
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];
        let filter = JoinFilter::new(
            Arc::new(BinaryExpr::new(
                Arc::new(Column::new("c2", 1)),
                Operator::Lt,
                Arc::new(Column::new("c1", 0)),
            )),
            vec![
                ColumnIndex {
                    index: 2,
                    side: JoinSide::Left,
                },
                ColumnIndex {
                    index: 2,
                    side: JoinSide::Right,
                },
            ],
            Arc::new(Schema::new(vec![
                Field::new("c1", DataType::Int32, true),
                Field::new("c2", DataType::Int32, true),
            ])),
        );
        let (_, batches) =
            join_collect_with_filter(left, right, on, filter, RightSemi).await?;
        let expected = [
            "+----+----+----+",
            "| a1 | b1 | c2 |",
            "+----+----+----+",
            "| 1  | 10 | 20 |",
            "+----+----+----+",
        ];
        assert_batches_eq!(expected, &batches);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_semi_with_nulls() -> Result<()> {
        let left = build_table_i32_nullable(
            ("a1", &vec![Some(0), Some(1), Some(2), Some(2), Some(3)]),
            ("b1", &vec![Some(3), Some(4), Some(5), None, Some(6)]),
            ("c2", &vec![Some(60), None, Some(80), Some(85), Some(90)]),
        );
        let right = build_table_i32_nullable(
            ("a1", &vec![Some(1), Some(2), Some(2), Some(3)]),
            ("b1", &vec![Some(4), Some(5), None, Some(6)]), // null in key field
            ("c2", &vec![Some(7), Some(8), Some(8), None]), // null in non-key field
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect(left, right, on, RightSemi).await?;
        let expected = [
            "+----+----+----+",
            "| a1 | b1 | c2 |",
            "+----+----+----+",
            "| 1  | 4  | 7  |",
            "| 2  | 5  | 8  |",
            "| 3  | 6  |    |",
            "+----+----+----+",
        ];
        // The output order is important as SMJ preserves sortedness
        assert_batches_eq!(expected, &batches);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_semi_with_nulls_with_options() -> Result<()> {
        let left = build_table_i32_nullable(
            ("a1", &vec![Some(3), Some(2), Some(1), Some(0), Some(2)]),
            ("b1", &vec![None, Some(5), Some(4), None, Some(5)]),
            ("c2", &vec![Some(90), Some(80), Some(70), Some(60), None]),
        );
        let right = build_table_i32_nullable(
            ("a1", &vec![Some(3), Some(2), Some(2), Some(1)]),
            ("b1", &vec![None, Some(5), Some(5), Some(4)]), // null in key field
            ("c2", &vec![Some(9), None, Some(8), Some(7)]), // null in non-key field
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) = join_collect_with_options(
            left,
            right,
            on,
            RightSemi,
            vec![
                SortOptions {
                    descending: true,
                    nulls_first: false,
                };
                2
            ],
            NullEquality::NullEqualsNull,
        )
        .await?;

        let expected = [
            "+----+----+----+",
            "| a1 | b1 | c2 |",
            "+----+----+----+",
            "| 3  |    | 9  |",
            "| 2  | 5  |    |",
            "| 2  | 5  | 8  |",
            "| 1  | 4  | 7  |",
            "+----+----+----+",
        ];
        // The output order is important as SMJ preserves sortedness
        assert_batches_eq!(expected, &batches);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_semi_output_two_batches() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 6]),
            ("c1", &vec![70, 80, 90, 100]),
        );
        let right = build_table(
            ("a1", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 7]),
            ("c2", &vec![7, 8, 8, 9]),
        );
        let on = vec![
            (
                Arc::new(Column::new_with_schema("a1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("a1", &right.schema())?) as _,
            ),
            (
                Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
                Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
            ),
        ];

        let (_, batches) =
            join_collect_batch_size_equals_two(left, right, on, RightSemi).await?;
        let expected = [
            "+----+----+----+",
            "| a1 | b1 | c2 |",
            "+----+----+----+",
            "| 1  | 4  | 7  |",
            "| 2  | 5  | 8  |",
            "| 2  | 5  | 8  |",
            "+----+----+----+",
        ];
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].num_rows(), 2);
        assert_eq!(batches[1].num_rows(), 1);
        assert_batches_eq!(expected, &batches);
        Ok(())
    }

    #[tokio::test]
    async fn join_left_mark() -> Result<()> {
        let left = build_table(
            ("a1", &vec![1, 2, 2, 3]),
            ("b1", &vec![4, 5, 5, 7]), // 7 does not exist on the right
            ("c1", &vec![7, 8, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![10, 20, 30, 40]),
            ("b1", &vec![4, 4, 5, 6]), // 5 is double on the right
            ("c2", &vec![60, 70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, LeftMark).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+-------+
            | a1 | b1 | c1 | mark  |
            +----+----+----+-------+
            | 1  | 4  | 7  | true  |
            | 2  | 5  | 8  | true  |
            | 2  | 5  | 8  | true  |
            | 3  | 7  | 9  | false |
            +----+----+----+-------+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_with_duplicated_column_names() -> Result<()> {
        let left = build_table(
            ("a", &vec![1, 2, 3]),
            ("b", &vec![4, 5, 7]),
            ("c", &vec![7, 8, 9]),
        );
        let right = build_table(
            ("a", &vec![10, 20, 30]),
            ("b", &vec![1, 2, 7]),
            ("c", &vec![70, 80, 90]),
        );
        let on = vec![(
            // join on a=b so there are duplicate column names on unjoined columns
            Arc::new(Column::new_with_schema("a", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Inner).await?;
        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +---+---+---+----+---+----+
            | a | b | c | a  | b | c  |
            +---+---+---+----+---+----+
            | 1 | 4 | 7 | 10 | 1 | 70 |
            | 2 | 5 | 8 | 20 | 2 | 80 |
            +---+---+---+----+---+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_date32() -> Result<()> {
        let left = build_date_table(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![19107, 19108, 19108]), // this has a repetition
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_date_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![19107, 19108, 19109]),
            ("c2", &vec![70, 80, 90]),
        );

        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Inner).await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +------------+------------+------------+------------+------------+------------+
            | a1         | b1         | c1         | a2         | b1         | c2         |
            +------------+------------+------------+------------+------------+------------+
            | 1970-01-02 | 2022-04-25 | 1970-01-08 | 1970-01-11 | 2022-04-25 | 1970-03-12 |
            | 1970-01-03 | 2022-04-26 | 1970-01-09 | 1970-01-21 | 2022-04-26 | 1970-03-22 |
            | 1970-01-04 | 2022-04-26 | 1970-01-10 | 1970-01-21 | 2022-04-26 | 1970-03-22 |
            +------------+------------+------------+------------+------------+------------+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_date64() -> Result<()> {
        let left = build_date64_table(
            ("a1", &vec![1, 2, 3]),
            ("b1", &vec![1650703441000, 1650903441000, 1650903441000]), // this has a repetition
            ("c1", &vec![7, 8, 9]),
        );
        let right = build_date64_table(
            ("a2", &vec![10, 20, 30]),
            ("b1", &vec![1650703441000, 1650503441000, 1650903441000]),
            ("c2", &vec![70, 80, 90]),
        );

        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b1", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Inner).await?;

        // The output order is important as SMJ preserves sortedness
        assert_snapshot!(batches_to_string(&batches), @r#"
            +-------------------------+---------------------+-------------------------+-------------------------+---------------------+-------------------------+
            | a1                      | b1                  | c1                      | a2                      | b1                  | c2                      |
            +-------------------------+---------------------+-------------------------+-------------------------+---------------------+-------------------------+
            | 1970-01-01T00:00:00.001 | 2022-04-23T08:44:01 | 1970-01-01T00:00:00.007 | 1970-01-01T00:00:00.010 | 2022-04-23T08:44:01 | 1970-01-01T00:00:00.070 |
            | 1970-01-01T00:00:00.002 | 2022-04-25T16:17:21 | 1970-01-01T00:00:00.008 | 1970-01-01T00:00:00.030 | 2022-04-25T16:17:21 | 1970-01-01T00:00:00.090 |
            | 1970-01-01T00:00:00.003 | 2022-04-25T16:17:21 | 1970-01-01T00:00:00.009 | 1970-01-01T00:00:00.030 | 2022-04-25T16:17:21 | 1970-01-01T00:00:00.090 |
            +-------------------------+---------------------+-------------------------+-------------------------+---------------------+-------------------------+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_left_sort_order() -> Result<()> {
        let left = build_table(
            ("a1", &vec![0, 1, 2, 3, 4, 5]),
            ("b1", &vec![3, 4, 5, 6, 6, 7]),
            ("c1", &vec![4, 5, 6, 7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![0, 10, 20, 30, 40]),
            ("b2", &vec![2, 4, 6, 6, 8]),
            ("c2", &vec![50, 60, 70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Left).await?;
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b2 | c2 |
            +----+----+----+----+----+----+
            | 0  | 3  | 4  |    |    |    |
            | 1  | 4  | 5  | 10 | 4  | 60 |
            | 2  | 5  | 6  |    |    |    |
            | 3  | 6  | 7  | 20 | 6  | 70 |
            | 3  | 6  | 7  | 30 | 6  | 80 |
            | 4  | 6  | 8  | 20 | 6  | 70 |
            | 4  | 6  | 8  | 30 | 6  | 80 |
            | 5  | 7  | 9  |    |    |    |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_sort_order() -> Result<()> {
        let left = build_table(
            ("a1", &vec![0, 1, 2, 3]),
            ("b1", &vec![3, 4, 5, 7]),
            ("c1", &vec![6, 7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![0, 10, 20, 30]),
            ("b2", &vec![2, 4, 5, 6]),
            ("c2", &vec![60, 70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Right).await?;
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b2 | c2 |
            +----+----+----+----+----+----+
            |    |    |    | 0  | 2  | 60 |
            | 1  | 4  | 7  | 10 | 4  | 70 |
            | 2  | 5  | 8  | 20 | 5  | 80 |
            |    |    |    | 30 | 6  | 90 |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_left_multiple_batches() -> Result<()> {
        let left_batch_1 = build_table_i32(
            ("a1", &vec![0, 1, 2]),
            ("b1", &vec![3, 4, 5]),
            ("c1", &vec![4, 5, 6]),
        );
        let left_batch_2 = build_table_i32(
            ("a1", &vec![3, 4, 5, 6]),
            ("b1", &vec![6, 6, 7, 9]),
            ("c1", &vec![7, 8, 9, 9]),
        );
        let right_batch_1 = build_table_i32(
            ("a2", &vec![0, 10, 20]),
            ("b2", &vec![2, 4, 6]),
            ("c2", &vec![50, 60, 70]),
        );
        let right_batch_2 = build_table_i32(
            ("a2", &vec![30, 40]),
            ("b2", &vec![6, 8]),
            ("c2", &vec![80, 90]),
        );
        let left = build_table_from_batches(vec![left_batch_1, left_batch_2]);
        let right = build_table_from_batches(vec![right_batch_1, right_batch_2]);
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Left).await?;
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b2 | c2 |
            +----+----+----+----+----+----+
            | 0  | 3  | 4  |    |    |    |
            | 1  | 4  | 5  | 10 | 4  | 60 |
            | 2  | 5  | 6  |    |    |    |
            | 3  | 6  | 7  | 20 | 6  | 70 |
            | 3  | 6  | 7  | 30 | 6  | 80 |
            | 4  | 6  | 8  | 20 | 6  | 70 |
            | 4  | 6  | 8  | 30 | 6  | 80 |
            | 5  | 7  | 9  |    |    |    |
            | 6  | 9  | 9  |    |    |    |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_right_multiple_batches() -> Result<()> {
        let right_batch_1 = build_table_i32(
            ("a2", &vec![0, 1, 2]),
            ("b2", &vec![3, 4, 5]),
            ("c2", &vec![4, 5, 6]),
        );
        let right_batch_2 = build_table_i32(
            ("a2", &vec![3, 4, 5, 6]),
            ("b2", &vec![6, 6, 7, 9]),
            ("c2", &vec![7, 8, 9, 9]),
        );
        let left_batch_1 = build_table_i32(
            ("a1", &vec![0, 10, 20]),
            ("b1", &vec![2, 4, 6]),
            ("c1", &vec![50, 60, 70]),
        );
        let left_batch_2 = build_table_i32(
            ("a1", &vec![30, 40]),
            ("b1", &vec![6, 8]),
            ("c1", &vec![80, 90]),
        );
        let left = build_table_from_batches(vec![left_batch_1, left_batch_2]);
        let right = build_table_from_batches(vec![right_batch_1, right_batch_2]);
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Right).await?;
        assert_snapshot!(batches_to_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b2 | c2 |
            +----+----+----+----+----+----+
            |    |    |    | 0  | 3  | 4  |
            | 10 | 4  | 60 | 1  | 4  | 5  |
            |    |    |    | 2  | 5  | 6  |
            | 20 | 6  | 70 | 3  | 6  | 7  |
            | 30 | 6  | 80 | 3  | 6  | 7  |
            | 20 | 6  | 70 | 4  | 6  | 8  |
            | 30 | 6  | 80 | 4  | 6  | 8  |
            |    |    |    | 5  | 7  | 9  |
            |    |    |    | 6  | 9  | 9  |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn join_full_multiple_batches() -> Result<()> {
        let left_batch_1 = build_table_i32(
            ("a1", &vec![0, 1, 2]),
            ("b1", &vec![3, 4, 5]),
            ("c1", &vec![4, 5, 6]),
        );
        let left_batch_2 = build_table_i32(
            ("a1", &vec![3, 4, 5, 6]),
            ("b1", &vec![6, 6, 7, 9]),
            ("c1", &vec![7, 8, 9, 9]),
        );
        let right_batch_1 = build_table_i32(
            ("a2", &vec![0, 10, 20]),
            ("b2", &vec![2, 4, 6]),
            ("c2", &vec![50, 60, 70]),
        );
        let right_batch_2 = build_table_i32(
            ("a2", &vec![30, 40]),
            ("b2", &vec![6, 8]),
            ("c2", &vec![80, 90]),
        );
        let left = build_table_from_batches(vec![left_batch_1, left_batch_2]);
        let right = build_table_from_batches(vec![right_batch_1, right_batch_2]);
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];

        let (_, batches) = join_collect(left, right, on, Full).await?;
        assert_snapshot!(batches_to_sort_string(&batches), @r#"
            +----+----+----+----+----+----+
            | a1 | b1 | c1 | a2 | b2 | c2 |
            +----+----+----+----+----+----+
            |    |    |    | 0  | 2  | 50 |
            |    |    |    | 40 | 8  | 90 |
            | 0  | 3  | 4  |    |    |    |
            | 1  | 4  | 5  | 10 | 4  | 60 |
            | 2  | 5  | 6  |    |    |    |
            | 3  | 6  | 7  | 20 | 6  | 70 |
            | 3  | 6  | 7  | 30 | 6  | 80 |
            | 4  | 6  | 8  | 20 | 6  | 70 |
            | 4  | 6  | 8  | 30 | 6  | 80 |
            | 5  | 7  | 9  |    |    |    |
            | 6  | 9  | 9  |    |    |    |
            +----+----+----+----+----+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn overallocation_single_batch_no_spill() -> Result<()> {
        let left = build_table(
            ("a1", &vec![0, 1, 2, 3, 4, 5]),
            ("b1", &vec![1, 2, 3, 4, 5, 6]),
            ("c1", &vec![4, 5, 6, 7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![0, 10, 20, 30, 40]),
            ("b2", &vec![1, 3, 4, 6, 8]),
            ("c2", &vec![50, 60, 70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];
        let sort_options = vec![SortOptions::default(); on.len()];

        let join_types = vec![
            Inner, Left, Right, RightSemi, Full, LeftSemi, LeftAnti, LeftMark,
        ];

        // Disable DiskManager to prevent spilling
        let runtime = RuntimeEnvBuilder::new()
            .with_memory_limit(100, 1.0)
            .with_disk_manager_builder(
                DiskManagerBuilder::default().with_mode(DiskManagerMode::Disabled),
            )
            .build_arc()?;
        let session_config = SessionConfig::default().with_batch_size(50);

        for join_type in join_types {
            let task_ctx = TaskContext::default()
                .with_session_config(session_config.clone())
                .with_runtime(Arc::clone(&runtime));
            let task_ctx = Arc::new(task_ctx);

            let join = join_with_options(
                Arc::clone(&left),
                Arc::clone(&right),
                on.clone(),
                join_type,
                sort_options.clone(),
                NullEquality::NullEqualsNothing,
            )?;

            let stream = join.execute(0, task_ctx)?;
            let err = common::collect(stream).await.unwrap_err();

            assert_contains!(err.to_string(), "Failed to allocate additional");
            assert_contains!(err.to_string(), "SMJStream[0]");
            assert_contains!(err.to_string(), "Disk spilling disabled");
            assert!(join.metrics().is_some());
            assert_eq!(join.metrics().unwrap().spill_count(), Some(0));
            assert_eq!(join.metrics().unwrap().spilled_bytes(), Some(0));
            assert_eq!(join.metrics().unwrap().spilled_rows(), Some(0));
        }

        Ok(())
    }

    #[tokio::test]
    async fn overallocation_multi_batch_no_spill() -> Result<()> {
        let left_batch_1 = build_table_i32(
            ("a1", &vec![0, 1]),
            ("b1", &vec![1, 1]),
            ("c1", &vec![4, 5]),
        );
        let left_batch_2 = build_table_i32(
            ("a1", &vec![2, 3]),
            ("b1", &vec![1, 1]),
            ("c1", &vec![6, 7]),
        );
        let left_batch_3 = build_table_i32(
            ("a1", &vec![4, 5]),
            ("b1", &vec![1, 1]),
            ("c1", &vec![8, 9]),
        );
        let right_batch_1 = build_table_i32(
            ("a2", &vec![0, 10]),
            ("b2", &vec![1, 1]),
            ("c2", &vec![50, 60]),
        );
        let right_batch_2 = build_table_i32(
            ("a2", &vec![20, 30]),
            ("b2", &vec![1, 1]),
            ("c2", &vec![70, 80]),
        );
        let right_batch_3 =
            build_table_i32(("a2", &vec![40]), ("b2", &vec![1]), ("c2", &vec![90]));
        let left =
            build_table_from_batches(vec![left_batch_1, left_batch_2, left_batch_3]);
        let right =
            build_table_from_batches(vec![right_batch_1, right_batch_2, right_batch_3]);
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];
        let sort_options = vec![SortOptions::default(); on.len()];

        let join_types = vec![
            Inner, Left, Right, RightSemi, Full, LeftSemi, LeftAnti, LeftMark,
        ];

        // Disable DiskManager to prevent spilling
        let runtime = RuntimeEnvBuilder::new()
            .with_memory_limit(100, 1.0)
            .with_disk_manager_builder(
                DiskManagerBuilder::default().with_mode(DiskManagerMode::Disabled),
            )
            .build_arc()?;
        let session_config = SessionConfig::default().with_batch_size(50);

        for join_type in join_types {
            let task_ctx = TaskContext::default()
                .with_session_config(session_config.clone())
                .with_runtime(Arc::clone(&runtime));
            let task_ctx = Arc::new(task_ctx);
            let join = join_with_options(
                Arc::clone(&left),
                Arc::clone(&right),
                on.clone(),
                join_type,
                sort_options.clone(),
                NullEquality::NullEqualsNothing,
            )?;

            let stream = join.execute(0, task_ctx)?;
            let err = common::collect(stream).await.unwrap_err();

            assert_contains!(err.to_string(), "Failed to allocate additional");
            assert_contains!(err.to_string(), "SMJStream[0]");
            assert_contains!(err.to_string(), "Disk spilling disabled");
            assert!(join.metrics().is_some());
            assert_eq!(join.metrics().unwrap().spill_count(), Some(0));
            assert_eq!(join.metrics().unwrap().spilled_bytes(), Some(0));
            assert_eq!(join.metrics().unwrap().spilled_rows(), Some(0));
        }

        Ok(())
    }

    #[tokio::test]
    async fn overallocation_single_batch_spill() -> Result<()> {
        let left = build_table(
            ("a1", &vec![0, 1, 2, 3, 4, 5]),
            ("b1", &vec![1, 2, 3, 4, 5, 6]),
            ("c1", &vec![4, 5, 6, 7, 8, 9]),
        );
        let right = build_table(
            ("a2", &vec![0, 10, 20, 30, 40]),
            ("b2", &vec![1, 3, 4, 6, 8]),
            ("c2", &vec![50, 60, 70, 80, 90]),
        );
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];
        let sort_options = vec![SortOptions::default(); on.len()];

        let join_types = [
            Inner, Left, Right, RightSemi, Full, LeftSemi, LeftAnti, LeftMark,
        ];

        // Enable DiskManager to allow spilling
        let runtime = RuntimeEnvBuilder::new()
            .with_memory_limit(100, 1.0)
            .with_disk_manager_builder(
                DiskManagerBuilder::default().with_mode(DiskManagerMode::OsTmpDirectory),
            )
            .build_arc()?;

        for batch_size in [1, 50] {
            let session_config = SessionConfig::default().with_batch_size(batch_size);

            for join_type in &join_types {
                let task_ctx = TaskContext::default()
                    .with_session_config(session_config.clone())
                    .with_runtime(Arc::clone(&runtime));
                let task_ctx = Arc::new(task_ctx);

                let join = join_with_options(
                    Arc::clone(&left),
                    Arc::clone(&right),
                    on.clone(),
                    *join_type,
                    sort_options.clone(),
                    NullEquality::NullEqualsNothing,
                )?;

                let stream = join.execute(0, task_ctx)?;
                let spilled_join_result = common::collect(stream).await.unwrap();

                assert!(join.metrics().is_some());
                assert!(join.metrics().unwrap().spill_count().unwrap() > 0);
                assert!(join.metrics().unwrap().spilled_bytes().unwrap() > 0);
                assert!(join.metrics().unwrap().spilled_rows().unwrap() > 0);

                // Run the test with no spill configuration as
                let task_ctx_no_spill =
                    TaskContext::default().with_session_config(session_config.clone());
                let task_ctx_no_spill = Arc::new(task_ctx_no_spill);

                let join = join_with_options(
                    Arc::clone(&left),
                    Arc::clone(&right),
                    on.clone(),
                    *join_type,
                    sort_options.clone(),
                    NullEquality::NullEqualsNothing,
                )?;
                let stream = join.execute(0, task_ctx_no_spill)?;
                let no_spilled_join_result = common::collect(stream).await.unwrap();

                assert!(join.metrics().is_some());
                assert_eq!(join.metrics().unwrap().spill_count(), Some(0));
                assert_eq!(join.metrics().unwrap().spilled_bytes(), Some(0));
                assert_eq!(join.metrics().unwrap().spilled_rows(), Some(0));
                // Compare spilled and non spilled data to check spill logic doesn't corrupt the data
                assert_eq!(spilled_join_result, no_spilled_join_result);
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn overallocation_multi_batch_spill() -> Result<()> {
        let left_batch_1 = build_table_i32(
            ("a1", &vec![0, 1]),
            ("b1", &vec![1, 1]),
            ("c1", &vec![4, 5]),
        );
        let left_batch_2 = build_table_i32(
            ("a1", &vec![2, 3]),
            ("b1", &vec![1, 1]),
            ("c1", &vec![6, 7]),
        );
        let left_batch_3 = build_table_i32(
            ("a1", &vec![4, 5]),
            ("b1", &vec![1, 1]),
            ("c1", &vec![8, 9]),
        );
        let right_batch_1 = build_table_i32(
            ("a2", &vec![0, 10]),
            ("b2", &vec![1, 1]),
            ("c2", &vec![50, 60]),
        );
        let right_batch_2 = build_table_i32(
            ("a2", &vec![20, 30]),
            ("b2", &vec![1, 1]),
            ("c2", &vec![70, 80]),
        );
        let right_batch_3 =
            build_table_i32(("a2", &vec![40]), ("b2", &vec![1]), ("c2", &vec![90]));
        let left =
            build_table_from_batches(vec![left_batch_1, left_batch_2, left_batch_3]);
        let right =
            build_table_from_batches(vec![right_batch_1, right_batch_2, right_batch_3]);
        let on = vec![(
            Arc::new(Column::new_with_schema("b1", &left.schema())?) as _,
            Arc::new(Column::new_with_schema("b2", &right.schema())?) as _,
        )];
        let sort_options = vec![SortOptions::default(); on.len()];

        let join_types = [
            Inner, Left, Right, RightSemi, Full, LeftSemi, LeftAnti, LeftMark,
        ];

        // Enable DiskManager to allow spilling
        let runtime = RuntimeEnvBuilder::new()
            .with_memory_limit(500, 1.0)
            .with_disk_manager_builder(
                DiskManagerBuilder::default().with_mode(DiskManagerMode::OsTmpDirectory),
            )
            .build_arc()?;

        for batch_size in [1, 50] {
            let session_config = SessionConfig::default().with_batch_size(batch_size);

            for join_type in &join_types {
                let task_ctx = TaskContext::default()
                    .with_session_config(session_config.clone())
                    .with_runtime(Arc::clone(&runtime));
                let task_ctx = Arc::new(task_ctx);
                let join = join_with_options(
                    Arc::clone(&left),
                    Arc::clone(&right),
                    on.clone(),
                    *join_type,
                    sort_options.clone(),
                    NullEquality::NullEqualsNothing,
                )?;

                let stream = join.execute(0, task_ctx)?;
                let spilled_join_result = common::collect(stream).await.unwrap();
                assert!(join.metrics().is_some());
                assert!(join.metrics().unwrap().spill_count().unwrap() > 0);
                assert!(join.metrics().unwrap().spilled_bytes().unwrap() > 0);
                assert!(join.metrics().unwrap().spilled_rows().unwrap() > 0);

                // Run the test with no spill configuration as
                let task_ctx_no_spill =
                    TaskContext::default().with_session_config(session_config.clone());
                let task_ctx_no_spill = Arc::new(task_ctx_no_spill);

                let join = join_with_options(
                    Arc::clone(&left),
                    Arc::clone(&right),
                    on.clone(),
                    *join_type,
                    sort_options.clone(),
                    NullEquality::NullEqualsNothing,
                )?;
                let stream = join.execute(0, task_ctx_no_spill)?;
                let no_spilled_join_result = common::collect(stream).await.unwrap();

                assert!(join.metrics().is_some());
                assert_eq!(join.metrics().unwrap().spill_count(), Some(0));
                assert_eq!(join.metrics().unwrap().spilled_bytes(), Some(0));
                assert_eq!(join.metrics().unwrap().spilled_rows(), Some(0));
                // Compare spilled and non spilled data to check spill logic doesn't corrupt the data
                assert_eq!(spilled_join_result, no_spilled_join_result);
            }
        }

        Ok(())
    }

    fn build_joined_record_batches() -> Result<JoinedRecordBatches> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
            Field::new("x", DataType::Int32, true),
            Field::new("y", DataType::Int32, true),
        ]));

        let mut batches = JoinedRecordBatches {
            batches: vec![],
            filter_mask: BooleanBuilder::new(),
            row_indices: UInt64Builder::new(),
            batch_ids: vec![],
        };

        // Insert already prejoined non-filtered rows
        batches.batches.push(RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(vec![1, 1])),
                Arc::new(Int32Array::from(vec![10, 10])),
                Arc::new(Int32Array::from(vec![1, 1])),
                Arc::new(Int32Array::from(vec![11, 9])),
            ],
        )?);

        batches.batches.push(RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![11])),
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![12])),
            ],
        )?);

        batches.batches.push(RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(vec![1, 1])),
                Arc::new(Int32Array::from(vec![12, 12])),
                Arc::new(Int32Array::from(vec![1, 1])),
                Arc::new(Int32Array::from(vec![11, 13])),
            ],
        )?);

        batches.batches.push(RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![13])),
                Arc::new(Int32Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![12])),
            ],
        )?);

        batches.batches.push(RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int32Array::from(vec![1, 1])),
                Arc::new(Int32Array::from(vec![14, 14])),
                Arc::new(Int32Array::from(vec![1, 1])),
                Arc::new(Int32Array::from(vec![12, 11])),
            ],
        )?);

        let streamed_indices = vec![0, 0];
        batches.batch_ids.extend(vec![0; streamed_indices.len()]);
        batches
            .row_indices
            .extend(&UInt64Array::from(streamed_indices));

        let streamed_indices = vec![1];
        batches.batch_ids.extend(vec![0; streamed_indices.len()]);
        batches
            .row_indices
            .extend(&UInt64Array::from(streamed_indices));

        let streamed_indices = vec![0, 0];
        batches.batch_ids.extend(vec![1; streamed_indices.len()]);
        batches
            .row_indices
            .extend(&UInt64Array::from(streamed_indices));

        let streamed_indices = vec![0];
        batches.batch_ids.extend(vec![2; streamed_indices.len()]);
        batches
            .row_indices
            .extend(&UInt64Array::from(streamed_indices));

        let streamed_indices = vec![0, 0];
        batches.batch_ids.extend(vec![3; streamed_indices.len()]);
        batches
            .row_indices
            .extend(&UInt64Array::from(streamed_indices));

        batches
            .filter_mask
            .extend(&BooleanArray::from(vec![true, false]));
        batches.filter_mask.extend(&BooleanArray::from(vec![true]));
        batches
            .filter_mask
            .extend(&BooleanArray::from(vec![false, true]));
        batches.filter_mask.extend(&BooleanArray::from(vec![false]));
        batches
            .filter_mask
            .extend(&BooleanArray::from(vec![false, false]));

        Ok(batches)
    }

    #[tokio::test]
    async fn test_left_outer_join_filtered_mask() -> Result<()> {
        let mut joined_batches = build_joined_record_batches()?;
        let schema = joined_batches.batches.first().unwrap().schema();

        let output = concat_batches(&schema, &joined_batches.batches)?;
        let out_mask = joined_batches.filter_mask.finish();
        let out_indices = joined_batches.row_indices.finish();

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0]),
                &[0usize],
                &BooleanArray::from(vec![true]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                true, false, false, false, false, false, false, false
            ])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0]),
                &[0usize],
                &BooleanArray::from(vec![false]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                false, false, false, false, false, false, false, false
            ])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0, 0]),
                &[0usize; 2],
                &BooleanArray::from(vec![true, true]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                true, true, false, false, false, false, false, false
            ])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0, 0, 0]),
                &[0usize; 3],
                &BooleanArray::from(vec![true, true, true]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![true, true, true, false, false, false, false, false])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0, 0, 0]),
                &[0usize; 3],
                &BooleanArray::from(vec![true, false, true]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                Some(true),
                None,
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false)
            ])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0, 0, 0]),
                &[0usize; 3],
                &BooleanArray::from(vec![false, false, true]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                None,
                None,
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false)
            ])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0, 0, 0]),
                &[0usize; 3],
                &BooleanArray::from(vec![false, true, true]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                None,
                Some(true),
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false)
            ])
        );

        assert_eq!(
            get_corrected_filter_mask(
                Left,
                &UInt64Array::from(vec![0, 0, 0]),
                &[0usize; 3],
                &BooleanArray::from(vec![false, false, false]),
                output.num_rows()
            )
            .unwrap(),
            BooleanArray::from(vec![
                None,
                None,
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(false)
            ])
        );

        let corrected_mask = get_corrected_filter_mask(
            Left,
            &out_indices,
            &joined_batches.batch_ids,
            &out_mask,
            output.num_rows(),
        )
        .unwrap();

        assert_eq!(
            corrected_mask,
            BooleanArray::from(vec![
                Some(true),
                None,
                Some(true),
                None,
                Some(true),
                Some(false),
                None,
                Some(false)
            ])
        );

        let filtered_rb = filter_record_batch(&output, &corrected_mask)?;

        assert_snapshot!(batches_to_string(&[filtered_rb]), @r#"
                +---+----+---+----+
                | a | b  | x | y  |
                +---+----+---+----+
                | 1 | 10 | 1 | 11 |
                | 1 | 11 | 1 | 12 |
                | 1 | 12 | 1 | 13 |
                +---+----+---+----+
            "#);

        // output null rows

        let null_mask = arrow::compute::not(&corrected_mask)?;
        assert_eq!(
            null_mask,
            BooleanArray::from(vec![
                Some(false),
                None,
                Some(false),
                None,
                Some(false),
                Some(true),
                None,
                Some(true)
            ])
        );

        let null_joined_batch = filter_record_batch(&output, &null_mask)?;

        assert_snapshot!(batches_to_string(&[null_joined_batch]), @r#"
                +---+----+---+----+
                | a | b  | x | y  |
                +---+----+---+----+
                | 1 | 13 | 1 | 12 |
                | 1 | 14 | 1 | 11 |
                +---+----+---+----+
            "#);
        Ok(())
    }

    #[tokio::test]
    async fn test_semi_join_filtered_mask() -> Result<()> {
        for join_type in [LeftSemi, RightSemi] {
            let mut joined_batches = build_joined_record_batches()?;
            let schema = joined_batches.batches.first().unwrap().schema();

            let output = concat_batches(&schema, &joined_batches.batches)?;
            let out_mask = joined_batches.filter_mask.finish();
            let out_indices = joined_batches.row_indices.finish();

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0]),
                    &[0usize],
                    &BooleanArray::from(vec![true]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![true])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0]),
                    &[0usize],
                    &BooleanArray::from(vec![false]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0]),
                    &[0usize; 2],
                    &BooleanArray::from(vec![true, true]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![Some(true), None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![true, true, true]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![Some(true), None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![true, false, true]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![Some(true), None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![false, false, true]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, Some(true),])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![false, true, true]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![None, Some(true), None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![false, false, false]),
                    output.num_rows()
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, None])
            );

            let corrected_mask = get_corrected_filter_mask(
                join_type,
                &out_indices,
                &joined_batches.batch_ids,
                &out_mask,
                output.num_rows(),
            )
            .unwrap();

            assert_eq!(
                corrected_mask,
                BooleanArray::from(vec![
                    Some(true),
                    None,
                    Some(true),
                    None,
                    Some(true),
                    None,
                    None,
                    None
                ])
            );

            let filtered_rb = filter_record_batch(&output, &corrected_mask)?;

            assert_batches_eq!(
                &[
                    "+---+----+---+----+",
                    "| a | b  | x | y  |",
                    "+---+----+---+----+",
                    "| 1 | 10 | 1 | 11 |",
                    "| 1 | 11 | 1 | 12 |",
                    "| 1 | 12 | 1 | 13 |",
                    "+---+----+---+----+",
                ],
                &[filtered_rb]
            );

            // output null rows
            let null_mask = arrow::compute::not(&corrected_mask)?;
            assert_eq!(
                null_mask,
                BooleanArray::from(vec![
                    Some(false),
                    None,
                    Some(false),
                    None,
                    Some(false),
                    None,
                    None,
                    None
                ])
            );

            let null_joined_batch = filter_record_batch(&output, &null_mask)?;

            assert_batches_eq!(
                &[
                    "+---+---+---+---+",
                    "| a | b | x | y |",
                    "+---+---+---+---+",
                    "+---+---+---+---+",
                ],
                &[null_joined_batch]
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_anti_join_filtered_mask() -> Result<()> {
        for join_type in [LeftAnti, RightAnti] {
            let mut joined_batches = build_joined_record_batches()?;
            let schema = joined_batches.batches.first().unwrap().schema();

            let output = concat_batches(&schema, &joined_batches.batches)?;
            let out_mask = joined_batches.filter_mask.finish();
            let out_indices = joined_batches.row_indices.finish();

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0]),
                    &[0usize],
                    &BooleanArray::from(vec![true]),
                    1
                )
                .unwrap(),
                BooleanArray::from(vec![None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0]),
                    &[0usize],
                    &BooleanArray::from(vec![false]),
                    1
                )
                .unwrap(),
                BooleanArray::from(vec![Some(true)])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0]),
                    &[0usize; 2],
                    &BooleanArray::from(vec![true, true]),
                    2
                )
                .unwrap(),
                BooleanArray::from(vec![None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![true, true, true]),
                    3
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![true, false, true]),
                    3
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![false, false, true]),
                    3
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![false, true, true]),
                    3
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, None])
            );

            assert_eq!(
                get_corrected_filter_mask(
                    join_type,
                    &UInt64Array::from(vec![0, 0, 0]),
                    &[0usize; 3],
                    &BooleanArray::from(vec![false, false, false]),
                    3
                )
                .unwrap(),
                BooleanArray::from(vec![None, None, Some(true)])
            );

            let corrected_mask = get_corrected_filter_mask(
                join_type,
                &out_indices,
                &joined_batches.batch_ids,
                &out_mask,
                output.num_rows(),
            )
            .unwrap();

            assert_eq!(
                corrected_mask,
                BooleanArray::from(vec![
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(true),
                    None,
                    Some(true)
                ])
            );

            let filtered_rb = filter_record_batch(&output, &corrected_mask)?;

            allow_duplicates! {
                assert_snapshot!(batches_to_string(&[filtered_rb]), @r#"
                    +---+----+---+----+
                    | a | b  | x | y  |
                    +---+----+---+----+
                    | 1 | 13 | 1 | 12 |
                    | 1 | 14 | 1 | 11 |
                    +---+----+---+----+
            "#);
            }

            // output null rows
            let null_mask = arrow::compute::not(&corrected_mask)?;
            assert_eq!(
                null_mask,
                BooleanArray::from(vec![
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(false),
                    None,
                    Some(false),
                ])
            );

            let null_joined_batch = filter_record_batch(&output, &null_mask)?;

            allow_duplicates! {
                assert_snapshot!(batches_to_string(&[null_joined_batch]), @r#"
                        +---+---+---+---+
                        | a | b | x | y |
                        +---+---+---+---+
                        +---+---+---+---+
                "#);
            }
        }
        Ok(())
    }

    /// Returns the column names on the schema
    fn columns(schema: &Schema) -> Vec<String> {
        schema.fields().iter().map(|f| f.name().clone()).collect()
    }
}
