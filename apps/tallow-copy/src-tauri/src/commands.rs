use std::sync::OnceLock;

#[cfg(not(test))]
use crate::jobs::engine::NativeTallowEngine;
#[cfg(test)]
use crate::jobs::engine::SimulatedEngine;
use crate::jobs::engine::{CopyEngine, EngineAppHandle};
use crate::jobs::store::JobStore;
#[cfg(not(test))]
use crate::jobs::types::EngineCapabilities;
#[cfg(not(test))]
use crate::jobs::types::{AppSettings, AppSettingsPatch};
use crate::jobs::types::{
    CommandError, JobHistoryEntry, JobPlan, JobRecord, JobRequest, TreeAuditReport, VerifyMode,
};
use serde::Deserialize;
#[cfg(not(test))]
use tauri_plugin_dialog::DialogExt;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSelectionKind {
    Source,
    Target,
}

#[cfg(test)]
fn copy_engine() -> &'static SimulatedEngine {
    static ENGINE: OnceLock<SimulatedEngine> = OnceLock::new();
    ENGINE.get_or_init(SimulatedEngine::default)
}

#[cfg(not(test))]
fn copy_engine() -> &'static NativeTallowEngine {
    static ENGINE: OnceLock<NativeTallowEngine> = OnceLock::new();
    ENGINE.get_or_init(NativeTallowEngine::default)
}

#[cfg(not(test))]
#[tauri::command]
pub fn get_engine_capabilities() -> EngineCapabilities {
    crate::diagnostics::log_info("get_engine_capabilities invoked");
    copy_engine().capabilities()
}

#[cfg(not(test))]
#[tauri::command]
pub fn select_path(
    app: tauri::AppHandle,
    kind: PathSelectionKind,
) -> Result<Option<String>, CommandError> {
    crate::diagnostics::log_info(format!("select_path invoked kind={kind:?}"));
    let label = match kind {
        PathSelectionKind::Source => "source",
        PathSelectionKind::Target => "target",
    };

    let selected_path = app
        .dialog()
        .file()
        .set_title(format!("Select {label} folder"))
        .blocking_pick_folder();

    let result = selected_path.map(|path| path.to_string());
    crate::diagnostics::log_info(format!(
        "select_path completed kind={kind:?} selected={}",
        result.is_some()
    ));
    Ok(result)
}

#[cfg(not(test))]
#[tauri::command]
pub fn audit_transfer(
    source: String,
    target: String,
    verify_mode: VerifyMode,
) -> Result<TreeAuditReport, CommandError> {
    audit_transfer_with(&source, &target, verify_mode)
}

pub(crate) fn audit_transfer_with(
    source: &str,
    target: &str,
    verify_mode: VerifyMode,
) -> Result<TreeAuditReport, CommandError> {
    crate::jobs::engine::audit_trees(source, target, verify_mode)
}

#[cfg(not(test))]
#[tauri::command]
pub fn list_job_history(store: tauri::State<'_, JobStore>) -> Result<Vec<JobHistoryEntry>, CommandError> {
    list_job_history_with_store(store.inner())
}

pub(crate) fn list_job_history_with_store(
    store: &JobStore,
) -> Result<Vec<JobHistoryEntry>, CommandError> {
    store.list_history()
}

#[cfg(not(test))]
#[tauri::command]
pub fn record_job_history(
    store: tauri::State<'_, JobStore>,
    entry: JobHistoryEntry,
) -> Result<(), CommandError> {
    record_job_history_with_store(store.inner(), entry)
}

pub(crate) fn record_job_history_with_store(
    store: &JobStore,
    entry: JobHistoryEntry,
) -> Result<(), CommandError> {
    store.record_history(entry)
}

#[cfg(not(test))]
#[tauri::command]
pub fn plan_job(
    store: tauri::State<'_, JobStore>,
    request: JobRequest,
) -> Result<JobPlan, CommandError> {
    crate::diagnostics::log_info(format!(
        "plan_job invoked source={} target={} mode={:?} verify={:?} metadata={:?} backend={:?} threads={} buffer={}",
        request.source,
        request.target,
        request.mode,
        request.verify_mode,
        request.metadata_mode,
        request.backend_mode,
        request.thread_count,
        request.buffer_size_bytes
    ));
    plan_job_with_store(store.inner(), request)
}

