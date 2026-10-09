pub mod commands;
pub mod diagnostics;
pub mod jobs;

#[cfg(not(test))]
use commands::{
    audit_transfer, cancel_job, execute_job, get_engine_capabilities, get_job, get_settings,
    list_job_history,
    list_jobs, pause_job, plan_job, record_job_history, resume_job, select_path, update_settings,
};
#[cfg(not(test))]
use diagnostics::{get_diagnostics_log_path, reveal_diagnostics_log, write_diagnostic_log};
#[cfg(not(test))]
use jobs::store::JobStore;

#[cfg(not(test))]
#[tauri::command]
fn app_status() -> String {
    "Tallow Copy bridge is ready".to_string()
}

#[cfg(not(test))]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    diagnostics::log_info("starting Tallow Copy application");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(JobStore::with_history(crate::jobs::store::history_path()))
        .invoke_handler(tauri::generate_handler![
            app_status,
            get_engine_capabilities,
            select_path,
            plan_job,
            execute_job,
            pause_job,
            resume_job,
            cancel_job,
            list_jobs,
            get_job,
            list_job_history,
            record_job_history,
            audit_transfer,
            get_settings,
            update_settings,
            get_diagnostics_log_path,
            write_diagnostic_log,
            reveal_diagnostics_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tallow Copy");
}

#[cfg(test)]
pub fn run() {}
