export type TransferMode = "copy" | "mirror" | "sync";

export type VerifyMode =
  | "none"
  | "size"
  | "full_hash"
  | "read_after_write";

export type MetadataMode =
  | "data_only"
  | "timestamps"
  | "attributes"
  | "all";

export type BackendMode = "auto" | "thread_pool";

export type DeletePolicy = "never" | "review" | "allow";

export type PathSelectionKind = "source" | "target";

export type OperationKind =
  | "copy"
  | "update"
  | "delete"
  | "skip"
  | "verify"
  | "conflict"
  | "error_risk"
  | "metadata_only";

export type JobPhase =
  | "planning"
  | "planned"
  | "copying"
  | "verifying"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export type JobState =
  | "planned"
  | "running"
  | "paused"
  | "completed"
  | "failed"
  | "cancelled";

export type ErrorCategory =
  | "permission_denied"
  | "locked_file"
  | "path_too_long"
  | "destination_full"
  | "network_interrupted"
  | "verification_mismatch"
  | "metadata_failed"
  | "symlink_policy_blocked"
  | "delete_requires_review"
  | "unknown";

export type RecommendedAction =
  | "retry"
  | "skip"
  | "reveal"
  | "change_setting"
  | "review_details"
  | "free_space"
  | "reconnect";

export interface JobFilters {
  includeGlobs: string[];
  excludeGlobs: string[];
  includeHidden: boolean;
  followSymlinks: boolean;
  maxDepth?: number | null;
}

export interface JobRequest {
  source: string;
  target: string;
  mode: TransferMode;
  filters: JobFilters;
  verifyMode: VerifyMode;
  metadataMode: MetadataMode;
  backendMode: BackendMode;
  threadCount: number;
  bufferSizeBytes: number;
  deletePolicy: DeletePolicy;
  /** Continue interrupted files from their partial siblings. Copy mode only. */
  resume: boolean;
  /** Stop the whole transfer at the first error instead of recording it and continuing. */
  stopOnError: boolean;
  /** The manifest a `manifest` verification job is checked against. That mode is refused without one. */
  manifestPath?: string | null;
}

export interface PlannedOperation {
  kind: OperationKind;
  source?: string | null;
  target?: string | null;
  bytes: number;
  reason: string;
}

export interface PlanTotals {
  bytes: number;
  files: number;
  directories: number;
  copies: number;
  updates: number;
  deletes: number;
  verifies: number;
  skips: number;
  conflicts: number;
}

export interface RiskSummary {
  destructive: boolean;
  requiresReview: boolean;
  deleteCount: number;
  conflictCount: number;
  lockedFileCount: number;
  estimatedErrorCount: number;
  warnings: string[];
}

export interface JobPlan {
  planId: string;
  request: JobRequest;
  /** The worker count the engine will actually use, after derivation (0 in the request means derive). */
  effectiveThreads: number;
  /** Where bundling small files would be faster than copying them one by one. */
  bundlingHint?: string | null;
  operations: PlannedOperation[];
  totals: PlanTotals;
  riskSummary: RiskSummary;
}

export interface JobRecord {
  jobId: string;
  planId: string;
  state: JobState;
  plan: JobPlan;
}

export interface ActiveFile {
  path: string;
  bytesCopied: number;
  bytesTotal: number;
  operation: OperationKind;
}

export interface JobProgress {
  jobId: string;
  planId: string;
  phase: JobPhase;
  backend: BackendMode;
  bytesCopied: number;
  bytesTotal: number;
  filesCopied: number;
  filesTotal: number;
  filesSkipped: number;
  errorCount: number;
  rateBytesPerSecond: number;
  etaSeconds?: number | null;
  activeFiles: ActiveFile[];
}

export interface JobSummary {
  jobId: string;
  planId: string;
  source: string;
  target: string;
  mode: TransferMode;
  phase: JobPhase;
  verifyMode: VerifyMode;
  bytesCopied: number;
  bytesTotal: number;
  filesCopied: number;
  filesSkipped: number;
  errorCount: number;
  elapsedSeconds: number;
  averageRateBytesPerSecond: number;
  manifestPath?: string | null;
}

export interface TreeAuditReport {
  source: string;
  target: string;
  /** The comparison actually made: "size", "hash" or "hash-all". */
  verify: string;
  /** Nothing to copy and nothing to delete - NOT "the trees are identical", which a size comparison cannot establish. */
  clean: boolean;
  differences: number;
  matchingFiles: number;
  matchingBytes: number;
  differingFiles: number;
  differingBytes: number;
  extraFiles: number;
  extraBytes: number;
  errorFiles: number;
  differing: string[];
  extra: string[];
  problems: string[];
}

export interface JobHistoryEntry {
  jobId: string;
  planId: string;
  source: string;
  target: string;
  mode: TransferMode;
  status: Extract<JobState, "completed" | "failed" | "cancelled">;
  bytesCopied: number;
  bytesTotal: number;
  filesCopied: number;
  filesTotal: number;
  filesSkipped: number;
  errorCount: number;
  elapsedSeconds: number;
  averageRateBytesPerSecond: number;
  completedAt: string;
}

export interface NeedsReviewItem {
  id: string;
  planId: string;
  path: string;
  message: string;
  category: ErrorCategory | OperationKind | "delete_review";
  retryable: boolean;
  recommendedAction: RecommendedAction | "skip" | "reveal";
  source: "runtime" | "plan";
}

export interface JobError {
  path?: string | null;
  category: ErrorCategory;
  message: string;
  retryable: boolean;
  recommendedAction: RecommendedAction;
}

export type JobEventName =
  | "job:started"
  | "job:progress"
  | "job:file-started"
  | "job:file-finished"
  | "job:error"
  | "job:paused"
  | "job:completed"
  | "job:cancelled";

export interface JobFileEvent {
  jobId: string;
  planId: string;
  file: ActiveFile;
}

export interface JobRuntimeErrorEvent {
  jobId: string;
  planId: string;
  error: JobError;
}

export interface EngineCapabilities {
  supportedModes: TransferMode[];
  supportedVerifyModes: VerifyMode[];
  supportedMetadataModes: MetadataMode[];
  supportedBackendModes: BackendMode[];
  defaultBackendMode: BackendMode;
  defaultThreadCount: number;
  maxThreadCount: number;
  defaultBufferSizeBytes: number;
  maxBufferSizeBytes: number;
  supportsResume: boolean;
  supportsManifest: boolean;
  supportsSecurityMetadata: boolean;
  supportsDirectIo: boolean;
}

export interface AppSettings {
  defaultVerifyMode: VerifyMode;
  defaultMetadataMode: MetadataMode;
  defaultBackendMode: BackendMode;
  defaultThreadCount: number;
  defaultBufferSizeBytes: number;
  deletePolicy: DeletePolicy;
  allowMirrorDeletesWithoutReview: boolean;
  throttleProgressMillis: number;
  preserveWindowState: boolean;
}

export type AppSettingsPatch = Partial<AppSettings>;

export interface CommandError {
  code: string;
  message: string;
  issues?: ValidationIssue[];
}

export interface ValidationIssue {
  code: string;
  field?: string | null;
  message: string;
  destructive: boolean;
  requiresReview: boolean;
}
