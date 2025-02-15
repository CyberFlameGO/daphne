// Copyright (c) 2022 Cloudflare, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause

//! This crate implements the core protocol logic for the Distributed Aggregation Protocol
//! ([DAP](https://datatracker.ietf.org/doc/draft-ietf-ppm-dap/)) standard under development in the
//! PPM working group of the IETF. See [`VdafConfig`] for a listing of supported
//! [VDAFs](https://github.com/cfrg/draft-irtf-cfrg-vdaf).
//!
//! Daphne implements draft-ietf-ppm-dap-02 and draft-ietf-ppm-dap-03.
//!
//! Daphne does not provide the complete, end-to-end functionality of any party in the protocol.
//! Instead, it defines traits for the functionalities that a concrete instantiation of the
//! protocol is required to implement. For example, the `daphne_worker` crate implements a backend
//! for the DAP Leader and Helper. See the [`crate::roles`](roles) module for details.
//!
//! Daphne is not yet feature complete. Known issues include:
//!
//! * The collect sub-protocol has not yet been fully implemented. In particular, Daphne Aggreators
//! do not check properly if batch intervals overlap across collect requests. Note that this
//! feature is privacy-critical and implementation is planned. See
//! <https://github.com/cloudflare/daphne/issues/45> for details.
//!
//! * Daphne is not compatible with DAP tasks whose maximum batch lifetime is longer than one.
//!
//! * Aborts are not handled precisely as specified. In particular, some fields in the "Problem
//! Details" document are omitted.
//!
//! * Daphne does not implement a complete DAP Client or Collector. However, methods are provided
//! on [`VdafConfig`](crate::VdafConfig) for producing reports and consuming aggregate results.
//!
//! * Daphne does not yet support deletion of collection jobs:
//!
//!     > The leader MUST remove a collect job's results when the collector sends an HTTP DELETE
//!     > request to the collect job  URI. The leader responds with HTTP status 204 No Content for
//!     > requests to a collect job URI whose results have been removed.

use crate::{
    hpke::HpkeReceiverConfig,
    messages::{
        BatchSelector, CollectResp, Duration, HpkeConfig, Id, Interval, PartialBatchSelector,
        ReportId, ReportMetadata, Time, TransitionFailure,
    },
    vdaf::{
        prio2::prio2_decode_prepare_state,
        prio3::{prio3_append_prepare_state, prio3_decode_prepare_state},
        VdafAggregateShare, VdafError, VdafMessage, VdafState, VdafVerifyKey,
    },
};
use messages::HpkeKemId;
use prio::{
    codec::{CodecError, Decode, Encode},
    vdaf::Aggregatable as AggregatableTrait,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};
use taskprov::TaskprovVersion;
use url::Url;

/// DAP errors.
#[derive(Debug, thiserror::Error)]
pub enum DapError {
    /// Fatal error. If this triggers an abort, then treat this as an internal error.
    #[error("fatal error: {0}")]
    Fatal(String),

    /// Error triggered by peer, resulting in an abort.
    #[error("abort: {0}")]
    Abort(DapAbort),

    /// Transition failure. This error blocks processing of a paritcular report and may, under
    /// certain conditions, trigger an abort.
    #[error("transition error: {0}")]
    Transition(TransitionFailure),
}

impl DapError {
    /// Create a fatal error.
    pub fn fatal(s: &'static str) -> Self {
        Self::Fatal(s.into())
    }
}

impl From<prometheus::Error> for DapError {
    fn from(e: prometheus::Error) -> Self {
        Self::Fatal(format!("prometheus: {e}"))
    }
}

impl From<serde_json::Error> for DapError {
    fn from(e: serde_json::Error) -> Self {
        Self::Fatal(format!("serde_json: {e}"))
    }
}

impl From<hex::FromHexError> for DapError {
    fn from(e: hex::FromHexError) -> Self {
        Self::Fatal(format!("from hex: {e}"))
    }
}

impl From<CodecError> for DapError {
    fn from(e: CodecError) -> Self {
        Self::Fatal(format!("codec: {e}"))
    }
}

impl From<VdafError> for DapError {
    fn from(e: VdafError) -> Self {
        match e {
            VdafError::Codec(..) | VdafError::Vdaf(..) => {
                Self::Transition(TransitionFailure::VdafPrepError)
            }
        }
    }
}

