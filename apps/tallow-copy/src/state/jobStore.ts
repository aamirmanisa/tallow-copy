import {
  cancelJob,
  executeJob,
  getDiagnosticsLogPath,
  getSettings,
  listJobs,
  pauseJob,
  planJob,
  revealDiagnosticsLog,
  resumeJob,
  selectPath,
  subscribeToJobEvents,
  updateSettings,
  writeDiagnosticLog,
  auditTransfer,
  listJobHistory,
  recordJobHistory,
  toCommandError,
} from "../api/tauriClient";
import type { PresetJobRequest } from "../presets/defaultPresets";
import type {
  ActiveFile,
  AppSettings,
  AppSettingsPatch,
  BackendMode,
  CommandError,
  DeletePolicy,
  JobFileEvent,
  JobHistoryEntry,
  TreeAuditReport,
  JobPlan,
  JobProgress,
  JobRecord,
  JobRequest,
  JobRuntimeErrorEvent,
  JobState,
  JobSummary,
  MetadataMode,
  NeedsReviewItem,
  PathSelectionKind,
  TransferMode,
  ValidationIssue,
  VerifyMode,
} from "../types/jobs";

export type LoadingCommand =
  | "audit"
  | "plan"
  | "execute"
  | "pause"
  | "resume"
  | "cancel"
  | "list"
  | "settings"
  | "diagnostics"
  | "selectPath";

export type AppView = "console" | "planReview" | "history" | "needsReview" | "scriptPresets";

export type PlanReviewGroupId =
  | "copy"
  | "skip"
  | "verify"
  | "delete"
  | "conflict"
  | "error_risk"
  | "metadata_only";

export interface JobOptions {
  verifyMode: VerifyMode;
  metadataMode: MetadataMode;
  backendMode: BackendMode;
  threadCount: number;
  bufferSizeBytes: number;
  deletePolicy: DeletePolicy;
  includeHidden: boolean;
  followSymlinks: boolean;
  resume: boolean;
  stopOnError: boolean;
  manifestPath: string;
}

export interface JobStoreState {
  sourcePath: string;
  targetPath: string;
  mode: TransferMode;
  options: JobOptions;
  currentPlan: JobPlan | null;
  currentJob: JobRecord | null;
  currentProgress: JobProgress | null;
  activeFiles: ActiveFile[];
  recentlyFinishedFiles: ActiveFile[];
  runtimeErrors: JobRuntimeErrorEvent[];
  history: JobHistoryEntry[];
  /** The last audit of the current pair, if one has been run. */
  audit: TreeAuditReport | null;
  reviewActionFeedback: string | null;
  errors: CommandError[];
  validationIssues: ValidationIssue[];
  loadingCommand: LoadingCommand | null;
  mirrorDeletesReviewed: boolean;
  activeView: AppView;
  selectedPlanReviewGroup: PlanReviewGroupId;
  selectedScriptPresetId: string;
  settings: AppSettings | null;
  jobs: JobRecord[];
  diagnosticsLogPath: string | null;
}

type Listener = (state: JobStoreState) => void;

const defaultOptions: JobOptions = {
  verifyMode: "size",
  metadataMode: "timestamps",
  backendMode: "auto",
  // 0 means derive from the source tree and path class. The engine is measured to need different counts
  // at the two ends (small-file trees want more, large files and CIFS want fewer), so the default is a
  // derivation rather than a constant.
  threadCount: 0,
  bufferSizeBytes: 33_554_432,
  deletePolicy: "never",
  includeHidden: false,
  followSymlinks: false,
  resume: false,
  stopOnError: false,
  manifestPath: "",
};

const initialState: JobStoreState = {
  sourcePath: "",
  targetPath: "",
  mode: "copy",
  options: defaultOptions,
  currentPlan: null,
  currentJob: null,
  currentProgress: null,
  activeFiles: [],
  recentlyFinishedFiles: [],
  runtimeErrors: [],
  history: [],
  audit: null,
  reviewActionFeedback: null,
  errors: [],
  validationIssues: [],
  loadingCommand: null,
  mirrorDeletesReviewed: false,
  activeView: "console",
  selectedPlanReviewGroup: "copy",
  selectedScriptPresetId: "media-mirror",
  settings: null,
  jobs: [],
  diagnosticsLogPath: null,
};

