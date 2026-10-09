use crate::jobs::store::{default_engine_capabilities, JobStore};
use crate::jobs::types::{
    ErrorCategory, JobError, JobPhase, JobProgress, JobState, JobSummary, RecommendedAction,
};
// The app<->engine plan translation is pure, so it is compiled in test builds as well and covered
// by the tests at the bottom of this file. Gating it out of tests (as it was) is how a seam that
// crosses two crates ends up with no coverage at all.
use crate::jobs::types::{BackendMode, OperationKind, PlanTotals, PlannedOperation, RiskSummary};
use crate::jobs::types::{
    CommandError, DeletePolicy, EngineCapabilities, JobPlan, JobRecord, JobRequest, MetadataMode,
    TransferMode, TreeAuditReport, VerifyMode,
};
use crate::jobs::types::{
    JobFileEvent, JobRuntimeErrorEvent, EVENT_JOB_CANCELLED, EVENT_JOB_COMPLETED, EVENT_JOB_ERROR,
    EVENT_JOB_FILE_FINISHED, EVENT_JOB_FILE_STARTED, EVENT_JOB_PROGRESS, EVENT_JOB_STARTED,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tallow_copy_engine as native;
use tauri::{Emitter, Manager};

pub type EngineAppHandle = tauri::AppHandle;

/// The execution path's whole view of its host: where progress events go, and how a job's terminal
/// state is recorded.
///
/// The app implements this over its Tauri handle and the store it manages. Tests implement it with
/// a recorder, which is what makes the execution path itself - the sequence and the payloads of the
/// events, not merely the arithmetic behind them - verifiable with no display and no webview. It
/// exists because taking a concrete `AppHandle` is what kept this code out of test builds entirely.
pub trait JobHost: Send + Sync {
    fn emit_json(&self, event: &str, payload: serde_json::Value);
    fn complete(&self, job_id: &str);
    fn fail(&self, job_id: &str);
}

/// Serialise and hand off. A payload that fails to serialise is dropped rather than emitted as a
/// lie; every type used here serialises.
fn emit<T: serde::Serialize>(host: &dyn JobHost, event: &str, payload: T) {
    match serde_json::to_value(payload) {
        Ok(value) => host.emit_json(event, value),
        Err(error) => crate::diagnostics::log_error(format!(
            "job event {event} not emitted: payload did not serialise: {error}"
        )),
    }
}

/// The real host: Tauri's event channel and the `JobStore` registered on the app.
struct TauriJobHost {
    app: tauri::AppHandle,
}

impl JobHost for TauriJobHost {
    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        let _ = self.app.emit(event, payload);
    }

    fn complete(&self, job_id: &str) {
        let _ = self.app.state::<JobStore>().complete_job(job_id);
    }

    fn fail(&self, job_id: &str) {
        let _ = self.app.state::<JobStore>().fail_job(job_id);
    }
}