/// DAP aborts.
#[derive(Debug, thiserror::Error)]
pub enum DapAbort {
    /// Bad request. Sent in response to an HTTP request that couldn't be handled preoprly.
    //
    // TODO spec: Decide whether to specify this.
    #[error("badRequest")]
    BadRequest(String),

    /// Invalid batch. Sent in response to a CollectReq or AggregateShareReq.
    #[error("batchInvalid")]
    BatchInvalid,

    /// Batch mismatch. Sent in response to an AggregateShareReq.
    #[error("batchMismatch")]
    BatchMismatch,

    /// Batch overlap. Sent in response to an CollectReq for which the Leader detects the same
    /// Collector requesting an aggregate share which it has collected in the past.
    #[error("batchOverlap")]
    BatchOverlap,

    /// Internal error.
    #[error("{0}")]
    Internal(#[source] Box<dyn std::error::Error + 'static + Send + Sync>),

    /// Invalid DAP version. Sent in response to requests for an unsupported (or unknown) DAP version.
    #[error("invalidProtocolVersion")]
    InvalidProtocolVersion,

    /// Invalid batch size (either too small or too large). Sent in response to a CollectReq or
    /// AggregateShareReq.
    #[error("invalidBatchSize")]
    InvalidBatchSize,

    /// Invalid DAP task.  Sent when a server opts out of a taskprov task configuration.
    #[error("invalidTask")]
    InvalidTask,

    /// Request with missing task ID.
    #[error("missingTaskID")]
    MissingTaskId,

    /// Query mismatch. Sent in response to a CollectReq or AggregateShareReq.
    #[error("queryMismatch")]
    QueryMismatch,

    /// Replayed report. Sent in response to an upload request containing a Report that has been replayed.
    //
    // TODO spec: Define this error type.
    #[error("replayedReport")]
    ReplayedReport,

    /// Report too late. Sent in response to an upload request for a task that is known to have
    /// expired.
    #[error("reportTooLate")]
    ReportTooLate,

    /// Stale report. Sent in response to an upload request containing a report pertaining to a
    /// batch that has already been collected.
    #[error("staleReport")]
    StaleReport,

    /// Unauthorized HTTP request.
    #[error("unauthorizedRequest")]
    UnauthorizedRequest,

    /// Unrecognized aggregation job. Sent in response to an AggregateContinueReq for which the
    /// Helper does not recognize the indicated aggregation job.
    #[error("unrecognizedAggregationJob")]
    UnrecognizedAggregationJob,

    /// Unrecognized HPKE config. Sent in response to an upload request for which the leader share
    /// is encrypted using an unrecognized HPKE configuration.
    //
    // TODO spec: Rename this error type.
    #[error("unrecognizedHpkeConfig")]
    UnrecognizedHpkeConfig,

    /// Unrecognized message. Sent in response to a malformed or unexpected message.
    #[error("unrecognizedMessage")]
    UnrecognizedMessage,

    /// Unrecognized DAP task. Sent in response to a request indicating an unrecognized task ID.
    #[error("unrecognizedTask")]
    UnrecognizedTask,
}

impl DapAbort {
    /// Construct a problem details JSON object for this abort. `url` is the URL to which the
    /// request was targeted and `task_id` is the associated TaskID.
    pub fn to_problem_details(&self) -> ProblemDetails {
        let (typ, detail) = match self {
            Self::BatchInvalid
            | Self::BatchMismatch
            | Self::BatchOverlap
            | Self::InvalidBatchSize
            | Self::InvalidProtocolVersion
            | Self::InvalidTask
            | Self::QueryMismatch
            | Self::MissingTaskId
            | Self::ReplayedReport
            | Self::ReportTooLate
            | Self::StaleReport
            | Self::UnauthorizedRequest
            | Self::UnrecognizedAggregationJob
            | Self::UnrecognizedHpkeConfig
            | Self::UnrecognizedMessage
            | Self::UnrecognizedTask => (self.to_string(), None),
            Self::BadRequest(s) => ("badRequest".to_string(), Some(s.clone())),
            Self::Internal(e) => ("internalError".to_string(), Some(e.to_string())),
        };

        ProblemDetails {
            typ: format!("urn:ietf:params:ppm:dap:error:{typ}"),
            taskid: None,   // TODO interop: Implement as specified.
            instance: None, // TODO interop: Implement as specified.
            detail,
        }
    }
}

impl From<DapError> for DapAbort {
    fn from(e: DapError) -> Self {
        match e {
            e @ DapError::Fatal(..) => Self::Internal(Box::new(e)),
            DapError::Abort(e) => e,
            DapError::Transition(t) => Self::from(t),
        }
    }
}

impl From<CodecError> for DapAbort {
    fn from(_e: CodecError) -> Self {
        Self::UnrecognizedMessage
    }
}

impl From<TransitionFailure> for DapAbort {
    fn from(failure_reason: TransitionFailure) -> Self {
        match failure_reason {
            TransitionFailure::BatchCollected => Self::StaleReport,
            TransitionFailure::ReportReplayed => Self::ReplayedReport,
            _ => DapError::fatal("unhandled transition failure").into(),
        }
    }
}

/// A problem details document compatible with RFC 7807.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub typ: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) taskid: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instance: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// DAP version used for a task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum DapVersion {
    #[serde(rename = "v02")]
    Draft02,

