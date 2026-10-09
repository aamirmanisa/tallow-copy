import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type {
  AppSettings,
  AppSettingsPatch,
  CommandError,
  EngineCapabilities,
  JobEventName,
  JobFileEvent,
  JobHistoryEntry,
  JobPlan,
  JobProgress,
  JobRecord,
  JobRequest,
  JobRuntimeErrorEvent,
  JobSummary,
  PathSelectionKind,
  TreeAuditReport,
  VerifyMode,
} from "../types/jobs";

type InvokeArgs = Record<string, unknown>;
type Unlisten = () => void;

export const JOB_EVENTS = {
  started: "job:started",
  progress: "job:progress",
  fileStarted: "job:file-started",
  fileFinished: "job:file-finished",
  error: "job:error",
  paused: "job:paused",
  completed: "job:completed",
  cancelled: "job:cancelled",
} as const satisfies Record<string, JobEventName>;

export interface JobEventHandlers {
  started?: (payload: JobRecord) => void;
  progress?: (payload: JobProgress) => void;
  fileStarted?: (payload: JobFileEvent) => void;
  fileFinished?: (payload: JobFileEvent) => void;
  error?: (payload: JobRuntimeErrorEvent) => void;
  paused?: (payload: JobProgress) => void;
  completed?: (payload: JobSummary) => void;
  cancelled?: (payload: JobProgress) => void;
}

function isCommandError(value: unknown): value is CommandError {
  return (
    typeof value === "object" &&
    value !== null &&
    "code" in value &&
    "message" in value &&
    typeof (value as CommandError).code === "string" &&
    typeof (value as CommandError).message === "string"
  );
}

export function toCommandError(error: unknown): CommandError {
  if (isCommandError(error)) {
    return error;
  }

  if (error instanceof Error) {
    return {
      code: "tauri_invoke_failed",
      message: error.message,
    };
  }

  return {
    code: "tauri_invoke_failed",
    message:
      typeof error === "string"
        ? error
        : "Tauri command invocation failed. Run this app under Tauri for backend commands.",
  };
}

async function invokeCommand<T>(command: string, args?: InvokeArgs): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toCommandError(error);
  }
}

function stringifyDiagnosticContext(context: unknown): string | null {
  if (context === undefined) {
    return null;
  }

  try {
    return JSON.stringify(context, (_key, value) =>
      typeof value === "bigint" ? value.toString() : value,
    );
  } catch {
    return JSON.stringify({ serializationError: "diagnostic context could not be serialized" });
  }
}

export function getEngineCapabilities(): Promise<EngineCapabilities> {
  return invokeCommand("get_engine_capabilities");
}

export function selectPath(kind: PathSelectionKind): Promise<string | null> {
  return invokeCommand("select_path", { kind });
}

export function planJob(request: JobRequest): Promise<JobPlan> {
  return invokeCommand("plan_job", { request });
}

export function auditTransfer(
  source: string,
  target: string,
  verifyMode: VerifyMode,
): Promise<TreeAuditReport> {
  return invokeCommand("audit_transfer", { source, target, verifyMode });
}

export function listJobHistory(): Promise<JobHistoryEntry[]> {
  return invokeCommand("list_job_history");
}

export function recordJobHistory(entry: JobHistoryEntry): Promise<void> {
  return invokeCommand("record_job_history", { entry });
}

export function executeJob(jobId: string): Promise<JobRecord> {
  return invokeCommand("execute_job", { jobId });
}

export function pauseJob(jobId: string): Promise<JobRecord> {
  return invokeCommand("pause_job", { jobId });
}

export function resumeJob(jobId: string): Promise<JobRecord> {
  return invokeCommand("resume_job", { jobId });
}

export function cancelJob(jobId: string): Promise<JobRecord> {
  return invokeCommand("cancel_job", { jobId });
}

export function getDiagnosticsLogPath(): Promise<string> {
  return invokeCommand("get_diagnostics_log_path");
}

export async function writeDiagnosticLog(
  level: "info" | "warn" | "error",
  message: string,
  context?: unknown,
): Promise<void> {
  const safeContext = stringifyDiagnosticContext(context);
  await invokeCommand("write_diagnostic_log", {
    entry: {
      level,
      message,
      context: safeContext,
    },
  });
}

export function revealDiagnosticsLog(): Promise<void> {
  return invokeCommand("reveal_diagnostics_log");
}

export function listJobs(): Promise<JobRecord[]> {
  return invokeCommand("list_jobs");
}

export function getJob(jobId: string): Promise<JobRecord> {
  return invokeCommand("get_job", { jobId });
}

export function getSettings(): Promise<AppSettings> {
  return invokeCommand("get_settings");
}

export function updateSettings(patch: AppSettingsPatch): Promise<AppSettings> {
  return invokeCommand("update_settings", { patch });
}

export async function subscribeToJobEvents(handlers: JobEventHandlers): Promise<Unlisten> {
  const unlisten: Unlisten[] = [];

  try {
    unlisten.push(
      await listen<JobRecord>(JOB_EVENTS.started, (event) => handlers.started?.(event.payload)),
    );
    unlisten.push(
      await listen<JobProgress>(JOB_EVENTS.progress, (event) => handlers.progress?.(event.payload)),
    );
    unlisten.push(
      await listen<JobFileEvent>(JOB_EVENTS.fileStarted, (event) =>
        handlers.fileStarted?.(event.payload),
      ),
    );
    unlisten.push(
      await listen<JobFileEvent>(JOB_EVENTS.fileFinished, (event) =>
        handlers.fileFinished?.(event.payload),
      ),
    );
    unlisten.push(
      await listen<JobRuntimeErrorEvent>(JOB_EVENTS.error, (event) =>
        handlers.error?.(event.payload),
      ),
    );
    unlisten.push(
      await listen<JobProgress>(JOB_EVENTS.paused, (event) => handlers.paused?.(event.payload)),
    );
    unlisten.push(
      await listen<JobSummary>(JOB_EVENTS.completed, (event) =>
        handlers.completed?.(event.payload),
      ),
    );
    unlisten.push(
      await listen<JobProgress>(JOB_EVENTS.cancelled, (event) =>
        handlers.cancelled?.(event.payload),
      ),
    );
  } catch {
    unlisten.forEach((dispose) => dispose());
    return () => {};
  }

  return () => {
    unlisten.forEach((dispose) => dispose());
  };
}