export function jobRequestFromState(state: JobStoreState): JobRequest {
  return {
    source: state.sourcePath,
    target: state.targetPath,
    mode: state.mode,
    filters: {
      includeGlobs: [],
      excludeGlobs: [],
      includeHidden: state.options.includeHidden,
      followSymlinks: state.options.followSymlinks,
      maxDepth: null,
    },
    verifyMode: state.options.verifyMode,
    metadataMode: state.options.metadataMode,
    backendMode: state.options.backendMode,
    threadCount: state.options.threadCount,
    bufferSizeBytes: state.options.bufferSizeBytes,
    deletePolicy: state.options.deletePolicy,
    resume: state.options.resume,
    stopOnError: state.options.stopOnError,
    manifestPath: state.options.manifestPath.trim() || null,
  };
}

export function planMatchesCurrentRequest(state: JobStoreState): boolean {
  return Boolean(
    state.currentPlan &&
      JSON.stringify(state.currentPlan.request) === JSON.stringify(jobRequestFromState(state)),
  );
}

export function canExecuteJobState(state: JobStoreState): boolean {
  return Boolean(
    state.currentPlan &&
      planMatchesCurrentRequest(state) &&
      !state.currentJob &&
      !state.loadingCommand &&
      !planNeedsDeleteReview(state.currentPlan, state.mirrorDeletesReviewed),
  );
}

function commandError(code: string, message: string): CommandError {
  return { code, message };
}

function validationCommandError(issues: ValidationIssue[]): CommandError {
  return {
    code: "request_validation_failed",
    message: "Transfer request needs attention before a plan can be created.",
    issues,
  };
}

function normalizeCommandError(error: unknown): CommandError {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    "message" in error &&
    typeof (error as CommandError).code === "string" &&
    typeof (error as CommandError).message === "string"
  ) {
    return error as CommandError;
  }

  if (error instanceof Error) {
    return commandError("frontend_error", error.message);
  }

  return commandError(
    "frontend_error",
    typeof error === "string" ? error : "Unexpected frontend command error",
  );
}

function planNeedsDeleteReview(plan: JobPlan | null, reviewed: boolean): boolean {
  return Boolean(plan?.riskSummary.requiresReview && !reviewed);
}