    #[serde(rename = "v03")]
    Draft03,

    #[serde(other)]
    #[serde(rename = "unknown_version")]
    Unknown,
}

impl From<&str> for DapVersion {
    fn from(version: &str) -> Self {
        match version {
            "v02" => DapVersion::Draft02,
            "v03" => DapVersion::Draft03,
            _ => DapVersion::Unknown,
        }
    }
}

impl AsRef<str> for DapVersion {
    fn as_ref(&self) -> &str {
        match self {
            DapVersion::Draft02 => "v02",
            DapVersion::Draft03 => "v03",
            _ => panic!("tried to construct string from unknown DAP version"),
        }
    }
}

impl std::fmt::Display for DapVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

/// Global DAP parameters common across tasks.
#[derive(Clone, Deserialize, Serialize)]
pub struct DapGlobalConfig {
    /// The report storage epoch duration. This value is used to control the period of time for
    /// which an Aggregator guarantees storage of reports and/or report metadata.
    ///
    /// A report will be accepted if its timestamp is no more than the specified number of seconds
    /// before the current time.
    pub report_storage_epoch_duration: Duration,

    /// The report storage maximum future time skew. Reports with timestamps greater than the
    /// current time plus this value will be rejected.
    pub report_storage_max_future_time_skew: Duration,

    /// Maximum interval duration permitted in CollectReq.
    /// Prevents Collectors from requesting wide range or reports.
    pub max_batch_duration: Duration,

    /// Lower bound of an acceptable batch interval for collect requests.
    /// Batch intervals cannot start more than `min_batch_interval_start`
    /// apart from the current batch interval.
    //
    // TODO(cjpatton) Rename this and clarify semantics.
    pub min_batch_interval_start: Duration,

    /// Upper bound of an acceptable batch interval for collect requests.
    /// Batch intervals cannot end more than `max_batch_interval_end`
    /// apart from the current batch interval.
    //
    // TODO(cjpatton) Rename this and clarify semantics.
    pub max_batch_interval_end: Duration,

    /// HPKE KEM types that are supported. Used when generating HPKE
    /// receiver config.
    pub supported_hpke_kems: Vec<HpkeKemId>,

    /// Is the taskprov extension allowed?
    pub allow_taskprov: bool,

    /// Which taskprov draft should be used?
    pub taskprov_version: TaskprovVersion,
}

impl DapGlobalConfig {
    /// Generate a list of HPKE receiver configurations, one for each element of supported KEM
    /// algorithm. `first_config_id` is used as the first config ID; subsequent IDs are chosen by
    /// incrementing `first_config_id`.
    pub fn gen_hpke_receiver_config_list(
        &self,
        first_config_id: u8,
    ) -> impl Iterator<Item = Result<HpkeReceiverConfig, DapError>> {
        assert!(self.supported_hpke_kems.len() <= 256);
        let kem_ids = self.supported_hpke_kems.clone();
        kem_ids.into_iter().enumerate().map(move |(i, kem_id)| {
            let (config_id, _overflowed) = first_config_id.overflowing_add(i as u8);
            HpkeReceiverConfig::gen(config_id, kem_id)
        })
    }
}