pub(crate) fn plan_job_with_store(
    store: &JobStore,
    request: JobRequest,
) -> Result<JobPlan, CommandError> {
    plan_job_with_engine(copy_engine(), store, request)
}

pub(crate) fn plan_job_with_engine(
    engine: &impl CopyEngine,
    store: &JobStore,
    request: JobRequest,
) -> Result<JobPlan, CommandError> {
    engine.plan(store, request)
}

#[cfg(not(test))]
#[tauri::command]
pub fn execute_job(
    app: tauri::AppHandle,
    store: tauri::State<'_, JobStore>,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    crate::diagnostics::log_info(format!("execute_job invoked job_id={job_id}"));
    execute_job_with_engine(copy_engine(), Some(app), store.inner(), job_id)
}

#[cfg(test)]
pub(crate) fn execute_job_with_store(
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    execute_job_with_engine(copy_engine(), None, store, job_id)
}

pub(crate) fn execute_job_with_engine(
    engine: &impl CopyEngine,
    app: Option<EngineAppHandle>,
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    engine.execute(app, store, &job_id)
}

#[cfg(not(test))]
#[tauri::command]
pub fn pause_job(
    app: tauri::AppHandle,
    store: tauri::State<'_, JobStore>,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    pause_job_with_engine(copy_engine(), Some(app), store.inner(), job_id)
}

#[cfg(test)]
pub(crate) fn pause_job_with_store(
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    pause_job_with_engine(copy_engine(), None, store, job_id)
}

pub(crate) fn pause_job_with_engine(
    engine: &impl CopyEngine,
    app: Option<EngineAppHandle>,
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    engine.pause(app, store, &job_id)
}

#[cfg(not(test))]
#[tauri::command]
pub fn resume_job(
    app: tauri::AppHandle,
    store: tauri::State<'_, JobStore>,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    resume_job_with_engine(copy_engine(), Some(app), store.inner(), job_id)
}

#[cfg(test)]
pub(crate) fn resume_job_with_store(
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    resume_job_with_engine(copy_engine(), None, store, job_id)
}

pub(crate) fn resume_job_with_engine(
    engine: &impl CopyEngine,
    app: Option<EngineAppHandle>,
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    engine.resume(app, store, &job_id)
}

#[cfg(not(test))]
#[tauri::command]
pub fn cancel_job(
    app: tauri::AppHandle,
    store: tauri::State<'_, JobStore>,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    crate::diagnostics::log_info(format!("cancel_job invoked job_id={job_id}"));
    cancel_job_with_engine(copy_engine(), Some(app), store.inner(), job_id)
}

#[cfg(test)]
pub(crate) fn cancel_job_with_store(
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    cancel_job_with_engine(copy_engine(), None, store, job_id)
}

pub(crate) fn cancel_job_with_engine(
    engine: &impl CopyEngine,
    app: Option<EngineAppHandle>,
    store: &JobStore,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    engine.cancel(app, store, &job_id)
}

#[cfg(not(test))]
#[tauri::command]
pub fn list_jobs(store: tauri::State<'_, JobStore>) -> Result<Vec<JobRecord>, CommandError> {
    store.list_jobs()
}

#[cfg(not(test))]
#[tauri::command]
pub fn get_job(
    store: tauri::State<'_, JobStore>,
    job_id: String,
) -> Result<JobRecord, CommandError> {
    store.get_job(&job_id)
}

#[cfg(not(test))]
#[tauri::command]
pub fn get_settings(store: tauri::State<'_, JobStore>) -> Result<AppSettings, CommandError> {
    store.get_settings()
}