function comparablePath(path: string): string {
  let normalized = path.trim().replace(/\//g, "\\");
  while (normalized.length > 3 && normalized.endsWith("\\")) {
    normalized = normalized.slice(0, -1);
  }
  return normalized.toLowerCase();
}

function isDescendantPath(parent: string, child: string): boolean {
  const prefix = parent.endsWith("\\") ? parent : `${parent}\\`;
  return child.startsWith(prefix);
}

function validationIssue(
  code: string,
  field: string | null,
  message: string,
  destructive = false,
  requiresReview = false,
): ValidationIssue {
  return {
    code,
    field,
    message,
    destructive,
    requiresReview,
  };
}

function matchingProgressPhaseForState(state: JobState): JobProgress["phase"] | null {
  if (state === "paused") {
    return "paused";
  }
  if (state === "cancelled") {
    return "cancelled";
  }
  if (state === "completed") {
    return "completed";
  }
  if (state === "failed") {
    return "failed";
  }

  return null;
}

function jobStateForProgressPhase(phase: JobProgress["phase"]): JobState {
  if (phase === "paused") {
    return "paused";
  }
  if (phase === "cancelled") {
    return "cancelled";
  }
  if (phase === "completed") {
    return "completed";
  }
  if (phase === "failed") {
    return "failed";
  }

  return "running";
}

function elapsedSecondsSince(startedAt: number | null): number {
  if (!startedAt) {
    return 0;
  }

  return Math.max(0, Math.round((Date.now() - startedAt) / 1000));
}

function operationPath(operation: { source?: string | null; target?: string | null }): string {
  return operation.source ?? operation.target ?? "Path unavailable";
}

class JobStore {
  private state = initialState;
  private listeners = new Set<Listener>();
  private eventUnsubscribe: (() => void) | null = null;
  private eventSubscribePromise: Promise<void> | null = null;
  private activeJobStartedAt: number | null = null;
  private lastFileEventRenderAt = 0;
  private lastProgressLogAt = 0;

  getSnapshot(): JobStoreState {
    return this.state;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  startEventSubscriptions(): Promise<void> {
    if (this.eventUnsubscribe || this.eventSubscribePromise) {
      return this.eventSubscribePromise ?? Promise.resolve();
    }

    this.eventSubscribePromise = subscribeToJobEvents({
      started: (record) => this.applyJobStarted(record),
      progress: (progress) => this.applyProgress(progress),
      fileStarted: (event) => this.applyFileStarted(event),
      fileFinished: (event) => this.applyFileFinished(event),
      error: (event) => this.applyRuntimeError(event),
      paused: (progress) => this.applyTerminalProgress(progress, "paused"),
      completed: (summary) => this.applyCompleted(summary),
      cancelled: (progress) => this.applyTerminalProgress(progress, "cancelled"),
    }).then((unsubscribe) => {
      this.eventUnsubscribe = unsubscribe;
      this.eventSubscribePromise = null;
      void this.loadDiagnosticsLogPath();
      this.logDiagnostic("info", "frontend event subscriptions ready");
    });

    return this.eventSubscribePromise;
  }

  stopEventSubscriptions(): void {
    this.eventUnsubscribe?.();
    this.eventUnsubscribe = null;
    this.eventSubscribePromise = null;
  }

  setSourcePath(path: string): void {
    this.setState({ sourcePath: path }, true);
  }

  setTargetPath(path: string): void {
    this.setState({ targetPath: path }, true);
  }

  async selectPath(kind: PathSelectionKind): Promise<void> {
    await this.runCommand("selectPath", async () => {
      const selectedPath = await selectPath(kind);
      if (!selectedPath) {
        return;
      }

      if (kind === "source") {
        this.setSourcePath(selectedPath);
      } else {
        this.setTargetPath(selectedPath);
      }
    });
  }

  setMode(mode: TransferMode): void {
    this.setState({ mode }, true);
  }

  setOption<K extends keyof JobOptions>(key: K, value: JobOptions[K]): void {
    this.setState(
      {
        options: {
          ...this.state.options,
          [key]: value,
        },
      },
      true,
    );
  }

  setMirrorDeletesReviewed(reviewed: boolean): void {
    this.setState({ mirrorDeletesReviewed: reviewed });
  }

  openPlanReview(group: PlanReviewGroupId = this.state.selectedPlanReviewGroup): void {
    this.setState({ activeView: "planReview", selectedPlanReviewGroup: group });
  }

  openHistory(): void {
    this.setState({ activeView: "history" });
  }

  openNeedsReview(): void {
    this.setState({ activeView: "needsReview", reviewActionFeedback: null });
  }

  openScriptPresets(): void {
    this.setState({ activeView: "scriptPresets" });
  }

  returnToConsole(): void {
    this.setState({ activeView: "console" });
  }

  setSelectedScriptPreset(presetId: string): void {
    this.setState({ selectedScriptPresetId: presetId });
  }

  applyScriptPreset(presetId: string, request: PresetJobRequest): void {
    this.setState(
      {
        sourcePath: request.source,
        targetPath: request.target,
        mode: request.mode,
        options: {
          verifyMode: request.verifyMode,
          metadataMode: request.metadataMode,
          backendMode: request.backendMode,
          threadCount: request.threadCount,
          bufferSizeBytes: request.bufferSizeBytes,
          deletePolicy: request.deletePolicy,
          resume: this.state.options.resume,
          stopOnError: this.state.options.stopOnError,
          manifestPath: this.state.options.manifestPath,
          includeHidden: request.filters.includeHidden,
          followSymlinks: request.filters.followSymlinks,
        },
        selectedScriptPresetId: presetId,
        reviewActionFeedback: null,
      },
    );
  }

  getNeedsReviewItems(): NeedsReviewItem[] {
    const runtimeItems = this.state.runtimeErrors
      .filter((event) => event.error.retryable || event.error.recommendedAction === "reveal")
      .map((event, index): NeedsReviewItem => ({
        id: `runtime-${event.jobId}-${index}`,
        planId: event.planId,
        path: event.error.path ?? "Runtime error",
        message: event.error.message,
        category: event.error.category,
        retryable: event.error.retryable,
        recommendedAction: event.error.recommendedAction,
        source: "runtime",
      }));

    const planItems = (this.state.currentPlan?.operations ?? [])
      .filter((operation) =>
        operation.kind === "conflict" ||
        operation.kind === "error_risk" ||
        operation.kind === "delete",
      )
      .slice(0, 12)
      .map((operation, index): NeedsReviewItem => ({
        id: `plan-${this.state.currentPlan!.planId}-${index}`,
        planId: this.state.currentPlan!.planId,
        path: operationPath(operation),
        message: operation.reason,
        category: operation.kind,
        retryable: operation.kind !== "delete",
        recommendedAction: operation.kind === "delete" ? "review_details" : "retry",
        source: "plan",
      }));

    if (this.state.currentPlan?.riskSummary.requiresReview) {
      planItems.unshift({
        id: `plan-${this.state.currentPlan.planId}-delete-review`,
        planId: this.state.currentPlan.planId,
        path: this.state.currentPlan.request.target,
        message: "Mirror delete plan requires review before execution.",
        category: "delete_review",
        retryable: false,
        recommendedAction: "review_details",
        source: "plan",
      });
    }

    return [...runtimeItems, ...planItems].slice(0, 20);
  }

  setSelectedPlanReviewGroup(group: PlanReviewGroupId): void {
    this.setState({ selectedPlanReviewGroup: group });
  }

  buildJobRequest(): JobRequest {
    return jobRequestFromState(this.state);
  }

  canExecute(): boolean {
    return canExecuteJobState(this.state);
  }

  canPause(): boolean {
    return this.state.currentJob?.state === "running" && !this.state.loadingCommand;
  }

  canResume(): boolean {
    return this.state.currentJob?.state === "paused" && !this.state.loadingCommand;
  }

  canCancel(): boolean {
    const state = this.state.currentJob?.state;
    return Boolean((state === "running" || state === "paused") && !this.state.loadingCommand);
  }

  needsMirrorDeleteReview(): boolean {
    return planNeedsDeleteReview(this.state.currentPlan, this.state.mirrorDeletesReviewed);
  }

  async planCurrentJob(): Promise<void> {
    const validationIssues = this.validateCurrentRequest();
    if (validationIssues.length > 0) {
      this.setState({
        currentPlan: null,
        currentJob: null,
        currentProgress: null,
        activeFiles: [],
        recentlyFinishedFiles: [],
        runtimeErrors: [],
        mirrorDeletesReviewed: false,
        errors: [validationCommandError(validationIssues)],
        validationIssues,
      });
      return;
    }

    await this.runCommand("plan", async () => {
      const plan = await planJob(this.buildJobRequest());
      this.setState({
        currentPlan: plan,
        currentJob: null,
        currentProgress: null,
        activeFiles: [],
        recentlyFinishedFiles: [],
        runtimeErrors: [],
        reviewActionFeedback: null,
        validationIssues: [],
        mirrorDeletesReviewed: false,
      });
    });
  }

  /**
   * Compare the target against the source and report what does not agree. Writes nothing: the engine
   * never calls `execute` on this path, so read-only is a property of the code rather than a promise.
   * The result renders in Needs review, which is where a report belongs.
   */
  async runAudit(): Promise<void> {
    const source = this.state.sourcePath.trim();
    const target = this.state.targetPath.trim();
    if (!source || !target) {
      this.addErrorObject(
        commandError("audit_needs_paths", "Choose both a source and a target folder before verifying."),
      );
      return;
    }
    this.setState({ loadingCommand: "audit", audit: null });
    try {
      const audit = await auditTransfer(source, target, this.state.options.verifyMode);
      this.setState({ audit, loadingCommand: null, activeView: "needsReview" });
    } catch (error: unknown) {
      this.setState({ loadingCommand: null });
      this.addErrorObject(toCommandError(error));
    }
  }

  async executeCurrentPlan(): Promise<void> {
    if (!this.state.currentPlan) {
      const validationIssues = this.validateCurrentRequest();
      if (validationIssues.length > 0) {
        this.setState({
          errors: [validationCommandError(validationIssues)],
          validationIssues,
        });
        return;
      }

      this.addError("missing_plan", "Create a plan before executing this transfer.");
      return;
    }

    if (!planMatchesCurrentRequest(this.state)) {
      this.addError("stale_plan", "Create a new plan before executing the updated transfer request.");
      return;
    }

    if (this.needsMirrorDeleteReview()) {
      this.addError(
        "mirror_delete_review_required",
        "Review mirror deletes before executing this plan.",
      );
      return;
    }

    await this.runCommand("execute", async () => {
      const record = await executeJob(this.state.currentPlan!.planId);
      this.setState({ currentJob: record });
    });
  }

  async pauseCurrentJob(): Promise<void> {
    const jobId = this.state.currentJob?.jobId;
    if (!jobId || !this.canPause()) {
      return;
    }

    await this.runCommand("pause", async () => {
      const record = await pauseJob(jobId);
      this.setState({ currentJob: record });
    });
  }

  async resumeCurrentJob(): Promise<void> {
    const jobId = this.state.currentJob?.jobId;
    if (!jobId || !this.canResume()) {
      return;
    }

    await this.runCommand("resume", async () => {
      const record = await resumeJob(jobId);
      this.setState({ currentJob: record });
    });
  }

  async cancelCurrentJob(): Promise<void> {
    const jobId = this.state.currentJob?.jobId;
    if (!jobId || !this.canCancel()) {
      return;
    }

    await this.runCommand("cancel", async () => {
      const record = await cancelJob(jobId);
      this.setState({ currentJob: record });
    });
  }

  async refreshJobs(): Promise<void> {
    await this.runCommand("list", async () => {
      const jobs = await listJobs();
      this.setState({ jobs });
    });
  }

  async loadSettings(): Promise<void> {
    await this.runCommand("settings", async () => {
      const settings = await getSettings();
      this.setState({ settings });
    });
  }

  async saveSettings(patch: AppSettingsPatch): Promise<void> {
    await this.runCommand("settings", async () => {
      const settings = await updateSettings(patch);
      this.setState({ settings });
    });
  }

  async openDiagnosticsLog(): Promise<void> {
    await this.runCommand("diagnostics", async () => {
      await revealDiagnosticsLog();
    });
  }

  private async runCommand(command: LoadingCommand, action: () => Promise<void>): Promise<void> {
    const startedAt = performance.now();
    this.setState({ loadingCommand: command, errors: [], validationIssues: [] });
    this.logDiagnostic("info", "command start", this.commandContext(command));

    try {
      await action();
      this.logDiagnostic("info", "command complete", {
        ...this.commandContext(command),
        elapsedMs: Math.round(performance.now() - startedAt),
      });
    } catch (error) {
      const normalizedError = normalizeCommandError(error);
      this.logDiagnostic("error", "command failed", {
        ...this.commandContext(command),
        elapsedMs: Math.round(performance.now() - startedAt),
        error: normalizedError,
      });
      this.addErrorObject(normalizedError);
    } finally {
      if (this.state.loadingCommand === command) {
        this.setState({ loadingCommand: null });
      }
    }
  }

  private async loadDiagnosticsLogPath(): Promise<void> {
    try {
      const path = await getDiagnosticsLogPath();
      this.setState({ diagnosticsLogPath: path });
    } catch {
      this.setState({ diagnosticsLogPath: null });
    }
  }

  private commandContext(command: LoadingCommand): Record<string, unknown> {
    return {
      command,
      source: this.state.sourcePath,
      target: this.state.targetPath,
      mode: this.state.mode,
      verifyMode: this.state.options.verifyMode,
      backendMode: this.state.options.backendMode,
      threadCount: this.state.options.threadCount,
      bufferSizeBytes: this.state.options.bufferSizeBytes,
      planId: this.state.currentPlan?.planId ?? null,
      jobId: this.state.currentJob?.jobId ?? null,
    };
  }

  private logDiagnostic(
    level: "info" | "warn" | "error",
    message: string,
    context?: Record<string, unknown>,
  ): void {
    void writeDiagnosticLog(level, message, context).catch(() => {});
  }

  private addError(code: string, message: string): void {
    this.addErrorObject(commandError(code, message));
  }

  private addErrorObject(error: CommandError): void {
    this.setState({ errors: [error], validationIssues: error.issues ?? [] });
  }

  private validateCurrentRequest(): ValidationIssue[] {
    const request = this.buildJobRequest();
    const issues: ValidationIssue[] = [];
    const source = request.source.trim();
    const target = request.target.trim();

    if (!source) {
      issues.push(validationIssue("empty_source", "source", "Choose a source path before planning."));
    }

    if (!target) {
      issues.push(validationIssue("empty_target", "target", "Choose a target path before planning."));
    }

    if (source && target) {
      const sourcePath = comparablePath(source);
      const targetPath = comparablePath(target);

      if (sourcePath === targetPath) {
        issues.push(
          validationIssue("identical_paths", "target", "Source and target must be different paths."),
        );
      } else if (request.mode === "mirror" && isDescendantPath(sourcePath, targetPath)) {
        const destructive = request.deletePolicy !== "never";
        issues.push(
          validationIssue(
            "target_inside_source",
            "target",
            "A mirror target cannot be inside the source tree.",
            destructive,
            destructive,
          ),
        );
      }
    }

    if (request.mode === "mirror" && request.deletePolicy === "allow") {
      issues.push(
        validationIssue(
          "mirror_delete_review_required",
          "deletePolicy",
          "Mirror deletes require review before they can be allowed.",
          true,
          true,
        ),
      );
    }

    return issues;
  }

  private setState(patch: Partial<JobStoreState>, resetPlan = false): void {
    this.state = {
      ...this.state,
      ...patch,
      ...(resetPlan
        ? {
            currentPlan: null,
            currentJob: null,
            currentProgress: null,
            activeFiles: [],
            recentlyFinishedFiles: [],
            runtimeErrors: [],
            mirrorDeletesReviewed: false,
            errors: [],
            validationIssues: [],
          }
        : {}),
    };
    this.emit();
  }

  private applyJobStarted(record: JobRecord): void {
    this.activeJobStartedAt = Date.now();
    this.lastProgressLogAt = 0;
    this.lastFileEventRenderAt = 0;
    this.logDiagnostic("info", "job started", {
      jobId: record.jobId,
      planId: record.planId,
      source: record.plan.request.source,
      target: record.plan.request.target,
      mode: record.plan.request.mode,
      operations: record.plan.operations.length,
      bytes: record.plan.totals.bytes,
    });
    this.setState({
      currentJob: { ...record, state: "running" },
      currentPlan: record.plan,
      currentProgress: null,
      activeFiles: [],
      recentlyFinishedFiles: [],
      runtimeErrors: [],
      reviewActionFeedback: null,
      loadingCommand: null,
    });
  }

  private applyProgress(progress: JobProgress): void {
    if (!this.matchesCurrentJob(progress.jobId, progress.planId)) {
      return;
    }

    const expectedTerminalPhase = this.state.currentJob
      ? matchingProgressPhaseForState(this.state.currentJob.state)
      : null;
    if (expectedTerminalPhase && progress.phase !== expectedTerminalPhase) {
      return;
    }

    const now = Date.now();
    if (now - this.lastProgressLogAt >= 1000 || progress.phase !== "copying") {
      this.lastProgressLogAt = now;
      this.logDiagnostic("info", "job progress", {
        jobId: progress.jobId,
        planId: progress.planId,
        phase: progress.phase,
        bytesCopied: progress.bytesCopied,
        bytesTotal: progress.bytesTotal,
        filesCopied: progress.filesCopied,
        filesTotal: progress.filesTotal,
        rateBytesPerSecond: progress.rateBytesPerSecond,
        errorCount: progress.errorCount,
      });
    }

    const nextJob = this.withCurrentJobState(jobStateForProgressPhase(progress.phase));
    const history =
      progress.phase === "failed" && nextJob
        ? this.withHistoryEntry(this.historyEntryFromProgress(progress, "failed", nextJob))
        : this.state.history;

    this.setState({
      currentProgress: progress,
      activeFiles: progress.activeFiles,
      currentJob: nextJob,
      history,
    });
  }

  private applyFileStarted(event: JobFileEvent): void {
    if (!this.matchesCurrentJob(event.jobId, event.planId) || this.shouldIgnoreRuntimeFileEvent()) {
      return;
    }

    if (!this.shouldRenderFileEvent()) {
      return;
    }

    this.setState({ activeFiles: [event.file] });
  }

  private applyFileFinished(event: JobFileEvent): void {
    if (!this.matchesCurrentJob(event.jobId, event.planId) || this.shouldIgnoreRuntimeFileEvent()) {
      return;
    }

    if (!this.shouldRenderFileEvent()) {
      return;
    }

    this.setState({
      recentlyFinishedFiles: [event.file, ...this.state.recentlyFinishedFiles].slice(0, 4),
      activeFiles: this.state.activeFiles.filter((file) => file.path !== event.file.path),
    });
  }

  private applyRuntimeError(event: JobRuntimeErrorEvent): void {
    if (!this.matchesCurrentJob(event.jobId, event.planId)) {
      return;
    }

    this.logDiagnostic("error", "job runtime error", {
      jobId: event.jobId,
      planId: event.planId,
      error: event.error,
    });

    this.setState({
      runtimeErrors: [event, ...this.state.runtimeErrors].slice(0, 5),
      errors: [
        commandError(
          `job_${event.error.category}`,
          `${event.error.message}${event.error.path ? ` (${event.error.path})` : ""}`,
        ),
      ],
    });
  }

  private applyTerminalProgress(progress: JobProgress, state: Extract<JobState, "paused" | "cancelled">): void {
    if (!this.matchesCurrentJob(progress.jobId, progress.planId)) {
      return;
    }

    const nextJob = this.withCurrentJobState(state);
    const history =
      state === "cancelled" && nextJob
        ? this.withHistoryEntry(this.historyEntryFromProgress(progress, state, nextJob))
        : this.state.history;

    this.logDiagnostic(state === "cancelled" ? "warn" : "info", `job ${state}`, {
      jobId: progress.jobId,
      planId: progress.planId,
      bytesCopied: progress.bytesCopied,
      bytesTotal: progress.bytesTotal,
      filesCopied: progress.filesCopied,
      filesTotal: progress.filesTotal,
      errorCount: progress.errorCount,
    });

    this.setState({
      currentProgress: progress,
      currentJob: nextJob,
      activeFiles: progress.activeFiles,
      history,
      loadingCommand: null,
    });
  }

  private applyCompleted(summary: JobSummary): void {
    if (!this.matchesCurrentJob(summary.jobId, summary.planId)) {
      return;
    }

    this.logDiagnostic(summary.errorCount > 0 ? "warn" : "info", "job completed", {
      jobId: summary.jobId,
      planId: summary.planId,
      source: summary.source,
      target: summary.target,
      mode: summary.mode,
      bytesCopied: summary.bytesCopied,
      bytesTotal: summary.bytesTotal,
      filesCopied: summary.filesCopied,
      filesSkipped: summary.filesSkipped,
      errorCount: summary.errorCount,
      elapsedSeconds: summary.elapsedSeconds,
      averageRateBytesPerSecond: summary.averageRateBytesPerSecond,
    });

    const progress: JobProgress = {
      jobId: summary.jobId,
      planId: summary.planId,
      phase: "completed",
      backend: this.state.currentProgress?.backend ?? this.state.options.backendMode,
      bytesCopied: summary.bytesCopied,
      bytesTotal: summary.bytesTotal,
      filesCopied: summary.filesCopied,
      filesTotal: summary.filesCopied,
      filesSkipped: summary.filesSkipped,
      errorCount: summary.errorCount,
      rateBytesPerSecond: summary.averageRateBytesPerSecond,
      etaSeconds: 0,
      activeFiles: [],
    };

    this.setState({
      currentProgress: progress,
      currentJob: this.withCurrentJobState("completed"),
      activeFiles: [],
      history: this.withHistoryEntry(this.historyEntryFromSummary(summary)),
      loadingCommand: null,
    });
  }

  private historyEntryFromSummary(summary: JobSummary): JobHistoryEntry {
    return {
      jobId: summary.jobId,
      planId: summary.planId,
      source: summary.source,
      target: summary.target,
      mode: summary.mode,
      status: summary.phase === "cancelled" || summary.phase === "failed" ? summary.phase : "completed",
      bytesCopied: summary.bytesCopied,
      bytesTotal: summary.bytesTotal,
      filesCopied: summary.filesCopied,
      filesTotal: summary.filesCopied + summary.filesSkipped,
      filesSkipped: summary.filesSkipped,
      errorCount: summary.errorCount,
      elapsedSeconds: summary.elapsedSeconds,
      averageRateBytesPerSecond: summary.averageRateBytesPerSecond,
      completedAt: new Date().toISOString(),
    };
  }

  private historyEntryFromProgress(
    progress: JobProgress,
    status: Extract<JobState, "failed" | "cancelled">,
    job: JobRecord,
  ): JobHistoryEntry {
    return {
      jobId: progress.jobId,
      planId: progress.planId,
      source: job.plan.request.source,
      target: job.plan.request.target,
      mode: job.plan.request.mode,
      status,
      bytesCopied: progress.bytesCopied,
      bytesTotal: progress.bytesTotal,
      filesCopied: progress.filesCopied,
      filesTotal: progress.filesTotal,
      filesSkipped: progress.filesSkipped,
      errorCount: progress.errorCount,
      elapsedSeconds: elapsedSecondsSince(this.activeJobStartedAt),
      averageRateBytesPerSecond: progress.rateBytesPerSecond,
      completedAt: new Date().toISOString(),
    };
  }

  private withHistoryEntry(entry: JobHistoryEntry): JobHistoryEntry[] {
    // This is the only place a finished job enters history, and only ever with a terminal state, so it is
    // the only place that needs to persist one. Fire and forget: a failed write must not disturb a
    // transfer that has already finished.
    void recordJobHistory(entry).catch((error: unknown) => {
      this.logDiagnostic("warn", "could not record job history", { error: String(error) });
    });
    const withoutDuplicate = this.state.history.filter((item) => item.jobId !== entry.jobId);
    return [entry, ...withoutDuplicate].slice(0, 24);
  }

  /** Load the jobs this install finished before, so REPORTS -> History survives a restart. */
  async hydrateHistory(): Promise<void> {
    try {
      const loaded = await listJobHistory();
      this.setState({ history: loaded.slice(0, 24) });
    } catch (error: unknown) {
      this.logDiagnostic("warn", "could not load job history", { error: String(error) });
    }
  }

  private matchesCurrentJob(jobId: string, planId: string): boolean {
    return (
      this.state.currentJob?.jobId === jobId ||
      this.state.currentPlan?.planId === planId ||
      this.state.currentProgress?.jobId === jobId
    );
  }

  private withCurrentJobState(state: JobState): JobRecord | null {
    if (this.state.currentJob) {
      return { ...this.state.currentJob, state };
    }

    if (this.state.currentPlan) {
      return {
        jobId: this.state.currentPlan.planId,
        planId: this.state.currentPlan.planId,
        state,
        plan: this.state.currentPlan,
      };
    }

    return null;
  }

  private shouldIgnoreRuntimeFileEvent(): boolean {
    const state = this.state.currentJob?.state;
    return state ? matchingProgressPhaseForState(state) !== null : false;
  }

  private shouldRenderFileEvent(): boolean {
    const now = Date.now();
    if (now - this.lastFileEventRenderAt < 100) {
      return false;
    }

    this.lastFileEventRenderAt = now;
    return true;
  }

  private emit(): void {
    this.listeners.forEach((listener) => listener(this.state));
  }
}

export const jobStore = new JobStore();