/// DAP Query configuration.
//
// TODO(cjpatton) Once we implement maximum batch lifetime, put the parameter here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DapQueryConfig {
    /// The "time-interval" query type. Each report in the batch must fall into the time interval
    /// specified by the query.
    TimeInterval,

    /// The "fixed-size" query type. The Leader partitions the reports into arbitary batches of
    /// roughly the same size.
    FixedSize { max_batch_size: u64 },
}

impl DapQueryConfig {
    pub(crate) fn is_valid_part_batch_sel(&self, part_batch_sel: &PartialBatchSelector) -> bool {
        matches!(
            (&self, part_batch_sel),
            (
                Self::TimeInterval { .. },
                PartialBatchSelector::TimeInterval
            ) | (
                Self::FixedSize { .. },
                PartialBatchSelector::FixedSizeByBatchId { .. }
            )
        )
    }

    pub(crate) fn is_valid_batch_sel(&self, batch_sel: &BatchSelector) -> bool {
        matches!(
            (&self, batch_sel),
            (
                Self::TimeInterval { .. },
                BatchSelector::TimeInterval { .. }
            ) | (
                Self::FixedSize { .. },
                BatchSelector::FixedSizeByBatchId { .. }
            )
        )
    }
}

/// A batch bucket.
///
/// A bucket is the smallest, disjoint set of reports that can be queried: For time-interval
/// queries, the bucket to which a report is assigned is determined by truncating its timestamp by
/// the task's `time_precision` parameter; for fixed-size queries, the span consists of a single
/// bucket, which is the batch determined by the batch ID (i.e., the partial batch selector).
#[derive(Clone, Eq, Hash, PartialEq)]
pub enum DapBatchBucket<'a> {
    FixedSize { batch_id: &'a Id },
    TimeInterval { batch_window: Time },
}

/// Per-task DAP parameters.
#[derive(Clone, Deserialize, Serialize)]
pub struct DapTaskConfig {
    /// The protocol version (i.e., which draft).
    pub version: DapVersion,

    /// Base URL of the Leader.
    pub leader_url: Url,

    /// Base URL of the Helper.
    pub helper_url: Url,

    /// Report granularity. Used by the Client to truncate the timestamp and by the Aggregators to
    /// constrain the batch interval of time=interval queries.
    pub time_precision: Duration,

    /// The time at which the task expires.
    pub expiration: Time,

    /// The smallest batch permitted for this task.
    pub min_batch_size: u64,

    /// The query configuration for this task.
    pub query: DapQueryConfig,

    /// The VDAF configuration for this task.
    pub vdaf: VdafConfig,

    /// VDAF verification key shared by the Aggregators. Used to aggregate reports.
    pub vdaf_verify_key: VdafVerifyKey,

    /// The Collector's HPKE configuration for this task.
    pub collector_hpke_config: HpkeConfig,
}

impl DapTaskConfig {
    /// Convert at timestamp `now` into an [`Interval`] that contains it. The timestamp is the
    /// numbre of seconds since the beginning of UNIX time.
    #[cfg(test)]
    pub fn query_for_current_batch_window(&self, now: u64) -> crate::messages::Query {
        let start = now - (now % self.time_precision);
        crate::messages::Query::TimeInterval {
            batch_interval: crate::messages::Interval {
                start,
                duration: self.time_precision,
            },
        }
    }

    pub(crate) fn truncate_time(&self, time: Time) -> Time {
        time - (time % self.time_precision)
    }