#[cfg(not(test))]
#[tauri::command]
pub fn update_settings(
    store: tauri::State<'_, JobStore>,
    patch: AppSettingsPatch,
) -> Result<AppSettings, CommandError> {
    store.update_settings(patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::types::{
        BackendMode, DeletePolicy, JobFilters, JobState, MetadataMode, TransferMode, VerifyMode,
    };

    fn history_entry(job_id: &str) -> JobHistoryEntry {
        JobHistoryEntry {
            job_id: job_id.to_string(),
            plan_id: format!("plan-{job_id}"),
            source: "/source".to_string(),
            target: "/target".to_string(),
            mode: crate::jobs::types::TransferMode::Copy,
            state: crate::jobs::types::JobState::Completed,
            bytes_copied: 4,
            bytes_total: 4,
            files_copied: 1,
            files_total: 1,
            files_skipped: 0,
            error_count: 0,
            elapsed_seconds: 0.5,
            average_rate_bytes_per_second: 8.0,
            completed_at: "2026-10-09T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn the_history_commands_round_trip_through_the_store() {
        let store = JobStore::default();
        record_job_history_with_store(&store, history_entry("job-7")).expect("record");
        let listed = list_job_history_with_store(&store).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].job_id, "job-7");
    }

    fn sample_request() -> JobRequest {
        JobRequest {
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
    fn planning_creates_a_plan_id() {
        let store = JobStore::default();

        let plan = plan_job_with_store(&store, sample_request()).expect("plan should be created");

        assert_eq!(plan.plan_id, "plan-1");
    }

    #[test]
    fn execute_transitions_planned_job_to_running() {
        let store = JobStore::default();
        let plan = plan_job_with_store(&store, sample_request()).expect("plan should be created");

        let record =
            execute_job_with_store(&store, plan.plan_id).expect("planned job should execute");

        assert_eq!(record.state, JobState::Running);
    }

    #[test]
    fn pause_transitions_running_job_to_paused() {
        let store = JobStore::default();
        let plan = plan_job_with_store(&store, sample_request()).expect("plan should be created");
        execute_job_with_store(&store, plan.plan_id.clone()).expect("planned job should execute");

        let record = pause_job_with_store(&store, plan.plan_id).expect("running job should pause");

        assert_eq!(record.state, JobState::Paused);
    }

    #[test]
    fn resume_transitions_paused_job_to_running() {
        let store = JobStore::default();
        let plan = plan_job_with_store(&store, sample_request()).expect("plan should be created");
        execute_job_with_store(&store, plan.plan_id.clone()).expect("planned job should execute");
        pause_job_with_store(&store, plan.plan_id.clone()).expect("running job should pause");

        let record = resume_job_with_store(&store, plan.plan_id).expect("paused job should resume");

        assert_eq!(record.state, JobState::Running);
    }

    #[test]
    fn completion_transitions_running_job_to_completed() {
        let store = JobStore::default();
        let plan = plan_job_with_store(&store, sample_request()).expect("plan should be created");
        execute_job_with_store(&store, plan.plan_id.clone()).expect("planned job should execute");

        let record = store
            .complete_job(&plan.plan_id)
            .expect("running job should complete");

        assert_eq!(record.state, JobState::Completed);
        assert_eq!(
            store
                .get_job(&plan.plan_id)
                .expect("completed job should be readable")
                .state,
            JobState::Completed
        );
        assert_eq!(
            store
                .list_jobs()
                .expect("completed job should be listed")
                .first()
                .expect("one job should exist")
                .state,
            JobState::Completed
        );
    }

    #[test]
    fn cancel_transitions_running_or_paused_jobs_to_cancelled() {
        let running_store = JobStore::default();
        let running_plan =
            plan_job_with_store(&running_store, sample_request()).expect("plan should be created");
        execute_job_with_store(&running_store, running_plan.plan_id.clone())
            .expect("planned job should execute");

        let running_record = cancel_job_with_store(&running_store, running_plan.plan_id)
            .expect("running job should cancel");

        assert_eq!(running_record.state, JobState::Cancelled);

        let paused_store = JobStore::default();
        let paused_plan =
            plan_job_with_store(&paused_store, sample_request()).expect("plan should be created");
        execute_job_with_store(&paused_store, paused_plan.plan_id.clone())
            .expect("planned job should execute");
        pause_job_with_store(&paused_store, paused_plan.plan_id.clone())
            .expect("running job should pause");

        let paused_record = cancel_job_with_store(&paused_store, paused_plan.plan_id)
            .expect("paused job should cancel");

        assert_eq!(paused_record.state, JobState::Cancelled);
    }

    #[test]
    fn invalid_job_id_returns_structured_error() {
        let store = JobStore::default();

        let error = execute_job_with_store(&store, "missing-job".to_string())
            .expect_err("missing job should return an error");

        assert_eq!(error.code, "job_not_found");
        assert!(error.message.contains("missing-job"));
    }

    #[test]
    fn path_selection_kind_deserializes_from_command_payload_values() {
        let source: PathSelectionKind =
            serde_json::from_str("\"source\"").expect("source kind should deserialize");
        let target: PathSelectionKind =
            serde_json::from_str("\"target\"").expect("target kind should deserialize");

        assert_eq!(source, PathSelectionKind::Source);
        assert_eq!(target, PathSelectionKind::Target);
    }

    #[test]
    fn planning_rejects_empty_source() {
        let store = JobStore::default();
        let mut request = sample_request();
        request.source = "  ".to_string();

        let error =
            plan_job_with_store(&store, request).expect_err("empty source should be rejected");

        assert_eq!(error.code, "request_validation_failed");
        assert_eq!(error.issues.len(), 1);
        assert_eq!(error.issues[0].code, "empty_source");
        assert_eq!(error.issues[0].field.as_deref(), Some("source"));
    }

    #[test]
    fn planning_rejects_empty_target() {
        let store = JobStore::default();
        let mut request = sample_request();
        request.target = "".to_string();

        let error =
            plan_job_with_store(&store, request).expect_err("empty target should be rejected");

        assert_eq!(error.code, "request_validation_failed");
        assert_eq!(error.issues.len(), 1);
        assert_eq!(error.issues[0].code, "empty_target");
        assert_eq!(error.issues[0].field.as_deref(), Some("target"));
    }

    #[test]
    fn planning_rejects_identical_source_and_target() {
        let store = JobStore::default();
        let mut request = sample_request();
        request.target = "C:/Source/".to_string();

        let error = plan_job_with_store(&store, request)
            .expect_err("identical source and target should be rejected");

        assert_eq!(error.code, "request_validation_failed");
        assert_eq!(error.issues[0].code, "identical_paths");
    }

    #[test]
    fn planning_rejects_dangerous_mirror_target_inside_source() {
        let store = JobStore::default();
        let mut request = sample_request();
        request.mode = TransferMode::Mirror;
        request.target = "C:\\Source\\NestedBackup".to_string();
        request.delete_policy = DeletePolicy::Review;

        let error = plan_job_with_store(&store, request)
            .expect_err("nested mirror target should be rejected");

        assert_eq!(error.code, "request_validation_failed");
        assert_eq!(error.issues[0].code, "target_inside_source");
        assert!(error.issues[0].destructive);
        assert!(error.issues[0].requires_review);
    }

    #[test]
    fn planning_rejects_mirror_deletes_without_review() {
        let store = JobStore::default();
        let mut request = sample_request();
        request.mode = TransferMode::Mirror;
        request.delete_policy = DeletePolicy::Allow;

        let error = plan_job_with_store(&store, request)
            .expect_err("mirror deletes without review should be rejected");

        assert_eq!(error.code, "request_validation_failed");
        assert_eq!(error.issues[0].code, "mirror_delete_review_required");
        assert!(error.issues[0].destructive);
        assert!(error.issues[0].requires_review);
    }

    #[test]
    fn planning_allows_reviewed_mirror_delete_plan_to_require_review() {
        let store = JobStore::default();
        let mut request = sample_request();
        request.mode = TransferMode::Mirror;
        request.delete_policy = DeletePolicy::Review;

        let plan = plan_job_with_store(&store, request).expect("review mirror should plan");

        assert!(plan.risk_summary.destructive);
        assert!(plan.risk_summary.requires_review);
    }
}

// A compile-time proof, not a test: the production accessor's return type IS the property that matters
// (a stub here would mean the app ships a simulated engine). A `#[test]` cannot assert this, because in
// test builds the accessor deliberately returns the simulated engine and the native type is not in
// scope - so the check runs on every production build instead, where it cannot be skipped.
#[cfg(not(test))]
const _PRODUCTION_USES_THE_NATIVE_ENGINE: fn() = || {
    let engine: &NativeTallowEngine = copy_engine();
    let _ = engine.capabilities();
};