/// Adapter boundary between Tauri commands and the copy implementation.
///
/// Production builds use `NativeTallowEngine`; tests can inject a lightweight
/// in-memory engine to exercise command and store transitions without touching disk.
pub trait CopyEngine: Send + Sync + 'static {
    fn capabilities(&self) -> EngineCapabilities;
    fn plan(&self, store: &JobStore, request: JobRequest) -> Result<JobPlan, CommandError>;
    fn execute(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError>;
    fn pause(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError>;
    fn resume(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError>;
    fn cancel(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError>;
}

#[derive(Clone, Copy, Debug, Default)]
#[cfg(test)]
pub struct SimulatedEngine;

#[cfg(test)]
impl CopyEngine for SimulatedEngine {
    fn capabilities(&self) -> EngineCapabilities {
        default_engine_capabilities()
    }

    fn plan(&self, store: &JobStore, request: JobRequest) -> Result<JobPlan, CommandError> {
        store.plan_job(request)
    }

    fn execute(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        let record = store.execute_job(job_id)?;
        let _ = app;
        Ok(record)
    }

    fn pause(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        let record = store.pause_job(job_id)?;
        let _ = app;
        Ok(record)
    }

    fn resume(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        let record = store.resume_job(job_id)?;
        let _ = app;
        Ok(record)
    }

    fn cancel(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        let record = store.cancel_job(job_id)?;
        let _ = app;
        Ok(record)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeTallowEngine;

fn native_controls() -> &'static Mutex<HashMap<String, native::CopyControl>> {
    static CONTROLS: OnceLock<Mutex<HashMap<String, native::CopyControl>>> = OnceLock::new();
    CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl CopyEngine for NativeTallowEngine {
    fn capabilities(&self) -> EngineCapabilities {
        let mut capabilities = default_engine_capabilities();
        capabilities.supported_verify_modes = vec![
            VerifyMode::None,
            VerifyMode::Size,
            VerifyMode::SampledHash,
            VerifyMode::FullHash,
            VerifyMode::ReadAfterWrite,
            VerifyMode::Manifest,
        ];
        capabilities.supported_metadata_modes = vec![
            MetadataMode::DataOnly,
            MetadataMode::Timestamps,
            MetadataMode::Attributes,
            MetadataMode::All,
        ];
        capabilities.supported_backend_modes = vec![BackendMode::Auto, BackendMode::ThreadPool];
        capabilities.supports_resume = true;
        capabilities.supports_manifest = true;
        capabilities.supports_security_metadata = false;
        capabilities.supports_direct_io = false;
        capabilities
    }

    fn plan(&self, store: &JobStore, request: JobRequest) -> Result<JobPlan, CommandError> {
        crate::diagnostics::log_info(format!(
            "native plan start source={} target={} mode={:?}",
            request.source, request.target, request.mode
        ));
        store.validate_job_request(&request)?;
        let native_plan = native::plan(native_job_from_request(&request))
            .map_err(|err| command_error_from_copy_error("native_plan_failed", &err))?;
        let (operations, totals, risk_summary) =
            app_plan_details_from_native(&request, &native_plan);

        crate::diagnostics::log_info(format!(
            "native plan complete actions={} files={} bytes={} deletes={} requires_review={}",
            operations.len(),
            totals.files,
            totals.bytes,
            risk_summary.delete_count,
            risk_summary.requires_review
        ));
        store.plan_job_with_details(request, operations, totals, risk_summary)
    }

    fn execute(
        &self,
        app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        let planned = store.get_job(job_id)?;
        if planned.plan.risk_summary.requires_review {
            crate::diagnostics::log_warn(format!(
                "native execute blocked by delete review job_id={job_id}"
            ));
            return Err(CommandError::new(
                "delete_requires_review",
                "This mirror plan includes deletes and must be approved before native execution.",
            ));
        }

        let record = store.execute_job(job_id)?;
        let native_plan = native::plan(native_job_from_request(&record.plan.request))
            .map_err(|err| command_error_from_copy_error("native_plan_failed", &err))?;

        if let Some(app) = app {
            crate::diagnostics::log_info(format!(
                "native execute spawn job_id={} actions={} bytes={} threads={} verify={:?}",
                record.job_id,
                native_plan.actions.len(),
                native_plan.total_bytes,
                native_plan.job.threads,
                native_plan.job.verify
            ));
            let host: Arc<dyn JobHost> = Arc::new(TauriJobHost { app: app.clone() });
            emit(&*host, EVENT_JOB_STARTED, &record);
            let control = native::CopyControl::new();
            if let Ok(mut controls) = native_controls().lock() {
                controls.insert(record.job_id.clone(), control.clone());
            }
            let background_record = record.clone();
            thread::spawn(move || {
                run_native_execution(host, background_record, native_plan, control)
            });
            Ok(planned_with_state(record, JobState::Running))
        } else {
            let report = native::execute(&native_plan);
            if report.errors.is_empty() {
                store.complete_job(job_id)
            } else {
                let _ = store.fail_job(job_id);
                Err(CommandError::new(
                    "native_copy_failed",
                    format!(
                        "Native copy finished with {} error(s).",
                        report.errors.len()
                    ),
                ))
            }
        }
    }

    fn pause(
        &self,
        _app: Option<EngineAppHandle>,
        _store: &JobStore,
        _job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        Err(CommandError::new(
            "unsupported_operation",
            "Native pause is not available until the copy engine has a control token.",
        ))
    }

    fn resume(
        &self,
        _app: Option<EngineAppHandle>,
        _store: &JobStore,
        _job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        Err(CommandError::new(
            "unsupported_operation",
            "Native resume is not available until checkpointed execution is implemented.",
        ))
    }

    fn cancel(
        &self,
        _app: Option<EngineAppHandle>,
        store: &JobStore,
        job_id: &str,
    ) -> Result<JobRecord, CommandError> {
        if let Ok(controls) = native_controls().lock() {
            if let Some(control) = controls.get(job_id) {
                control.cancel();
            }
        }
        store.cancel_job(job_id)
    }
}

/// The worker count a request will actually use. `0` means derive from the source tree and path class -
/// one place decides that, so the job the engine runs and the number the UI shows cannot disagree.
pub(crate) fn effective_thread_count(request: &JobRequest) -> u16 {
    if request.thread_count == 0 {
        native::recommended_threads_for_tree_path(
            std::path::Path::new(&request.source),
            std::path::Path::new(&request.target),
        ) as u16
    } else {
        request.thread_count
    }
}

/// Where the engine would recommend bundling small files instead of copying them one by one. The engine
/// bundles but does not carry the recommendation in the plan, so it is derived from the same bounded probe
/// the CLI uses and surfaced to the operator.
pub(crate) fn bundling_hint(request: &JobRequest) -> Option<String> {
    let small = native::probe_small_files(std::path::Path::new(&request.source));
    if small >= native::BUNDLE_MIN_SMALL_FILES {
        Some(format!(
            "{small} small file(s) in the source tree: bundling them into archives is likely faster than copying one by one"
        ))
    } else {
        None
    }
}

/// Audit the two folders the operator picked, writing nothing anywhere.
///
/// The engine does the walk with `dry_run` set and never calls `execute`, so "read-only" is a property
/// of the code path rather than a promise in a comment. `verify_mode` decides how hard the comparison
/// looks, and it means the same thing here as it does for a copy.
pub(crate) fn audit_trees(
    source: &str,
    target: &str,
    verify_mode: VerifyMode,
) -> Result<TreeAuditReport, CommandError> {
    let (skip, label) = match verify_mode {
        VerifyMode::None | VerifyMode::Size => (native::SkipPolicy::SizeMtime, "size"),
        VerifyMode::SampledHash => (native::SkipPolicy::SizeMtimeHash, "hash"),
        VerifyMode::FullHash | VerifyMode::ReadAfterWrite => (native::SkipPolicy::Hash, "hash-all"),
        // The engine refuses manifest verification instead of quietly behaving like a hash. An audit must
        // not claim a strictness it cannot deliver either, so this is refused rather than downgraded.
        VerifyMode::Manifest => {
            return Err(CommandError::new(
                "verify_mode_unsupported",
                "Manifest verification is not implemented; choose size, hash or full hash",
            ))
        }
    };
    let audit = native::audit_trees(
        std::path::Path::new(source),
        std::path::Path::new(target),
        skip,
        label,
        AUDIT_LIST_LIMIT,
    )
    .map_err(|error| CommandError::new("audit_failed", error.to_string()))?;
    // Read what depends on the whole audit before its fields are moved into the report.
    let clean = audit.is_clean();
    let differences = audit.differences();
    Ok(TreeAuditReport {
        source: audit.source,
        target: audit.target,
        verify: audit.verify,
        clean,
        differences,
        matching_files: audit.matching_files,
        matching_bytes: audit.matching_bytes,
        differing_files: audit.differing_files,
        differing_bytes: audit.differing_bytes,
        extra_files: audit.extra_files,
        extra_bytes: audit.extra_bytes,
        error_files: audit.error_files,
        differing: audit.differing,
        extra: audit.extra,
        problems: audit.problems,
    })
}

/// Bounded so a 100k-file mismatch cannot flood the console; the counts stay complete.
const AUDIT_LIST_LIMIT: usize = 50;

fn native_job_from_request(request: &JobRequest) -> native::CopyJob {
    let mut job = native::CopyJob::copy(&request.source, &request.target);
    // Resume is an engine CopyMode rather than a flag on the job, so it replaces the mode - and only for a
    // plain copy. A mirror or sync run has no partial to continue from, and substituting Resume there would
    // silently change what the mode means.
    if request.resume && request.mode == TransferMode::Copy {
        job.mode = native::CopyMode::Resume;
    } else {
        job.mode = match request.mode {
            TransferMode::Copy => native::CopyMode::Copy,
        TransferMode::Sync => native::CopyMode::Sync,
        TransferMode::Mirror if request.delete_policy == DeletePolicy::Never => {
            native::CopyMode::Sync
        }
        TransferMode::Mirror => native::CopyMode::Mirror,
        };
    }
    job.error_policy = if request.stop_on_error {
        native::ErrorPolicy::Strict
    } else {
        native::ErrorPolicy::BestEffort
    };
    job.manifest = request
        .manifest_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from);
    job.verify = match request.verify_mode {
        VerifyMode::None => native::VerifyPolicy::None,
        VerifyMode::Size => native::VerifyPolicy::Size,
        VerifyMode::SampledHash => native::VerifyPolicy::SampledHash,
        VerifyMode::FullHash => native::VerifyPolicy::FullHash,
        VerifyMode::ReadAfterWrite => native::VerifyPolicy::ReadAfterWrite,
        VerifyMode::Manifest => native::VerifyPolicy::Manifest,
    };
    job.metadata = match request.metadata_mode {
        MetadataMode::DataOnly => native::MetadataPolicy::DataOnly,
        MetadataMode::Timestamps => native::MetadataPolicy::Timestamps,
        MetadataMode::Attributes => native::MetadataPolicy::Attributes,
        MetadataMode::Security => native::MetadataPolicy::Security,
        MetadataMode::All => native::MetadataPolicy::All,
    };
    job.link_policy = if request.filters.follow_symlinks {
        native::LinkPolicy::Follow
    } else {
        native::LinkPolicy::Skip
    };
    job.threads = usize::from(effective_thread_count(request));
    job.buffer_size_bytes = request.buffer_size_bytes.max(4096) as usize;
    job
}

fn app_plan_details_from_native(
    request: &JobRequest,
    native_plan: &native::CopyPlan,
) -> (Vec<PlannedOperation>, PlanTotals, RiskSummary) {
    let mut totals = PlanTotals {
        bytes: native_plan.total_bytes,
        files: native_plan.total_files,
        skips: native_plan.skipped_files,
        deletes: native_plan.delete_files,
        ..PlanTotals::default()
    };

    let operations = native_plan
        .actions
        .iter()
        .map(|action| {
            let kind = match action.kind {
                native::CopyActionKind::Copy if action.reason.contains("changed") => {
                    totals.updates += 1;
                    OperationKind::Update
                }
                native::CopyActionKind::Copy => {
                    totals.copies += 1;
                    OperationKind::Copy
                }
                native::CopyActionKind::Skip => OperationKind::Skip,
                native::CopyActionKind::Verify => {
                    totals.verifies += 1;
                    OperationKind::Verify
                }
                native::CopyActionKind::Delete => OperationKind::Delete,
                native::CopyActionKind::Mkdir => {
                    totals.directories += 1;
                    OperationKind::MetadataOnly
                }
                native::CopyActionKind::Metadata => OperationKind::MetadataOnly,
                native::CopyActionKind::Error => {
                    totals.conflicts += 1;
                    OperationKind::ErrorRisk
                }
            };

            PlannedOperation {
                kind,
                source: action.source.as_ref().map(|path| path_to_string(path)),
                target: Some(path_to_string(&action.destination)),
                bytes: action.bytes,
                reason: action.reason.clone(),
            }
        })
        .collect::<Vec<_>>();

    let mut warnings = Vec::new();
    if !request.filters.include_globs.is_empty()
        || !request.filters.exclude_globs.is_empty()
        || !request.filters.include_hidden
        || request.filters.max_depth.is_some()
    {
        warnings.push(
            "Advanced filters are visible in the UI but are not enforced by the native engine yet."
                .to_string(),
        );
    }
    if request.backend_mode == BackendMode::Iocp || request.backend_mode == BackendMode::DirectIo {
        warnings.push(
            "IOCP and Direct I/O are not implemented yet; native execution uses bounded file I/O."
                .to_string(),
        );
    }
    if request.metadata_mode == MetadataMode::Security {
        warnings.push(
            "Windows security descriptor preservation is not implemented yet; native execution preserves basic metadata only."
                .to_string(),
        );
    }

    let destructive = native_plan.delete_files > 0;
    let requires_review = destructive && request.delete_policy == DeletePolicy::Review;
    let risk_summary = RiskSummary {
        destructive,
        requires_review,
        delete_count: native_plan.delete_files,
        conflict_count: totals.conflicts,
        locked_file_count: 0,
        estimated_error_count: totals.conflicts,
        warnings,
    };

    (operations, totals, risk_summary)
}

fn run_native_execution(
    host: Arc<dyn JobHost>,
    record: JobRecord,
    native_plan: native::CopyPlan,
    control: native::CopyControl,
) {
    crate::diagnostics::log_info(format!(
        "native execution thread start job_id={} plan_id={} actions={} bytes={}",
        record.job_id,
        record.plan_id,
        native_plan.actions.len(),
        native_plan.total_bytes
    ));
    let started_at = Instant::now();
    let mut last_progress_emit = Instant::now();
    let mut last_progress_log = Instant::now();
    let mut active_files = HashMap::<String, crate::jobs::types::ActiveFile>::new();
    let report = native::execute_with_control(&native_plan, &control, |event| match event.kind {
        native::CopyProgressKind::FileStarted => {
            let file = active_file_from_native_event(&event, 0);
            active_files.insert(file.path.clone(), file.clone());
            emit(&*host, EVENT_JOB_FILE_STARTED,
                JobFileEvent {
                    job_id: record.job_id.clone(),
                    plan_id: record.plan_id.clone(),
                    file,
                },
            );
        }
        native::CopyProgressKind::BytesCopied => {
            let file = active_file_from_native_event(&event, event.bytes_done);
            active_files.insert(file.path.clone(), file);
            let should_emit = last_progress_emit.elapsed() >= Duration::from_millis(150)
                || event.aggregate_bytes_done >= event.aggregate_bytes_total;
            if should_emit {
                last_progress_emit = Instant::now();
                let progress = progress_from_native_event(
                    &record,
                    &event,
                    JobPhase::Copying,
                    event.aggregate_bytes_done,
                    0,
                    started_at.elapsed().as_secs_f64(),
                    active_files.values().cloned().collect(),
                );
                emit(&*host, EVENT_JOB_PROGRESS, progress);
            }
            if last_progress_log.elapsed() >= Duration::from_secs(1) {
                last_progress_log = Instant::now();
                crate::diagnostics::log_info(format!(
                    "native progress job_id={} bytes={}/{} files={}/{} active_files={}",
                    record.job_id,
                    event.aggregate_bytes_done,
                    event.aggregate_bytes_total,
                    event.files_done,
                    event.files_total,
                    active_files.len()
                ));
            }
        }
        native::CopyProgressKind::FileFinished => {
            let file = active_file_from_native_event(&event, event.bytes_total);
            emit(&*host, EVENT_JOB_FILE_FINISHED,
                JobFileEvent {
                    job_id: record.job_id.clone(),
                    plan_id: record.plan_id.clone(),
                    file: file.clone(),
                },
            );
            active_files.remove(&file.path);
            let progress = progress_from_native_event(
                &record,
                &event,
                JobPhase::Copying,
                event.aggregate_bytes_done,
                0,
                started_at.elapsed().as_secs_f64(),
                active_files.values().cloned().collect(),
            );
            emit(&*host, EVENT_JOB_PROGRESS, progress);
        }
        native::CopyProgressKind::FileSkipped => {
            let progress = progress_from_native_event(
                &record,
                &event,
                JobPhase::Copying,
                event.aggregate_bytes_done,
                0,
                started_at.elapsed().as_secs_f64(),
                active_files.values().cloned().collect(),
            );
            emit(&*host, EVENT_JOB_PROGRESS, progress);
        }
        native::CopyProgressKind::FileDeleted => {
            let progress = progress_from_native_event(
                &record,
                &event,
                JobPhase::Copying,
                event.aggregate_bytes_done,
                0,
                started_at.elapsed().as_secs_f64(),
                active_files.values().cloned().collect(),
            );
            emit(&*host, EVENT_JOB_PROGRESS, progress);
        }
        native::CopyProgressKind::Error => {
            let file = active_file_from_native_event(&event, event.bytes_done);
            active_files.remove(&file.path);
            if let Some(error) = &event.error {
                crate::diagnostics::log_error(format!(
                    "native event error job_id={} category={:?} path={:?} message={}",
                    record.job_id, error.category, error.path, error.message
                ));
                emit(&*host, EVENT_JOB_ERROR,
                    JobRuntimeErrorEvent {
                        job_id: record.job_id.clone(),
                        plan_id: record.plan_id.clone(),
                        error: job_error_from_copy_error(error),
                    },
                );
            }
        }
    });
    let elapsed_seconds = started_at.elapsed().as_secs_f64().max(0.001);
    if let Ok(mut controls) = native_controls().lock() {
        controls.remove(&record.job_id);
    }

    for error in &report.errors {
        crate::diagnostics::log_error(format!(
            "native report error job_id={} category={:?} path={:?} message={}",
            record.job_id, error.category, error.path, error.message
        ));
        emit(&*host, EVENT_JOB_ERROR,
            JobRuntimeErrorEvent {
                job_id: record.job_id.clone(),
                plan_id: record.plan_id.clone(),
                error: job_error_from_copy_error(error),
            },
        );
    }

    let cancelled = report
        .errors
        .iter()
        .any(|error| matches!(error.category, native::CopyErrorCategory::Cancelled));
    let phase = if cancelled {
        JobPhase::Cancelled
    } else if report.errors.is_empty() {
        host.complete(&record.job_id);
        JobPhase::Completed
    } else {
        host.fail(&record.job_id);
        JobPhase::Failed
    };
    let progress = progress_from_report(&record, &report, phase.clone(), elapsed_seconds);
    emit(&*host, EVENT_JOB_PROGRESS, &progress);

    if phase == JobPhase::Completed {
        crate::diagnostics::log_info(format!(
            "native execution complete job_id={} bytes={} copied={} skipped={} verified={} deleted={} worker_threads={} elapsed={elapsed_seconds:.3}s",
            record.job_id,
            report.bytes_copied,
            report.copied_files,
            report.skipped_files,
            report.verified_files,
            report.deleted_files,
            report.worker_threads_used
        ));
        emit(&*host, EVENT_JOB_COMPLETED,
            summary_from_report(&record, &report, elapsed_seconds),
        );
    } else if phase == JobPhase::Cancelled {
        crate::diagnostics::log_warn(format!(
            "native execution cancelled job_id={}",
            record.job_id
        ));
        emit(&*host, EVENT_JOB_CANCELLED, &progress);
    } else {
        crate::diagnostics::log_error(format!(
            "native execution failed job_id={} errors={} elapsed={elapsed_seconds:.3}s",
            record.job_id,
            report.errors.len()
        ));
    }
}

fn progress_from_native_event(
    record: &JobRecord,
    event: &native::CopyProgressEvent,
    phase: JobPhase,
    bytes_copied: u64,
    error_count: u64,
    elapsed_seconds: f64,
    active_files: Vec<crate::jobs::types::ActiveFile>,
) -> JobProgress {
    let rate_bytes_per_second = bytes_copied as f64 / elapsed_seconds.max(0.001);
    JobProgress {
        job_id: record.job_id.clone(),
        plan_id: record.plan_id.clone(),
        phase,
        backend: record.plan.request.backend_mode.clone(),
        bytes_copied: bytes_copied.min(record.plan.totals.bytes),
        bytes_total: record.plan.totals.bytes,
        files_copied: event.files_done,
        files_total: event.files_total,
        files_skipped: 0,
        error_count,
        rate_bytes_per_second,
        eta_seconds: None,
        active_files,
    }
}

fn active_file_from_native_event(
    event: &native::CopyProgressEvent,
    bytes_copied: u64,
) -> crate::jobs::types::ActiveFile {
    crate::jobs::types::ActiveFile {
        path: path_to_string(&event.destination),
        bytes_copied,
        bytes_total: event.bytes_total,
        operation: operation_kind_from_native_action(&event.action_kind, ""),
    }
}

fn operation_kind_from_native_action(
    action_kind: &native::CopyActionKind,
    reason: &str,
) -> OperationKind {
    match action_kind {
        native::CopyActionKind::Copy if reason.contains("changed") => OperationKind::Update,
        native::CopyActionKind::Copy => OperationKind::Copy,
        native::CopyActionKind::Skip => OperationKind::Skip,
        native::CopyActionKind::Verify => OperationKind::Verify,
        native::CopyActionKind::Delete => OperationKind::Delete,
        native::CopyActionKind::Mkdir | native::CopyActionKind::Metadata => {
            OperationKind::MetadataOnly
        }
        native::CopyActionKind::Error => OperationKind::ErrorRisk,
    }
}

fn progress_from_report(
    record: &JobRecord,
    report: &native::CopyReport,
    phase: JobPhase,
    elapsed_seconds: f64,
) -> JobProgress {
    let bytes_total = record.plan.totals.bytes;
    let bytes_copied = report.bytes_copied.min(bytes_total);
    let rate_bytes_per_second = bytes_copied as f64 / elapsed_seconds.max(0.001);

    JobProgress {
        job_id: record.job_id.clone(),
        plan_id: record.plan_id.clone(),
        phase,
        backend: record.plan.request.backend_mode.clone(),
        bytes_copied,
        bytes_total,
        // NOT `copied + verified`: on a job that copies WITH verification the same files are
        // counted in both, so the sum double-counted and could exceed the plan total - a two-file
        // copy with the app's default Size verification reported 4 of 2 files. Verified files are
        // a subset of copied ones for every mode this app can request.
        files_copied: report.copied_files,
        files_total: record.plan.totals.files,
        files_skipped: report.skipped_files,
        error_count: report.errors.len() as u64,
        rate_bytes_per_second,
        eta_seconds: Some(0.0),
        active_files: Vec::new(),
    }
}

fn summary_from_report(
    record: &JobRecord,
    report: &native::CopyReport,
    elapsed_seconds: f64,
) -> JobSummary {
    let elapsed = elapsed_seconds.max(0.001);
    JobSummary {
        job_id: record.job_id.clone(),
        plan_id: record.plan_id.clone(),
        source: record.plan.request.source.clone(),
        target: record.plan.request.target.clone(),
        mode: record.plan.request.mode.clone(),
        phase: JobPhase::Completed,
        verify_mode: record.plan.request.verify_mode.clone(),
        bytes_copied: report.bytes_copied,
        bytes_total: record.plan.totals.bytes,
        // NOT `copied + verified`: on a job that copies WITH verification the same files are
        // counted in both, so the sum double-counted and could exceed the plan total - a two-file
        // copy with the app's default Size verification reported 4 of 2 files. Verified files are
        // a subset of copied ones for every mode this app can request.
        files_copied: report.copied_files,
        files_skipped: report.skipped_files,
        error_count: report.errors.len() as u64,
        elapsed_seconds: elapsed,
        average_rate_bytes_per_second: report.bytes_copied as f64 / elapsed,
        manifest_path: None,
    }
}

fn command_error_from_copy_error(code: &str, error: &native::CopyError) -> CommandError {
    CommandError::new(code, error.message.clone())
}

fn job_error_from_copy_error(error: &native::CopyError) -> JobError {
    JobError {
        path: error.path.as_ref().map(|path| path_to_string(path)),
        category: match error.category {
            native::CopyErrorCategory::VerifyMismatch => ErrorCategory::VerificationMismatch,
            native::CopyErrorCategory::InvalidInput => ErrorCategory::Unknown,
            native::CopyErrorCategory::Unsupported => ErrorCategory::Unknown,
            native::CopyErrorCategory::Io => ErrorCategory::Unknown,
            native::CopyErrorCategory::Cancelled => ErrorCategory::Unknown,
        },
        message: error.message.clone(),
        retryable: error.retryable,
        recommended_action: if error.retryable {
            RecommendedAction::Retry
        } else {
            RecommendedAction::ReviewDetails
        },
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn planned_with_state(mut record: JobRecord, state: JobState) -> JobRecord {
    record.state = state;
    record
}

#[cfg(test)]
mod tests {
    use crate::jobs::engine::{
        audit_trees, bundling_hint, effective_thread_count, native_job_from_request, CopyEngine,
        SimulatedEngine,
    };
    use crate::jobs::store::{default_engine_capabilities, JobStore};
    use crate::jobs::types::{
        BackendMode, DeletePolicy, JobFilters, JobState, MetadataMode, TransferMode, VerifyMode,
    };

    #[test]
    fn a_zero_thread_count_is_derived_and_a_fixed_one_is_honoured() {
        let dir = Scratch::new("threads");
        for i in 0..4 {
            std::fs::write(dir.path().join(format!("f{i}.bin")), b"x").expect("write");
        }
        let target = Scratch::new("threads-target");
        let mut request = sample_request();
        request.source = dir.path().to_string_lossy().to_string();
        request.target = target.path().to_string_lossy().to_string();
        request.thread_count = 0;
        let derived = effective_thread_count(&request);
        assert!(derived > 0);
        assert_eq!(
            derived,
            native::recommended_threads_for_tree_path(dir.path(), target.path()) as u16
        );
        request.thread_count = 4;
        assert_eq!(effective_thread_count(&request), 4);
    }

    #[test]
    fn resume_selects_the_engine_resume_mode_for_copy_only() {
        let mut request = sample_request();
        request.mode = TransferMode::Copy;
        request.resume = true;
        assert!(matches!(
            native_job_from_request(&request).mode,
            native::CopyMode::Resume
        ));
        // A mirror run has no partial to continue from, so resume must not silently replace its mode.
        request.mode = TransferMode::Mirror;
        assert!(matches!(
            native_job_from_request(&request).mode,
            native::CopyMode::Mirror
        ));
    }

    #[test]
    fn stop_on_error_selects_the_strict_error_policy() {
        let mut request = sample_request();
        request.stop_on_error = false;
        assert!(matches!(
            native_job_from_request(&request).error_policy,
            native::ErrorPolicy::BestEffort
        ));
        request.stop_on_error = true;
        assert!(matches!(
            native_job_from_request(&request).error_policy,
            native::ErrorPolicy::Strict
        ));
    }

    #[test]
    fn the_bundling_hint_is_silent_for_a_tree_that_is_not_small_file_heavy() {
        let mut request = sample_request();
        request.source = "/definitely/not/a/directory".to_string();
        assert!(bundling_hint(&request).is_none());
    }

    #[test]
    fn an_audit_reports_differences_without_touching_the_target() {
        let source = Scratch::new("audit-source");
        let target = Scratch::new("audit-target");
        fs::write(source.join("same.txt"), "same").expect("write");
        fs::write(target.join("same.txt"), "same").expect("write");
        fs::write(source.join("changed.txt"), "new content").expect("write");
        fs::write(target.join("changed.txt"), "old").expect("write");
        // Present in the source, absent on the target: the case an operator most wants named.
        fs::write(source.join("only-in-source.txt"), "never transferred").expect("write");
        fs::write(target.join("stray.txt"), "not in source").expect("write");

        let report = audit_trees(
            &source.0.to_string_lossy(),
            &target.0.to_string_lossy(),
            VerifyMode::Size,
        )
        .expect("audit");
        assert!(!report.clean);
        assert_eq!(report.verify, "size");
        assert_eq!(report.matching_files, 1);
        assert_eq!(
            report.differing_files, 2,
            "changed + never transferred: {:?}",
            report.differing
        );
        assert_eq!(report.extra_files, 1);
        // Read-only: the stray file is still there and the differing file still holds the old bytes.
        assert!(target.join("stray.txt").exists());
        assert_eq!(
            fs::read(target.join("changed.txt")).expect("read"),
            b"old".to_vec()
        );
    }

    #[test]
    fn an_audit_under_manifest_verification_is_refused_rather_than_downgraded() {
        let source = Scratch::new("audit-manifest-source");
        let target = Scratch::new("audit-manifest-target");
        let refused = audit_trees(
            &source.0.to_string_lossy(),
            &target.0.to_string_lossy(),
            VerifyMode::Manifest,
        );
        assert!(refused.is_err());
        assert_eq!(refused.unwrap_err().code, "verify_mode_unsupported");
    }

    fn sample_request() -> crate::jobs::types::JobRequest {
        crate::jobs::types::JobRequest {
            source: "C:\\Source".to_string(),
            target: "D:\\Target".to_string(),
            mode: TransferMode::Copy,
            filters: JobFilters::default(),
            verify_mode: VerifyMode::Size,
            metadata_mode: MetadataMode::Timestamps,
            backend_mode: BackendMode::Auto,
            thread_count: 4,
            buffer_size_bytes: 1_048_576,
            delete_policy: DeletePolicy::Review,
            resume: false,
            stop_on_error: false,
            manifest_path: None,
        }
    }

    #[test]
    fn simulated_engine_exposes_existing_capability_contract() {
        let engine = SimulatedEngine::default();

        assert_eq!(engine.capabilities(), default_engine_capabilities());
    }

    #[test]
    fn simulated_engine_routes_plan_and_state_transitions_through_store() {
        let store = JobStore::default();
        let engine = SimulatedEngine::default();

        let plan = engine
            .plan(&store, sample_request())
            .expect("plan should be created");
        let running = engine
            .execute(None, &store, &plan.plan_id)
            .expect("planned job should execute");
        let paused = engine
            .pause(None, &store, &plan.plan_id)
            .expect("running job should pause");
        let resumed = engine
            .resume(None, &store, &plan.plan_id)
            .expect("paused job should resume");
        let cancelled = engine
            .cancel(None, &store, &plan.plan_id)
            .expect("running job should cancel");

        assert_eq!(plan.plan_id, "plan-1");
        assert_eq!(running.state, JobState::Running);
        assert_eq!(paused.state, JobState::Paused);
        assert_eq!(resumed.state, JobState::Running);
        assert_eq!(cancelled.state, JobState::Cancelled);
    }

    #[test]
    fn native_request_mapping_preserves_custom_buffer_size() {
        let mut request = sample_request();
        request.buffer_size_bytes = 32 * 1024 * 1024;

        let job = native_job_from_request(&request);

        assert_eq!(job.buffer_size_bytes, 32 * 1024 * 1024);
    }

    // ── The REAL engine seam ────────────────────────────────────────────────
    //
    // Everything below drives the actual engine through the app's own mapping. The tests above
    // exercise `SimulatedEngine`, a test-only stand-in, which is what the whole seam used to be
    // covered by - i.e. not at all.

    use super::native;
    use super::{app_plan_details_from_native, OperationKind};
    use crate::jobs::types::JobRequest;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Unique scratch directory, removed on drop (the app crate has no `tempfile` dependency).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("tallow-app-engine-{}-{}", tag, std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create scratch dir");
            Scratch(path)
        }

        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_manifest_verification_request_reaches_the_engine_and_checks_the_result() {
        let scratch = Scratch::new("manifest-policy");
        let src = scratch.join("src");
        let dst = scratch.join("dst");
        fs::create_dir_all(&src).expect("src");
        fs::write(src.join("a.txt"), "alpha").expect("write");
        fs::write(src.join("b.txt"), "beta").expect("write");
        // Written by the engine's own writer, so this test is about the app's wiring and not the format.
        let manifest = scratch.join("manifest.tsv");
        native::create_manifest(&src, &manifest).expect("create manifest");

        let mut request = real_request(&src, &dst);
        request.verify_mode = VerifyMode::Manifest;
        request.manifest_path = Some(manifest.to_string_lossy().to_string());

        let job = native_job_from_request(&request);
        assert_eq!(job.verify, native::VerifyPolicy::Manifest);
        assert!(job.manifest.is_some(), "the path must reach the engine, not be dropped");

        let planned = native::plan(job).expect("plan");
        let report = native::execute(&planned);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.verified_files, 2, "the manifest pass checked both copied files");
    }

    #[test]
    fn a_blank_manifest_path_is_not_a_manifest() {
        let mut request = real_request(Path::new("/source"), Path::new("/target"));
        request.verify_mode = VerifyMode::Manifest;
        request.manifest_path = Some("   ".to_string());
        // Whitespace is not a path: passing it through would turn the engine's clear refusal into a
        // confusing file-not-found after the transfer.
        assert!(native_job_from_request(&request).manifest.is_none());
    }

    fn real_request(source: &Path, target: &Path) -> JobRequest {
        JobRequest {
            source: source.to_string_lossy().to_string(),
            target: target.to_string_lossy().to_string(),
            mode: TransferMode::Copy,
            filters: JobFilters::default(),
            verify_mode: VerifyMode::Size,
            // The app's declared default (`default_settings().default_metadata_mode`); a mode that
            // carries mtimes is what makes a repeat Sync incremental.
            metadata_mode: MetadataMode::Timestamps,
            backend_mode: BackendMode::Auto,
            thread_count: 4,
            buffer_size_bytes: 1_048_576,
            delete_policy: DeletePolicy::Never,
            resume: false,
            stop_on_error: false,
            manifest_path: None,
        }
    }

    #[test]
    fn every_app_mode_maps_onto_the_engine_policy_it_intends() {
        let scratch = Scratch::new("mapping");
        let src = scratch.join("src");
        fs::create_dir_all(&src).unwrap();
        let mut req = real_request(&src, &scratch.join("dst"));

        req.mode = TransferMode::Copy;
        assert_eq!(native_job_from_request(&req).mode, native::CopyMode::Copy);
        req.mode = TransferMode::Sync;
        assert_eq!(native_job_from_request(&req).mode, native::CopyMode::Sync);

        // A mirror the user has not permitted to delete must not become the mode that deletes.
        req.mode = TransferMode::Mirror;
        req.delete_policy = DeletePolicy::Never;
        assert_eq!(
            native_job_from_request(&req).mode,
            native::CopyMode::Sync,
            "mirror without delete permission must degrade to sync, never to mirror"
        );
        for policy in [DeletePolicy::Review, DeletePolicy::Allow] {
            req.delete_policy = policy;
            assert_eq!(
                native_job_from_request(&req).mode,
                native::CopyMode::Mirror,
                "mirror stays mirror when deletion is permitted"
            );
        }

        // Every variant of both enums, so an engine-side rename cannot pass silently.
        for (app, engine) in [
            (VerifyMode::None, native::VerifyPolicy::None),
            (VerifyMode::Size, native::VerifyPolicy::Size),
            (VerifyMode::SampledHash, native::VerifyPolicy::SampledHash),
            (VerifyMode::FullHash, native::VerifyPolicy::FullHash),
            (VerifyMode::ReadAfterWrite, native::VerifyPolicy::ReadAfterWrite),
            (VerifyMode::Manifest, native::VerifyPolicy::Manifest),
        ] {
            req.verify_mode = app.clone();
            assert_eq!(native_job_from_request(&req).verify, engine, "verify {app:?}");
        }
        for (app, engine) in [
            (MetadataMode::DataOnly, native::MetadataPolicy::DataOnly),
            (MetadataMode::Timestamps, native::MetadataPolicy::Timestamps),
            (MetadataMode::Attributes, native::MetadataPolicy::Attributes),
            (MetadataMode::Security, native::MetadataPolicy::Security),
            (MetadataMode::All, native::MetadataPolicy::All),
        ] {
            req.metadata_mode = app.clone();
            assert_eq!(
                native_job_from_request(&req).metadata,
                engine,
                "metadata {app:?}"
            );
        }
        req.filters.follow_symlinks = false;
        assert_eq!(native_job_from_request(&req).link_policy, native::LinkPolicy::Skip);
        req.filters.follow_symlinks = true;
        assert_eq!(native_job_from_request(&req).link_policy, native::LinkPolicy::Follow);

        // The app always sends a concrete thread count, so it never picks up the engine's
        // path-class auto-derivation (signalled by threads == 0), and the buffer keeps its floor.
        req.thread_count = 0;
        assert_eq!(
            native_job_from_request(&req).threads,
            1,
            "a zero thread count must not reach the engine, where 0 means 'derive it'"
        );
        req.thread_count = 16;
        assert_eq!(native_job_from_request(&req).threads, 16);
        req.buffer_size_bytes = 0;
        assert_eq!(native_job_from_request(&req).buffer_size_bytes, 4096);
    }

    #[test]
    fn a_mapped_request_really_copies_and_the_plan_translates() {
        let scratch = Scratch::new("execute");
        let src = scratch.join("src");
        let dst = scratch.join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        fs::write(src.join("sub/b.bin"), vec![7u8; 4096]).unwrap();

        let req = real_request(&src, &dst);
        let plan = native::plan(native_job_from_request(&req)).expect("plan");
        assert_eq!(plan.total_files, 2);
        assert_eq!(plan.total_bytes, 5 + 4096);

        let (operations, totals, risk) = app_plan_details_from_native(&req, &plan);
        assert_eq!(totals.files, 2);
        assert!(!risk.requires_review, "a plain copy needs no delete review");
        assert!(
            operations.iter().any(|op| op.kind == OperationKind::Copy),
            "expected a copy operation, got {:?}",
            operations.iter().map(|op| op.kind.clone()).collect::<Vec<_>>()
        );
        for op in &operations {
            assert!(op.target.is_some(), "every operation needs a target for the UI");
        }

        let report = native::execute(&plan);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.copied_files, 2);
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(dst.join("sub/b.bin")).unwrap(), vec![7u8; 4096]);

        // The app's Sync promise: re-planning the same request must find nothing left to do.
        let second = native::plan(native_job_from_request(&req)).expect("plan 2");
        let (ops2, _, _) = app_plan_details_from_native(&req, &second);
        assert_eq!(
            ops2.iter()
                .filter(|op| op.kind == OperationKind::Copy)
                .count(),
            0,
            "a repeat Sync must be incremental; plan was {:?}",
            ops2.iter().map(|op| op.kind.clone()).collect::<Vec<_>>()
        );

        // Why the contrast above is NOT the story it first looks like, measured rather than
        // assumed: `SkipPolicy::SizeMtime` compares WHOLE SECONDS (`modified_seconds`), and a fast
        // copy lands in the same second as its source - so DataOnly, which gives the destination
        // the copy time, still matched and still skipped (0 copies, not the 2 I first predicted).
        // The boundary is an mtime difference of a second or more, which a slow copy can cross.
        // That is the hazard the app's Timestamps default protects it from, and it is pinned here:
        let dst_gap = scratch.join("dst-past-second");
        let req_gap = real_request(&src, &dst_gap);
        let plan_gap = native::plan(native_job_from_request(&req_gap)).expect("plan gap");
        assert_eq!(native::execute(&plan_gap).copied_files, 2);

        let ahead = fs::metadata(src.join("a.txt"))
            .unwrap()
            .modified()
            .unwrap()
            + std::time::Duration::from_secs(2);
        fs::File::options()
            .write(true)
            .open(dst_gap.join("a.txt"))
            .unwrap()
            .set_modified(ahead)
            .unwrap();

        let second_gap = native::plan(native_job_from_request(&req_gap)).expect("plan gap 2");
        let scheduled = second_gap
            .actions
            .iter()
            .filter(|action| action.kind == native::CopyActionKind::Copy)
            .count();
        assert!(
            scheduled >= 1,
            "an mtime two seconds off the source must defeat the size+mtime skip; plan was {:?}",
            second_gap
                .actions
                .iter()
                .map(|action| action.kind.clone())
                .collect::<Vec<_>>()
        );

        // And the app-level translation of a change: it must reach the UI as an Update rather than
        // a Copy, which is the branch that tells the user "modified" instead of "new".
        let (ops_gap, _, _) = app_plan_details_from_native(&req_gap, &second_gap);
        assert!(
            ops_gap.iter().any(|op| op.kind == OperationKind::Update),
            "a changed file must reach the UI as an update, got {:?}",
            ops_gap.iter().map(|op| op.kind.clone()).collect::<Vec<_>>()
        );

    }
    #[test]
    fn the_reports_the_app_shows_do_not_double_count_verified_files() {
        use crate::jobs::store::JobStore;
        use crate::jobs::types::JobPhase;

        let scratch = Scratch::new("reports");
        let src = scratch.join("src");
        let dst = scratch.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        fs::write(src.join("b.txt"), vec![1u8; 2048]).unwrap();

        let store = JobStore::default();
        let req = real_request(&src, &dst); // VerifyMode::Size, the app's declared default
        let plan = native::plan(native_job_from_request(&req)).unwrap();
        let (ops, totals, risk) = app_plan_details_from_native(&req, &plan);
        let planned = store
            .plan_job_with_details(req.clone(), ops, totals, risk)
            .expect("plan");
        let record = store.get_job(&planned.plan_id).expect("stored record");

        let report = native::execute(&plan);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.copied_files, 2);
        assert_eq!(
            report.verified_files, 2,
            "verification covers the same files, which is what made the old sum read 4"
        );

        let progress = super::progress_from_report(&record, &report, JobPhase::Copying, 0.5);
        assert_eq!(progress.files_copied, 2);
        assert_eq!(progress.files_total, 2);
        assert!(
            progress.files_copied <= progress.files_total,
            "progress must never claim more files than the plan holds: {progress:?}"
        );
        assert!(progress.bytes_copied <= progress.bytes_total);
        assert!(progress.rate_bytes_per_second > 0.0);
        assert_eq!(progress.error_count, 0);

        let summary = super::summary_from_report(&record, &report, 0.5);
        assert_eq!(summary.files_copied, 2, "the final summary carries the same rule");
        assert_eq!(summary.bytes_copied, report.bytes_copied);
        assert_eq!(summary.phase, JobPhase::Completed);
    }

    #[test]
    fn an_engine_error_reaches_the_app_categorized() {
        let scratch = Scratch::new("errors");
        let src = scratch.join("src");
        fs::create_dir_all(&src).unwrap();
        let mut req = real_request(&src, &scratch.join("dst"));
        req.source = scratch.join("does-not-exist").to_string_lossy().to_string();

        let error = native::plan(native_job_from_request(&req))
            .expect_err("a missing source must fail to plan");

        let command = super::command_error_from_copy_error("native_plan_failed", &error);
        assert_eq!(command.code, "native_plan_failed");
        assert!(!command.message.is_empty());

        let job_error = super::job_error_from_copy_error(&error);
        assert!(
            job_error.path.is_some(),
            "the failing path must reach the UI: {job_error:?}"
        );
        assert!(!job_error.message.is_empty());
        println!("engine error mapped to category {:?}", job_error.category);
    }
    /// A host that records instead of emitting, and whose store is a real one - so the execution
    /// path can be run to completion, events and terminal state included, with no display.
    struct RecordingHost {
        events: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
        store: crate::jobs::store::JobStore,
    }

    impl RecordingHost {
        fn new(store: crate::jobs::store::JobStore) -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
                store,
            }
        }

        fn names(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _)| name.clone())
                .collect()
        }

        fn payloads(&self, name: &str) -> Vec<serde_json::Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(event, _)| event == name)
                .map(|(_, payload)| payload.clone())
                .collect()
        }
    }

    impl super::JobHost for RecordingHost {
        fn emit_json(&self, event: &str, payload: serde_json::Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }

        fn complete(&self, job_id: &str) {
            let _ = self.store.complete_job(job_id);
        }

        fn fail(&self, job_id: &str) {
            let _ = self.store.fail_job(job_id);
        }
    }

    #[test]
    fn the_execution_path_emits_its_whole_event_sequence_and_records_the_outcome() {
        use crate::jobs::store::JobStore;
        use crate::jobs::types::{
            JobPhase, JobState, EVENT_JOB_COMPLETED, EVENT_JOB_ERROR, EVENT_JOB_FILE_FINISHED,
            EVENT_JOB_FILE_STARTED, EVENT_JOB_PROGRESS,
        };
        use std::sync::Arc;

        let scratch = Scratch::new("host");
        let src = scratch.join("src");
        let dst = scratch.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        fs::write(src.join("b.bin"), vec![9u8; 4096]).unwrap();

        let store = JobStore::default();
        let req = real_request(&src, &dst);
        let plan = native::plan(native_job_from_request(&req)).unwrap();
        let (ops, totals, risk) = app_plan_details_from_native(&req, &plan);
        let planned = store.plan_job_with_details(req.clone(), ops, totals, risk).unwrap();
        let record = store.get_job(&planned.plan_id).expect("stored record");

        // The real glue marks the job running before the worker starts (that is what `execute`
        // does before it spawns); a worker completion of a still-Planned job is refused by the
        // store, which is how this test found the ordering it must mirror.
        assert_eq!(
            store.execute_job(&record.job_id).expect("running").state,
            JobState::Running
        );
        let recorder = Arc::new(RecordingHost::new(store));
        let host: Arc<dyn super::JobHost> = recorder.clone();
        // This is the body the spawned worker runs, and it is synchronous itself - so the whole
        // sequence is asserted with no thread to wait on.
        super::run_native_execution(
            host.clone(),
            record.clone(),
            plan,
            native::CopyControl::new(),
        );

        let names = recorder.names();
        for expected in [
            EVENT_JOB_FILE_STARTED,
            EVENT_JOB_FILE_FINISHED,
            EVENT_JOB_PROGRESS,
            EVENT_JOB_COMPLETED,
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "expected a {expected} event, got {names:?}"
            );
        }
        assert!(
            !names.iter().any(|name| name == EVENT_JOB_ERROR),
            "a clean copy must emit no error event: {names:?}"
        );
        assert_eq!(
            names.last().map(String::as_str),
            Some(EVENT_JOB_COMPLETED),
            "the run must finish with the completion event: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|name| *name == EVENT_JOB_FILE_STARTED).count(),
            2,
            "one file-started event per file: {names:?}"
        );

        // The per-file events name the files that were touched.
        let started = serde_json::to_string(&recorder.payloads(EVENT_JOB_FILE_STARTED)).unwrap();
        assert!(started.contains("a.txt") && started.contains("b.bin"), "{started}");

        // Every progress payload keeps the invariant that the UI counter cannot exceed its total.
        for payload in recorder.payloads(EVENT_JOB_PROGRESS) {
            let copied = payload["filesCopied"].as_u64().unwrap_or(0);
            let total = payload["filesTotal"].as_u64().unwrap_or(u64::MAX);
            assert!(
                copied <= total,
                "progress claimed {copied} of {total}: {payload}"
            );
        }

        // The completion payload carries the engine's numbers, and the phase as the app serialises it.
        let completed = recorder.payloads(EVENT_JOB_COMPLETED).pop().expect("completed payload");
        assert_eq!(completed["filesCopied"], 2);
        assert_eq!(completed["bytesCopied"], 5 + 4096);
        assert_eq!(completed["phase"], serde_json::to_value(JobPhase::Completed).unwrap());

        // And the outcome reached the store through the same port the app implements.
        assert_eq!(
            recorder.store.get_job(&record.job_id).unwrap().state,
            JobState::Completed,
            "the terminal transition must be recorded, not just announced"
        );
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"alpha");
    }
}