    /// Compute the "batch span" of a set of output shares and, for each buckent in the span,
    /// aggregate the output shares into an aggregate share.
    pub fn batch_span_for_out_shares<'a>(
        &self,
        part_batch_sel: &'a PartialBatchSelector,
        out_shares: Vec<DapOutputShare>,
    ) -> Result<HashMap<DapBatchBucket<'a>, DapAggregateShare>, DapError> {
        if !self.query.is_valid_part_batch_sel(part_batch_sel) {
            return Err(DapError::fatal(
                "partial batch selector not compatible with task",
            ));
        }

        let mut span: HashMap<DapBatchBucket<'a>, DapAggregateShare> = HashMap::new();
        for out_share in out_shares.into_iter() {
            let bucket = match part_batch_sel {
                PartialBatchSelector::TimeInterval => DapBatchBucket::TimeInterval {
                    batch_window: self.truncate_time(out_share.time),
                },
                PartialBatchSelector::FixedSizeByBatchId { batch_id } => {
                    DapBatchBucket::FixedSize { batch_id }
                }
            };

            let agg_share = span.entry(bucket).or_default();
            agg_share.merge(DapAggregateShare {
                report_count: 1,
                checksum: out_share.checksum,
                data: Some(out_share.data),
            })?;
        }

        Ok(span)
    }

    /// Return the batch span determined by the given batch selector. The span includes every
    /// bucket to which a report that matches the batch selector could be assigned.
    pub fn batch_span_for_sel<'a>(
        &self,
        batch_sel: &'a BatchSelector,
    ) -> Result<HashSet<DapBatchBucket<'a>>, DapError> {
        if !self.query.is_valid_batch_sel(batch_sel) {
            return Err(DapError::fatal("batch selector not compatible with task"));
        }

        match batch_sel {
            BatchSelector::TimeInterval {
                batch_interval: Interval { start, duration },
            } => {
                let windows = duration / self.time_precision;
                let mut span = HashSet::with_capacity(windows as usize);
                for i in 0..windows {
                    span.insert(DapBatchBucket::TimeInterval {
                        batch_window: start + i * self.time_precision,
                    });
                }
                Ok(span)
            }
            BatchSelector::FixedSizeByBatchId { batch_id } => {
                Ok(HashSet::from([DapBatchBucket::FixedSize { batch_id }]))
            }
        }
    }

    /// Return the batch span of a set of reports with the given metadata.
    pub fn batch_span_for_meta<'a>(
        &self,
        part_batch_sel: &'a PartialBatchSelector,
        report_meta: impl Iterator<Item = &'a ReportMetadata>,
    ) -> Result<HashMap<DapBatchBucket<'a>, Vec<&'a ReportMetadata>>, DapError> {
        if !self.query.is_valid_part_batch_sel(part_batch_sel) {
            return Err(DapError::fatal(
                "partial batch selector not compatible with task",
            ));
        }

        let mut span: HashMap<_, Vec<_>> = HashMap::new();
        for metadata in report_meta {
            let bucket = match part_batch_sel {
                PartialBatchSelector::TimeInterval => DapBatchBucket::TimeInterval {
                    batch_window: self.truncate_time(metadata.time),
                },
                PartialBatchSelector::FixedSizeByBatchId { batch_id } => {
                    DapBatchBucket::FixedSize { batch_id }
                }
            };

            let report_ids = span.entry(bucket).or_default();
            report_ids.push(metadata);
        }

        Ok(span)
    }

    /// Check if the batch is ready to aggregate based on the report count. Returns an error if the
    /// report count is too large.
    pub(crate) fn is_report_count_compatible(&self, report_count: u64) -> Result<bool, DapAbort> {
        match self.query {
            DapQueryConfig::TimeInterval => (),
            DapQueryConfig::FixedSize { max_batch_size } => {
                if report_count > max_batch_size {
                    return Err(DapAbort::InvalidBatchSize);
                }
            }
        };

        Ok(report_count >= self.min_batch_size)
    }
}

impl AsRef<DapTaskConfig> for DapTaskConfig {
    fn as_ref(&self) -> &Self {
        self
    }
}

/// A measurement from which a Client generates a report.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DapMeasurement {
    U64(u64),
    U32Vec(Vec<u32>),
}

/// The aggregate result computed by the Collector.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DapAggregateResult {
    U32Vec(Vec<u32>),
    U64(u64),
    U128(u128),
    U128Vec(Vec<u128>),
}

/// The Leader's state after sending an AggregateInitReq.
#[derive(Debug)]
pub struct DapLeaderState {
    pub(crate) seq: Vec<(VdafState, VdafMessage, Time, ReportId)>,
}

