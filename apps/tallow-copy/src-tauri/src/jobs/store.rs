use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::jobs::types::{
    AppSettings, AppSettingsPatch, BackendMode, CommandError, DeletePolicy, EngineCapabilities,
    JobHistoryEntry, JobPlan, JobRecord, JobRequest, JobState, MetadataMode, PlanTotals, RiskSummary,
    TransferMode, ValidationIssue, VerifyMode,
};

/// How many finished jobs are kept on disk. The UI renders the newest 24; keeping more means a long
/// session does not silently discard what it just did.
const HISTORY_LIMIT: usize = 200;

/// Where finished jobs are kept, beside the logs so support can ask for one folder.
pub fn history_path() -> PathBuf {
    crate::diagnostics::data_root()
        .unwrap_or_else(std::env::temp_dir)
        .join("Tallow Copy")
        .join("history.json")
}

pub struct JobStore {
    jobs: Mutex<HashMap<String, JobRecord>>,
    next_id: Mutex<u64>,
    settings: Mutex<AppSettings>,
    history: Mutex<Vec<JobHistoryEntry>>,
    /// `None` means in-memory only: tests, and any run that cannot resolve a data directory, must not
    /// start writing files as a side effect.
    history_path: Option<PathBuf>,
}

impl Default for JobStore {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
            settings: Mutex::new(default_settings()),
            history: Mutex::new(Vec::new()),
            history_path: None,
        }
    }
}

impl JobStore {
    pub fn plan_job(&self, request: JobRequest) -> Result<JobPlan, CommandError> {
        let destructive =
            request.mode == TransferMode::Mirror && request.delete_policy != DeletePolicy::Never;
        let requires_review = destructive
            && (request.delete_policy == DeletePolicy::Review
                || !self.get_settings()?.allow_mirror_deletes_without_review);

        self.plan_job_with_details(
            request,
            Vec::new(),
            PlanTotals::default(),
            RiskSummary {
                destructive,
                requires_review,
                delete_count: 0,
                conflict_count: 0,
                locked_file_count: 0,
                estimated_error_count: 0,
                warnings: Vec::new(),
            },
        )
    }

    pub fn plan_job_with_details(
        &self,
        request: JobRequest,
        operations: Vec<crate::jobs::types::PlannedOperation>,
        totals: PlanTotals,
        mut risk_summary: RiskSummary,
    ) -> Result<JobPlan, CommandError> {
        self.validate_job_request(&request)?;

        let plan_id = self.next_plan_id()?;
        let effective_threads = crate::jobs::engine::effective_thread_count(&request);
        let hint = crate::jobs::engine::bundling_hint(&request);
        // Surface the recommendation through the warnings the UI already renders, so it reaches the
        // operator instead of staying inside the engine.
        if let Some(text) = &hint {
            risk_summary.warnings.push(text.clone());
        }
        let plan = JobPlan {
            plan_id: plan_id.clone(),
            request,
            effective_threads,
            bundling_hint: hint,
            operations,
            totals,
            risk_summary,
        };
        let record = JobRecord {
            job_id: plan_id.clone(),
            plan_id: plan_id.clone(),
            state: JobState::Planned,
            plan: plan.clone(),
        };

        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;
        jobs.insert(plan_id, record);

        Ok(plan)
    }

