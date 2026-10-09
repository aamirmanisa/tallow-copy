use serde::{Deserialize, Serialize};

pub const EVENT_JOB_STARTED: &str = "job:started";
pub const EVENT_JOB_PROGRESS: &str = "job:progress";
pub const EVENT_JOB_FILE_STARTED: &str = "job:file-started";
pub const EVENT_JOB_FILE_FINISHED: &str = "job:file-finished";
pub const EVENT_JOB_ERROR: &str = "job:error";
pub const EVENT_JOB_PAUSED: &str = "job:paused";
pub const EVENT_JOB_COMPLETED: &str = "job:completed";
pub const EVENT_JOB_CANCELLED: &str = "job:cancelled";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    Copy,
    Mirror,
    Sync,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    None,
    Size,
    SampledHash,
    FullHash,
    ReadAfterWrite,
    Manifest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataMode {
    DataOnly,
    Timestamps,
    Attributes,
    Security,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendMode {
    Auto,
    ThreadPool,
    Iocp,
    DirectIo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletePolicy {
    Never,
    Review,
    Allow,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Copy,
    Update,
    Delete,
    Skip,
    Verify,
    Conflict,
    ErrorRisk,
    MetadataOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Planning,
    Planned,
    Copying,
    Verifying,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Planned,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    PermissionDenied,
    LockedFile,
    PathTooLong,
    DestinationFull,
    NetworkInterrupted,
    VerificationMismatch,
    MetadataFailed,
    SymlinkPolicyBlocked,
    DeleteRequiresReview,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Retry,
    Skip,
    Reveal,
    ChangeSetting,
    ReviewDetails,
    FreeSpace,
    Reconnect,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFilters {
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub max_depth: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    pub source: String,
    pub target: String,
    pub mode: TransferMode,
    pub filters: JobFilters,
    pub verify_mode: VerifyMode,
    pub metadata_mode: MetadataMode,
    pub backend_mode: BackendMode,
    /// 0 means derive the count from the source tree and the path class (the engine's
    /// `recommended_threads_for_tree_path`). No constant is right: eight threads help a small-file tree and
    /// hurt large files and CIFS.
    #[serde(default)]
    pub thread_count: u16,
    pub buffer_size_bytes: u64,
    pub delete_policy: DeletePolicy,
    /// Continue each interrupted file from its deterministic partial sibling. Copy mode only - a mirror or
    /// sync run has no partial to continue from.
    #[serde(default)]
    pub resume: bool,
    /// Stop the transfer at the first error instead of recording it and continuing.
    #[serde(default)]
    pub stop_on_error: bool,
    /// The manifest a `manifest` verification job is checked against, as the operator gave it. That mode
    /// is refused without one: a manifest check with no manifest verifies nothing.
    #[serde(default)]
    pub manifest_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedOperation {
    pub kind: OperationKind,
    pub source: Option<String>,
    pub target: Option<String>,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTotals {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
    pub copies: u64,
    pub updates: u64,
    pub deletes: u64,
    pub verifies: u64,
    pub skips: u64,
    pub conflicts: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskSummary {
    pub destructive: bool,
    pub requires_review: bool,
    pub delete_count: u64,
    pub conflict_count: u64,
    pub locked_file_count: u64,
    pub estimated_error_count: u64,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobPlan {
    pub plan_id: String,
    pub request: JobRequest,
    /// The worker count this plan will actually use, after derivation. The UI displayed a literal 16
    /// labelled "AUTO"; this is the number the engine resolved.
    pub effective_threads: u16,
    /// Where bundling small files would be faster than copying them one by one.
    pub bundling_hint: Option<String>,
    pub operations: Vec<PlannedOperation>,
    pub totals: PlanTotals,
    pub risk_summary: RiskSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub job_id: String,
    pub plan_id: String,
    pub state: JobState,
    pub plan: JobPlan,
}

/// One finished job, exactly as the app observed it. Written by the UI from live progress - which is
/// where the real numbers are - so a job that failed halfway cannot be filed as a completed one.
/// What an audit of two folders found. Mapped from the engine's `TreeAudit` so the wire format stays
/// the app's, as with the rest of the engine surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeAuditReport {
    pub source: String,
    pub target: String,
    /// The comparison that was actually made: "size", "hash" or "hash-all".
    pub verify: String,
    /// Nothing to copy and nothing to delete. NOT "the trees are identical" - a size comparison cannot
    /// establish that, and saying so would be the quietest possible way for a verification to be wrong.
    pub clean: bool,
    pub differences: u64,
    pub matching_files: u64,
    pub matching_bytes: u64,
    pub differing_files: u64,
    pub differing_bytes: u64,
    pub extra_files: u64,
    pub extra_bytes: u64,
    pub error_files: u64,
    pub differing: Vec<String>,
    pub extra: Vec<String>,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobHistoryEntry {
    pub job_id: String,
    pub plan_id: String,
    pub source: String,
    pub target: String,
    pub mode: TransferMode,
    pub state: JobState,
    pub bytes_copied: u64,
    pub bytes_total: u64,
    pub files_copied: u64,
    pub files_total: u64,
    pub files_skipped: u64,
    pub error_count: u64,
    pub elapsed_seconds: f64,
    pub average_rate_bytes_per_second: f64,
    pub completed_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveFile {
    pub path: String,
    pub bytes_copied: u64,
    pub bytes_total: u64,
    pub operation: OperationKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    pub job_id: String,
    pub plan_id: String,
    pub phase: JobPhase,
    pub backend: BackendMode,
    pub bytes_copied: u64,
    pub bytes_total: u64,
    pub files_copied: u64,
    pub files_total: u64,
    pub files_skipped: u64,
    pub error_count: u64,
    pub rate_bytes_per_second: f64,
    pub eta_seconds: Option<f64>,
    pub active_files: Vec<ActiveFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub job_id: String,
    pub plan_id: String,
    pub source: String,
    pub target: String,
    pub mode: TransferMode,
    pub phase: JobPhase,
    pub verify_mode: VerifyMode,
    pub bytes_copied: u64,
    pub bytes_total: u64,
    pub files_copied: u64,
    pub files_skipped: u64,
    pub error_count: u64,
    pub elapsed_seconds: f64,
    pub average_rate_bytes_per_second: f64,
    pub manifest_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobError {
    pub path: Option<String>,
    pub category: ErrorCategory,
    pub message: String,
    pub retryable: bool,
    pub recommended_action: RecommendedAction,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFileEvent {
    pub job_id: String,
    pub plan_id: String,
    pub file: ActiveFile,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRuntimeErrorEvent {
    pub job_id: String,
    pub plan_id: String,
    pub error: JobError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineCapabilities {
    pub supported_modes: Vec<TransferMode>,
    pub supported_verify_modes: Vec<VerifyMode>,
    pub supported_metadata_modes: Vec<MetadataMode>,
    pub supported_backend_modes: Vec<BackendMode>,
    pub default_backend_mode: BackendMode,
    pub default_thread_count: u16,
    pub max_thread_count: u16,
    pub default_buffer_size_bytes: u64,
    pub max_buffer_size_bytes: u64,
    pub supports_resume: bool,
    pub supports_manifest: bool,
    pub supports_security_metadata: bool,
    pub supports_direct_io: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub default_verify_mode: VerifyMode,
    pub default_metadata_mode: MetadataMode,
    pub default_backend_mode: BackendMode,
    pub default_thread_count: u16,
    pub default_buffer_size_bytes: u64,
    pub delete_policy: DeletePolicy,
    pub allow_mirror_deletes_without_review: bool,
    pub throttle_progress_millis: u16,
    pub preserve_window_state: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsPatch {
    pub default_verify_mode: Option<VerifyMode>,
    pub default_metadata_mode: Option<MetadataMode>,
    pub default_backend_mode: Option<BackendMode>,
    pub default_thread_count: Option<u16>,
    pub default_buffer_size_bytes: Option<u64>,
    pub delete_policy: Option<DeletePolicy>,
    pub allow_mirror_deletes_without_review: Option<bool>,
    pub throttle_progress_millis: Option<u16>,
    pub preserve_window_state: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationIssue {
    pub code: String,
    pub field: Option<String>,
    pub message: String,
    pub destructive: bool,
    pub requires_review: bool,
}

impl ValidationIssue {
    pub fn new(
        code: impl Into<String>,
        field: Option<&str>,
        message: impl Into<String>,
        destructive: bool,
        requires_review: bool,
    ) -> Self {
        Self {
            code: code.into(),
            field: field.map(str::to_string),
            message: message.into(),
            destructive,
            requires_review,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<ValidationIssue>,
}

impl CommandError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            issues: Vec::new(),
        }
    }

    pub fn request_validation_failed(issues: Vec<ValidationIssue>) -> Self {
        Self {
            code: "request_validation_failed".to_string(),
            message: "Transfer request needs attention before a plan can be created".to_string(),
            issues,
        }
    }

    pub fn job_not_found(job_id: &str) -> Self {
        Self::new("job_not_found", format!("Job '{job_id}' was not found"))
    }

    pub fn invalid_job_state(job_id: &str, expected: &str, actual: &JobState) -> Self {
        Self::new(
            "invalid_job_state",
            format!("Job '{job_id}' must be {expected}; current state is {actual:?}"),
        )
    }

    pub fn store_unavailable() -> Self {
        Self::new(
            "store_unavailable",
            "The in-memory job store is unavailable",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_request_serializes_with_camel_case_fields_and_enum_values() {
        let request = JobRequest {
            source: "C:\\Source".to_string(),
            target: "D:\\Target".to_string(),
            mode: TransferMode::Mirror,
            filters: JobFilters {
                include_globs: vec!["**/*.jpg".to_string()],
                exclude_globs: vec!["**/cache/**".to_string()],
                include_hidden: false,
                follow_symlinks: false,
                max_depth: Some(8),
            },
            verify_mode: VerifyMode::SampledHash,
            metadata_mode: MetadataMode::Security,
            backend_mode: BackendMode::DirectIo,
            thread_count: 8,
            buffer_size_bytes: 4_194_304,
            delete_policy: DeletePolicy::Review,
            resume: false,
            stop_on_error: false,
            manifest_path: None,
        };

        let value = serde_json::to_value(request).expect("request should serialize");

        assert_eq!(value["source"], "C:\\Source");
        assert_eq!(value["target"], "D:\\Target");
        assert_eq!(value["mode"], "mirror");
        assert_eq!(value["verifyMode"], "sampled_hash");
        assert_eq!(value["metadataMode"], "security");
        assert_eq!(value["backendMode"], "direct_io");
        assert_eq!(value["threadCount"], 8);
        assert_eq!(value["bufferSizeBytes"], 4_194_304);
        assert_eq!(value["deletePolicy"], "review");
        assert_eq!(value["filters"]["includeGlobs"][0], "**/*.jpg");
        assert_eq!(value["filters"]["excludeGlobs"][0], "**/cache/**");
        assert_eq!(value["filters"]["includeHidden"], false);
        assert_eq!(value["filters"]["followSymlinks"], false);
        assert_eq!(value["filters"]["maxDepth"], 8);
    }

    #[test]
    fn job_plan_serializes_operations_and_risk_summary() {
        let plan = JobPlan {
            plan_id: "plan-1".to_string(),
            request: JobRequest {
                source: "C:\\Source".to_string(),
                target: "D:\\Target".to_string(),
                mode: TransferMode::Sync,
                filters: JobFilters::default(),
                verify_mode: VerifyMode::Manifest,
                metadata_mode: MetadataMode::All,
                backend_mode: BackendMode::ThreadPool,
                thread_count: 4,
                buffer_size_bytes: 1_048_576,
                delete_policy: DeletePolicy::Never,
                resume: false,
                stop_on_error: false,
                manifest_path: None,
            },
            effective_threads: 4,
            bundling_hint: None,
            operations: vec![PlannedOperation {
                kind: OperationKind::Verify,
                source: Some("C:\\Source\\file.bin".to_string()),
                target: Some("D:\\Target\\file.bin".to_string()),
                bytes: 1_024,
                reason: "manifest changed".to_string(),
            }],
            totals: PlanTotals {
                bytes: 1_024,
                files: 1,
                directories: 0,
                copies: 0,
                updates: 0,
                deletes: 0,
                verifies: 1,
                skips: 0,
                conflicts: 0,
            },
            risk_summary: RiskSummary {
                destructive: false,
                requires_review: false,
                delete_count: 0,
                conflict_count: 0,
                locked_file_count: 0,
                estimated_error_count: 0,
                warnings: vec!["verification will read target".to_string()],
            },
        };

        let value = serde_json::to_value(plan).expect("plan should serialize");

        assert_eq!(value["planId"], "plan-1");
        assert_eq!(value["request"]["mode"], "sync");
        assert_eq!(value["request"]["verifyMode"], "manifest");
        assert_eq!(value["request"]["metadataMode"], "all");
        assert_eq!(value["request"]["backendMode"], "thread_pool");
        assert_eq!(value["operations"][0]["kind"], "verify");
        assert_eq!(value["operations"][0]["source"], "C:\\Source\\file.bin");
        assert_eq!(value["operations"][0]["target"], "D:\\Target\\file.bin");
        assert_eq!(value["operations"][0]["bytes"], 1_024);
        assert_eq!(value["riskSummary"]["requiresReview"], false);
        assert_eq!(value["riskSummary"]["deleteCount"], 0);
        assert_eq!(value["totals"]["verifies"], 1);
    }

    #[test]
    fn job_progress_serializes_runtime_progress_fields() {
        let progress = JobProgress {
            job_id: "job-1".to_string(),
            plan_id: "plan-1".to_string(),
            phase: JobPhase::Verifying,
            backend: BackendMode::Iocp,
            bytes_copied: 2_048,
            bytes_total: 4_096,
            files_copied: 2,
            files_total: 4,
            files_skipped: 1,
            error_count: 1,
            rate_bytes_per_second: 512_000.0,
            eta_seconds: Some(12.5),
            active_files: vec![ActiveFile {
                path: "D:\\Target\\file.bin".to_string(),
                bytes_copied: 512,
                bytes_total: 1_024,
                operation: OperationKind::Copy,
            }],
        };

        let value = serde_json::to_value(progress).expect("progress should serialize");

        assert_eq!(value["jobId"], "job-1");
        assert_eq!(value["planId"], "plan-1");
        assert_eq!(value["phase"], "verifying");
        assert_eq!(value["backend"], "iocp");
        assert_eq!(value["bytesCopied"], 2_048);
        assert_eq!(value["bytesTotal"], 4_096);
        assert_eq!(value["filesCopied"], 2);
        assert_eq!(value["filesSkipped"], 1);
        assert_eq!(value["errorCount"], 1);
        assert_eq!(value["rateBytesPerSecond"], 512_000.0);
        assert_eq!(value["etaSeconds"], 12.5);
        assert_eq!(value["activeFiles"][0]["operation"], "copy");
    }
}