/// The Leader's state after sending an AggregateContReq.
#[derive(Debug)]
pub struct DapLeaderUncommitted {
    pub(crate) seq: Vec<(DapOutputShare, ReportId)>,
}

/// The Helper's state during the aggregation flow.
#[derive(Clone, Debug, PartialEq)]
pub struct DapHelperState {
    pub(crate) part_batch_sel: PartialBatchSelector,
    pub(crate) seq: Vec<(VdafState, Time, ReportId)>,
}

impl DapHelperState {
    /// Encode the Helper state as a byte string.
    ///
    /// This method is used by the Helper in order to offload its state to the Leader. For
    /// example, it might encrypt the output and add the ciphertext to an outgoing aggregate
    /// response.
    ///
    /// Note that the encoding format is not specified by the DAP standard.
    pub fn get_encoded(&self, vdaf_config: &VdafConfig) -> Result<Vec<u8>, DapError> {
        let mut bytes = vec![];
        self.part_batch_sel.encode(&mut bytes);
        for (state, time, report_id) in self.seq.iter() {
            match (vdaf_config, state) {
                (VdafConfig::Prio3(prio3_config), _) => {
                    prio3_append_prepare_state(&mut bytes, prio3_config, state)?;
                }
                (VdafConfig::Prio2 { .. }, VdafState::Prio2(state)) => {
                    state.encode(&mut bytes);
                }
                _ => return Err(DapError::fatal("VDAF config and prep state mismatch")),
            }
            time.encode(&mut bytes);
            report_id.encode(&mut bytes);
        }
        Ok(bytes)
    }

    /// Decode the Helper state from a byte string.
    pub fn get_decoded(vdaf_config: &VdafConfig, data: &[u8]) -> Result<Self, DapError> {
        let mut r = std::io::Cursor::new(data);
        let part_batch_sel = PartialBatchSelector::decode(&mut r)?;
        let mut seq = vec![];
        while (r.position() as usize) < data.len() {
            let state = match vdaf_config {
                VdafConfig::Prio3(ref prio3_config) => {
                    prio3_decode_prepare_state(prio3_config, 1, &mut r)?
                }
                VdafConfig::Prio2 { dimension } => {
                    prio2_decode_prepare_state(*dimension, 1, &mut r)?
                }
            };
            let time = Time::decode(&mut r)?;
            let report_id = ReportId::decode(&mut r)?;
            seq.push((state, time, report_id))
        }

        Ok(DapHelperState {
            part_batch_sel,
            seq,
        })
    }
}

#[derive(Debug)]
/// An ouptut share produced by an Aggregator for a single report.
pub struct DapOutputShare {
    pub(crate) time: u64, // Value from the report
    pub(crate) checksum: [u8; 32],
    pub(crate) data: VdafAggregateShare,
}

/// An aggregate share computed by combining a set of output shares.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DapAggregateShare {
    pub(crate) report_count: u64,
    pub(crate) checksum: [u8; 32],
    pub(crate) data: Option<VdafAggregateShare>,
}

impl DapAggregateShare {
    /// Merge two aggregate shares. This method is run by an Aggregator.
    //
    // TODO Add unit tests.
    pub fn merge(&mut self, other: DapAggregateShare) -> Result<(), DapError> {
        // Update the aggregate share data.
        match (self.data.as_mut(), other.data) {
            (_, None) => (),
            (None, Some(data)) => {
                self.data = Some(data);
            }
            (Some(VdafAggregateShare::Field64(left)), Some(VdafAggregateShare::Field64(right))) => {
                left.merge(&right)
                    .map_err(|e| DapError::Fatal(e.to_string()))?;
            }
            (
                Some(VdafAggregateShare::Field128(left)),
                Some(VdafAggregateShare::Field128(right)),
            ) => {
                left.merge(&right)
                    .map_err(|e| DapError::Fatal(e.to_string()))?;
            }
            (
                Some(VdafAggregateShare::FieldPrio2(left)),
                Some(VdafAggregateShare::FieldPrio2(right)),
            ) => {
                left.merge(&right)
                    .map_err(|e| DapError::Fatal(e.to_string()))?;
            }

            _ => return Err(DapError::fatal("invalid aggregate share merge")),
        };

        self.report_count += other.report_count;
        for (x, y) in self.checksum.iter_mut().zip(other.checksum) {
            *x ^= y;
        }
        Ok(())
    }