    pub fn execute_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        self.transition_job(job_id, &[JobState::Planned], JobState::Running, "planned")
    }

    pub fn pause_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        self.transition_job(job_id, &[JobState::Running], JobState::Paused, "running")
    }

    pub fn resume_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        self.transition_job(job_id, &[JobState::Paused], JobState::Running, "paused")
    }

    pub fn cancel_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        self.transition_job(
            job_id,
            &[JobState::Running, JobState::Paused],
            JobState::Cancelled,
            "running or paused",
        )
    }

    pub fn complete_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        self.transition_job(job_id, &[JobState::Running], JobState::Completed, "running")
    }

    pub fn fail_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        self.transition_job(job_id, &[JobState::Running], JobState::Failed, "running")
    }

    pub fn list_jobs(&self) -> Result<Vec<JobRecord>, CommandError> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;
        let mut records = jobs.values().cloned().collect::<Vec<_>>();
        records.sort_by(|left, right| left.job_id.cmp(&right.job_id));
        Ok(records)
    }

    /// A store whose finished jobs survive a restart. Loading is best effort: a file that cannot be read
    /// is kept aside rather than deleted, and never stops the app from starting.
    pub fn with_history(path: PathBuf) -> Self {
        let loaded = load_history(&path);
        Self {
            history: Mutex::new(loaded),
            history_path: Some(path),
            ..Self::default()
        }
    }

    pub fn list_history(&self) -> Result<Vec<JobHistoryEntry>, CommandError> {
        let history = self
            .history
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;
        Ok(history.clone())
    }

    /// File a finished job. Newest first, deduplicated by job id, capped, then persisted atomically.
    pub fn record_history(&self, entry: JobHistoryEntry) -> Result<(), CommandError> {
        if !matches!(
            entry.state,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        ) {
            // Only a finished job belongs here; filing a running one would turn the report into a claim.
            crate::diagnostics::log_warn(format!(
                "not filing job {} in history while it is {:?}",
                entry.job_id, entry.state
            ));
            return Ok(());
        }
        {
            let mut history = self
                .history
                .lock()
                .map_err(|_| CommandError::store_unavailable())?;
            history.retain(|item| item.job_id != entry.job_id);
            history.insert(0, entry);
            history.truncate(HISTORY_LIMIT);
        }
        self.persist_history();
        Ok(())
    }

    fn persist_history(&self) {
        let Some(path) = self.history_path.as_ref() else {
            return;
        };
        let snapshot = match self.history.lock() {
            Ok(history) => history.clone(),
            Err(_) => return,
        };
        if let Err(error) = write_history(path, &snapshot) {
            crate::diagnostics::log_warn(format!("could not persist job history: {}", error));
        }
    }

    pub fn get_job(&self, job_id: &str) -> Result<JobRecord, CommandError> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;
        jobs.get(job_id)
            .cloned()
            .ok_or_else(|| CommandError::job_not_found(job_id))
    }

    pub fn get_settings(&self) -> Result<AppSettings, CommandError> {
        self.settings
            .lock()
            .map(|settings| settings.clone())
            .map_err(|_| CommandError::store_unavailable())
    }

    pub fn update_settings(&self, patch: AppSettingsPatch) -> Result<AppSettings, CommandError> {
        let mut settings = self
            .settings
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;

        if let Some(value) = patch.default_verify_mode {
            settings.default_verify_mode = value;
        }
        if let Some(value) = patch.default_metadata_mode {
            settings.default_metadata_mode = value;
        }
        if let Some(value) = patch.default_backend_mode {
            settings.default_backend_mode = value;
        }
        if let Some(value) = patch.default_thread_count {
            settings.default_thread_count = value;
        }
        if let Some(value) = patch.default_buffer_size_bytes {
            settings.default_buffer_size_bytes = value;
        }
        if let Some(value) = patch.delete_policy {
            settings.delete_policy = value;
        }
        if let Some(value) = patch.allow_mirror_deletes_without_review {
            settings.allow_mirror_deletes_without_review = value;
        }
        if let Some(value) = patch.throttle_progress_millis {
            settings.throttle_progress_millis = value;
        }
        if let Some(value) = patch.preserve_window_state {
            settings.preserve_window_state = value;
        }

        Ok(settings.clone())
    }

    pub fn validate_job_request(&self, request: &JobRequest) -> Result<(), CommandError> {
        let issues = validate_job_request(request);
        if issues.is_empty() {
            Ok(())
        } else {
            Err(CommandError::request_validation_failed(issues))
        }
    }

    fn next_plan_id(&self) -> Result<String, CommandError> {
        let mut next_id = self
            .next_id
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;
        let plan_id = format!("plan-{}", *next_id);
        *next_id += 1;
        Ok(plan_id)
    }

    fn transition_job(
        &self,
        job_id: &str,
        allowed_states: &[JobState],
        next_state: JobState,
        expected: &str,
    ) -> Result<JobRecord, CommandError> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| CommandError::store_unavailable())?;
        let record = jobs
            .get_mut(job_id)
            .ok_or_else(|| CommandError::job_not_found(job_id))?;

        if !allowed_states.contains(&record.state) {
            return Err(CommandError::invalid_job_state(
                job_id,
                expected,
                &record.state,
            ));
        }

        record.state = next_state;
        Ok(record.clone())
    }
}