    /// Return `true` if the aggregate share contains no reports.
    pub fn empty(&self) -> bool {
        self.report_count == 0
    }

    /// Set the aggregate share to zero.
    pub fn reset(&mut self) {
        self.report_count = 0;
        self.checksum = [0; 32];
        self.data = None;
    }

    #[cfg(test)]
    pub(crate) fn try_from_out_shares(
        out_shares: impl IntoIterator<Item = DapOutputShare>,
    ) -> Result<Self, DapError> {
        let mut agg_share = Self::default();
        for out_share in out_shares.into_iter() {
            agg_share.merge(DapAggregateShare {
                report_count: 1,
                checksum: out_share.checksum,
                data: Some(out_share.data),
            })?;
        }
        Ok(agg_share)
    }
}

/// Leader state transition during the aggregation flow.
#[derive(Debug)]
pub enum DapLeaderTransition<M: Debug> {
    /// The Leader has produced the next outbound message and its state has been updated.
    Continue(DapLeaderState, M),

    /// The leader has computed output shares, but is waiting on an AggregateResp from the hepler
    /// before committing them.
    Uncommitted(DapLeaderUncommitted, M),

    /// The Leader has completed the aggregation flow without computing an aggregate share.
    Skip,
}

/// Helper state transition during the aggregation flow.
#[derive(Debug)]
pub enum DapHelperTransition<M: Debug> {
    /// The Helper has produced the next outbound message and its state has been updated.
    Continue(DapHelperState, M),

    /// The Helper has produced the last outbound message and has computed a sequence of output
    /// shares.
    //
    // TODO Instead of merging all output shares into a single aggregate share, return a collection
    // of aggregat shares, each corresponding to a different batch interval.
    Finish(Vec<DapOutputShare>, M),
}

/// Specificaiton of a concrete VDAF.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VdafConfig {
    Prio3(Prio3Config),
    Prio2 { dimension: u32 },
}

impl std::str::FromStr for VdafConfig {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

/// Supported data types for prio3.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Prio3Config {
    /// A 64-bit counter. The aggregate is the sum of the measurements, where each measurement is
    /// equal to `0` or `1`.
    Count,

    /// A histogram for estimating the distribution of 64-bit, unsigned integers using pre-defined
    /// bucket boundaries.
    Histogram { buckets: Vec<u64> },

    /// The sum of 64-bit, unsigned integers. Each measurement is an integer in range `[0,
    /// 2^bits)`.
    Sum { bits: u32 },
}

/// DAP request.
#[derive(Debug)]
pub struct DapRequest<S> {
    pub version: DapVersion,
    pub media_type: Option<&'static str>,
    pub task_id: Option<Id>,
    pub payload: Vec<u8>,
    pub url: Url,
    pub sender_auth: Option<S>,
}

impl<S> DapRequest<S> {
    pub(crate) fn task_id(&self) -> Result<&Id, DapAbort> {
        if let Some(ref id) = self.task_id {
            Ok(id)
        } else {
            // Handle missing task ID as decoding failure.
            Err(DapAbort::UnrecognizedMessage)
        }
    }
}

/// DAP response.
#[derive(Debug)]
pub struct DapResponse {
    pub media_type: Option<&'static str>,
    pub payload: Vec<u8>,
}

/// Status of a collect job.
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DapCollectJob {
    Done(CollectResp),
    Pending,
    Unknown,
}

/// Telemetry information for the leader's processing loop.
//
// TODO This is used for tests. Perhaps Prometheus metrics would be sufficient?
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DapLeaderProcessTelemetry {
    /// Number of collect requests completed.
    pub reports_collected: u64,

    /// The number of reports aggregated.
    pub reports_aggregated: u64,

    /// The number of reports processed.
    pub reports_processed: u64,
}

pub mod auth;
pub mod constants;
pub mod hpke;
#[cfg(test)]
mod hpke_test;
pub mod messages;
pub mod metrics;
pub mod roles;
#[cfg(test)]
mod roles_test;
pub mod taskprov;
#[cfg(test)]
mod taskprov_test;
pub mod testing;
pub mod vdaf;