fn validate_job_request(request: &JobRequest) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let source = request.source.trim();
    let target = request.target.trim();

    if source.is_empty() {
        issues.push(ValidationIssue::new(
            "empty_source",
            Some("source"),
            "Choose a source path before planning.",
            false,
            false,
        ));
    }

    if target.is_empty() {
        issues.push(ValidationIssue::new(
            "empty_target",
            Some("target"),
            "Choose a target path before planning.",
            false,
            false,
        ));
    }

    if !source.is_empty() && !target.is_empty() {
        let source_path = comparable_path(source);
        let target_path = comparable_path(target);

        if source_path == target_path {
            issues.push(ValidationIssue::new(
                "identical_paths",
                Some("target"),
                "Source and target must be different paths.",
                false,
                false,
            ));
        } else if request.mode == TransferMode::Mirror
            && is_descendant_path(&source_path, &target_path)
        {
            issues.push(ValidationIssue::new(
                "target_inside_source",
                Some("target"),
                "A mirror target cannot be inside the source tree.",
                request.delete_policy != DeletePolicy::Never,
                request.delete_policy != DeletePolicy::Never,
            ));
        }
    }

    if request.mode == TransferMode::Mirror && request.delete_policy == DeletePolicy::Allow {
        issues.push(ValidationIssue::new(
            "mirror_delete_review_required",
            Some("deletePolicy"),
            "Mirror deletes require review before they can be allowed.",
            true,
            true,
        ));
    }

    issues
}

fn comparable_path(path: &str) -> String {
    let mut normalized = path.trim().replace('/', "\\");
    while normalized.len() > 3 && normalized.ends_with('\\') {
        normalized.pop();
    }
    normalized.to_ascii_lowercase()
}

fn is_descendant_path(parent: &str, child: &str) -> bool {
    let mut prefix = parent.to_string();
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }
    child.starts_with(&prefix)
}

pub fn default_engine_capabilities() -> EngineCapabilities {
    EngineCapabilities {
        supported_modes: vec![TransferMode::Copy, TransferMode::Mirror, TransferMode::Sync],
        supported_verify_modes: vec![
            VerifyMode::None,
            VerifyMode::Size,
            VerifyMode::FullHash,
            VerifyMode::ReadAfterWrite,
        ],
        supported_metadata_modes: vec![
            MetadataMode::DataOnly,
            MetadataMode::Timestamps,
            MetadataMode::Attributes,
            MetadataMode::All,
        ],
        supported_backend_modes: vec![BackendMode::Auto, BackendMode::ThreadPool],
        default_backend_mode: BackendMode::Auto,
        default_thread_count: 4,
        max_thread_count: 64,
        default_buffer_size_bytes: 1_048_576,
        max_buffer_size_bytes: 67_108_864,
        supports_resume: false,
        supports_manifest: false,
        supports_security_metadata: false,
        supports_direct_io: false,
    }
}

fn default_settings() -> AppSettings {
    AppSettings {
        default_verify_mode: VerifyMode::Size,
        default_metadata_mode: MetadataMode::Timestamps,
        default_backend_mode: BackendMode::Auto,
        default_thread_count: 4,
        default_buffer_size_bytes: 1_048_576,
        delete_policy: DeletePolicy::Review,
        allow_mirror_deletes_without_review: false,
        throttle_progress_millis: 150,
        preserve_window_state: true,
    }
}

fn load_history(path: &Path) -> Vec<JobHistoryEntry> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // Absent is the normal first run, not a problem.
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str::<Vec<JobHistoryEntry>>(&text) {
        Ok(entries) => entries,
        Err(error) => {
            // Never destroy what cannot be read: keep it aside so the operator can look at it.
            let aside = path.with_extension(format!("json.corrupt-{}", now_epoch_seconds()));
            let _ = std::fs::rename(path, &aside);
            crate::diagnostics::log_warn(format!(
                "job history unreadable ({}); kept at {}",
                error,
                aside.display()
            ));
            Vec::new()
        }
    }
}

fn write_history(path: &Path, entries: &[JobHistoryEntry]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(entries)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    // Write beside the target and rename over it, so a crash mid-write cannot leave half a history.
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, body)?;
    std::fs::rename(&temp, path)
}

fn now_epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::types::{JobState, TransferMode};

    fn scratch(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("tallow-app-store-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch dir");
        path
    }

    fn entry(job_id: &str) -> JobHistoryEntry {
        JobHistoryEntry {
            job_id: job_id.to_string(),
            plan_id: format!("plan-{job_id}"),
            source: "/source".to_string(),
            target: "/target".to_string(),
            mode: TransferMode::Copy,
            state: JobState::Completed,
            bytes_copied: 10,
            bytes_total: 10,
            files_copied: 2,
            files_total: 2,
            files_skipped: 0,
            error_count: 0,
            elapsed_seconds: 1.5,
            average_rate_bytes_per_second: 6.6,
            completed_at: "2026-10-09T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn history_survives_a_restart() {
        let dir = scratch("history");
        let path = dir.join("history.json");
        {
            let store = JobStore::with_history(path.clone());
            store.record_history(entry("job-1")).expect("record");
            store.record_history(entry("job-2")).expect("record");
        }
        // A fresh store over the same file is exactly what a restart looks like.
        let reopened = JobStore::with_history(path.clone());
        let listed = reopened.list_history().expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].job_id, "job-2", "newest first, the order the UI renders");
        assert_eq!(listed[0].bytes_copied, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_without_a_history_path_writes_nothing() {
        let store = JobStore::default();
        store.record_history(entry("job-1")).expect("record");
        assert_eq!(store.list_history().expect("list").len(), 1);
        assert!(store.history_path.is_none());
    }

    #[test]
    fn an_unreadable_history_file_is_kept_aside_rather_than_destroyed() {
        let dir = scratch("corrupt");
        let path = dir.join("history.json");
        std::fs::write(&path, "{ not json at all").expect("write");
        let store = JobStore::with_history(path.clone());
        assert!(store.list_history().expect("list").is_empty());
        let kept = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|item| item.ok())
            .any(|item| item.file_name().to_string_lossy().contains("corrupt"));
        assert!(kept, "an unreadable history must be kept aside, not deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_is_capped_so_the_file_cannot_grow_without_bound() {
        let store = JobStore::with_history(scratch("cap").join("history.json"));
        for index in 0..(HISTORY_LIMIT + 5) {
            store.record_history(entry(&format!("job-{index}"))).expect("record");
        }
        let listed = store.list_history().expect("list");
        assert_eq!(listed.len(), HISTORY_LIMIT);
        assert_eq!(listed[0].job_id, format!("job-{}", HISTORY_LIMIT + 4));
    }

    #[test]
    fn history_lives_beside_the_logs() {
        // Support asks for one folder, so the history file has to sit in it rather than somewhere else.
        let history = history_path();
        assert_eq!(history.file_name().expect("file name"), "history.json");
        assert_eq!(
            history.parent().expect("parent"),
            crate::diagnostics::diagnostics_dir()
                .parent()
                .expect("logs parent")
        );
    }

    #[test]
    fn a_running_job_is_not_filed_as_history() {
        let store = JobStore::with_history(scratch("running").join("history.json"));
        let mut running = entry("job-1");
        running.state = JobState::Running;
        store.record_history(running).expect("tolerated");
        assert!(store.list_history().expect("list").is_empty());
    }
}
