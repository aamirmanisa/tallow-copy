import {
  canExecuteJobState,
  jobStore,
  planMatchesCurrentRequest,
  type AppView,
  type JobStoreState,
} from "../state/jobStore";
import type {
  ActiveFile,
  BackendMode,
  JobHistoryEntry,
  MetadataMode,
  NeedsReviewItem,
  PlannedOperation,
  TransferMode,
} from "../types/jobs";
import { icon } from "./icons";
import { bindPlanReviewInteractions, renderPlanReview } from "./PlanReview";
import { bindScriptPresetsInteractions, renderScriptPresetsView } from "./ScriptPresetsView";

type FileRow = {
  id: string;
  type: string;
  typeClass: string;
  name: string;
  size: string;
  modified: string;
  status: string;
  statusTone: "queued" | "active" | "done" | "error";
  selected?: boolean;
};

type Setting =
  | { id: string; label: string; kind: "switch"; enabled: boolean }
  | { id: string; label: string; kind: "select"; value: string; options: string[] }
  | { id: string; label: string; kind: "text"; value: string; placeholder: string };

type SidebarItem = {
  id: string;
  label: string;
  icon: Parameters<typeof icon>[0];
  count?: string;
  active?: boolean;
};

type SidebarSection = {
  label: string;
  items: SidebarItem[];
  separated?: boolean;
};

type ToolbarAction = {
  id: "execute" | "cancel" | "plan" | "verify";
  label: string;
  icon: Parameters<typeof icon>[0];
  variant?: "primary" | "danger";
};

const sidebarSections: SidebarSection[] = [
  {
    label: "Jobs",
    items: [
      { id: "active", label: "Active", icon: "activity", active: true },
      { id: "plans", label: "Plan", icon: "clock" },
    ],
  },
  {
    label: "Reports",
    items: [
      { id: "history", label: "History", icon: "check" },
      { id: "needs-review", label: "Needs review", icon: "history" },
    ],
  },
  {
    label: "Templates",
    separated: true,
    items: [{ id: "script-presets", label: "Transfer Presets", icon: "script" }],
  },
];

const toolbarActionGroups = [
  {
    id: "job-controls",
    actions: [
      { id: "execute", label: "Execute", icon: "arrowRight", variant: "primary" },
      { id: "cancel", label: "Cancel", icon: "cancel", variant: "danger" },
    ],
  },
  {
    id: "planning",
    actions: [
      { id: "plan", label: "Plan", icon: "plan" },
      { id: "verify", label: "Verify", icon: "check" },
    ],
  },
] satisfies Array<{ id: "job-controls" | "planning"; actions: ToolbarAction[] }>;

const toolbarModes: Array<{ id: TransferMode; label: string }> = [
  { id: "copy", label: "Copy" },
  { id: "mirror", label: "Mirror" },
  { id: "sync", label: "Sync" },
];

const settings: Setting[] = [
  { id: "hash", label: "Full hash verify", kind: "switch", enabled: true },
  { id: "deletes", label: "Mirror deletes", kind: "switch", enabled: false },
  { id: "resume", label: "Resume partial files", kind: "switch", enabled: false },
  { id: "stopOnError", label: "Stop on error", kind: "switch", enabled: false },
  {
    id: "manifestPath",
    label: "Manifest path",
    kind: "text",
    value: "",
    placeholder: "Used by manifest verify",
  },
  {
    id: "buffer",
    label: "Buffer",
    kind: "select",
    value: "32 MB",
    options: ["8 MB", "32 MB", "128 MB"],
  },
  { id: "threads", label: "Threads", kind: "select", value: "auto", options: ["auto", "8", "16", "32"] },
  { id: "backend", label: "Backend", kind: "select", value: "Auto", options: ["Auto", "Thread Pool"] },
  {
    id: "metadata",
    label: "Metadata",
    kind: "select",
    value: "Timestamps",
    options: ["Data only", "Timestamps", "Attributes", "All"],
  },
];

const emptySpeedBars = Array.from({ length: 27 }, () => 8);

type DisplayStat = {
  label: string;
  value: string;
  unit?: string;
  tone?: "accent" | "blue" | "orange";
};

declare global {
  interface Window {
    __tallowCopyVisualTest?: {
      seedState: (patch: Partial<JobStoreState>) => void;
      getState: () => JobStoreState;
    };
  }
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function formatBytes(bytes: number): string {
  if (bytes <= 0) {
    return "0 B";
  }

  const units = ["B", "KB", "MB", "GB", "TB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value >= 10 ? value.toFixed(0) : value.toFixed(1)} ${units[exponent]}`;
}

function formatRate(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

function formatDuration(seconds: number): string {
  if (seconds <= 0) {
    return "0s";
  }

  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  if (minutes <= 0) {
    return `${remainingSeconds}s`;
  }

  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  if (hours <= 0) {
    return `${minutes}m ${remainingSeconds}s`;
  }

  return `${hours}h ${remainingMinutes}m`;
}

function filenameFromPath(path: string): string {
  return path.split(/[\\/]+/).filter(Boolean).pop() ?? path;
}

function fileTypeFromName(name: string): { type: string; typeClass: string } {
  const extension = name.includes(".") ? name.split(".").pop()?.toUpperCase() ?? "FILE" : "DIR";

  if (["RAW", "DNG", "AVIF", "JPG", "PNG"].includes(extension)) {
    return { type: extension.slice(0, 3), typeClass: "ft-image" };
  }
  if (["MOV", "R3D", "MP4"].includes(extension)) {
    return { type: extension, typeClass: "ft-video" };
  }
  if (["JSON", "SHA256", "LOG"].includes(extension)) {
    return { type: "LOG", typeClass: "ft-doc" };
  }
  if (["TALLOW", "TL"].includes(extension)) {
    return { type: "TL", typeClass: "ft-code" };
  }
  if (["WAV", "AIF", "AIFF"].includes(extension)) {
    return { type: "AUD", typeClass: "ft-archive" };
  }

  return { type: extension.slice(0, 4), typeClass: "ft-archive" };
}

function activeFileToRow(file: ActiveFile, index: number): FileRow {
  const name = filenameFromPath(file.path);
  const type = fileTypeFromName(name);
  const percent =
    file.bytesTotal > 0 ? Math.min(100, Math.round((file.bytesCopied / file.bytesTotal) * 100)) : 0;

  return {
    id: `active-${index}-${file.path}`,
    name,
    size: formatBytes(file.bytesTotal),
    modified: "Active",
    status: `${percent}%`,
    statusTone: "active",
    selected: index === 0,
    ...type,
  };
}

function finishedFileToRow(file: ActiveFile, index: number): FileRow {
  const name = filenameFromPath(file.path);
  return {
    id: `finished-${index}-${file.path}`,
    name,
    size: formatBytes(file.bytesTotal),
    modified: "Just now",
    status: "Done",
    statusTone: "done",
    ...fileTypeFromName(name),
  };
}

function operationToRow(operation: PlannedOperation, index: number): FileRow {
  const path = operation.target ?? operation.source ?? operation.reason;
  const name = filenameFromPath(path);
  const type = fileTypeFromName(name);
  const status =
    operation.kind === "skip"
      ? "Skip"
      : operation.kind === "verify"
        ? "Verify"
        : operation.kind === "delete"
          ? "Delete"
          : operation.kind === "metadata_only"
            ? "Metadata"
            : "Queued";
  const statusTone = operation.kind === "delete" ? "error" : operation.kind === "skip" ? "done" : "queued";

  return {
    id: `planned-${index}-${path}`,
    name,
    size: formatBytes(operation.bytes),
    modified: "Planned",
    status,
    statusTone,
    ...type,
  };
}

function fileRowsForState(state: JobStoreState): FileRow[] {
  if (!state.currentProgress && state.activeFiles.length === 0) {
    return state.currentPlan?.operations.slice(0, 10).map(operationToRow) ?? [];
  }

  const activeRows = state.activeFiles.map(activeFileToRow);
  const finishedRows = state.recentlyFinishedFiles.map(finishedFileToRow);

  return [...activeRows, ...finishedRows].slice(0, 10);
}

function progressPercent(state: JobStoreState): number {
  if (state.currentProgress && state.currentProgress.bytesTotal > 0) {
    return Math.min(
      100,
      Math.round((state.currentProgress.bytesCopied / state.currentProgress.bytesTotal) * 100),
    );
  }

  if (state.currentJob?.state === "running") {
    return 1;
  }

  if (state.currentJob?.state === "completed") {
    return 100;
  }

  return 0;
}

function statusText(state: JobStoreState): string {
  if (state.loadingCommand) {
    return `${state.loadingCommand[0].toUpperCase()}${state.loadingCommand.slice(1)}...`;
  }

  if (state.errors.length > 0) {
    return "Command error";
  }

  if (jobStore.needsMirrorDeleteReview()) {
    return "Review required";
  }

  if (state.currentPlan && !planMatchesCurrentRequest(state)) {
    return "Plan stale";
  }

  if (state.currentJob) {
    return state.currentJob.state;
  }

  if (state.currentPlan) {
    return "Plan ready";
  }

  return "Ready";
}

function latestError(state: JobStoreState): string | null {
  return state.errors[0]?.message ?? null;
}

function pathFeedbackMessages(state: JobStoreState): Array<{ message: string; tone: "error" | "warning" }> {
  const validationMessages = state.validationIssues
    .filter((issue) => issue.field === "source" || issue.field === "target")
    .map((issue) => ({
      message: issue.message,
      tone: (issue.destructive || issue.requiresReview ? "warning" : "error") as
        | "error"
        | "warning",
    }));

  if (validationMessages.length > 0) {
    return validationMessages.slice(0, 2);
  }

  const error = state.errors[0];
  if (error?.code === "dialog_unavailable" || error?.code === "tauri_invoke_failed") {
    return [{ message: error.message, tone: "warning" }];
  }

  return [];
}

function reviewFeedbackMessage(state: JobStoreState): string | null {
  const reviewIssue = state.validationIssues.find((issue) => issue.requiresReview);
  return reviewIssue?.message ?? null;
}

function renderTitlebar(): string {
  return `
    <header class="titlebar">
      <div class="traffic-lights" aria-hidden="true">
        <span class="traffic-light close"></span>
        <span class="traffic-light minimize"></span>
        <span class="traffic-light maximize"></span>
      </div>
      <div class="titlebar-title">Tallow Copy &mdash; Production Transfer Console</div>
      <div class="titlebar-actions">
        <button class="tb-btn" type="button" aria-label="Create new transfer">+</button>
      </div>
    </header>
  `;
}

function sidebarItemIsActive(item: SidebarItem, state: JobStoreState): boolean {
  if (item.id === "plans") {
    return state.activeView === "planReview";
  }

  if (item.id === "history") {
    return state.activeView === "history";
  }

  if (item.id === "needs-review") {
    return state.activeView === "needsReview";
  }

  if (item.id === "script-presets") {
    return state.activeView === "scriptPresets";
  }

  if (item.id === "active") {
    return state.activeView === "console";
  }

  return Boolean(item.active && state.activeView === "console");
}

function sidebarItemCount(item: SidebarItem, state: JobStoreState): string | undefined {
  if (item.id === "plans" && state.currentPlan) {
    return jobStore.needsMirrorDeleteReview() ? "review" : "1";
  }

  if (item.id === "history") {
    return state.history.length > 0 ? String(state.history.length) : undefined;
  }

  if (item.id === "needs-review") {
    const count = jobStore.getNeedsReviewItems().length;
    return count > 0 ? String(count) : undefined;
  }

  return item.count;
}

function renderSidebarItem(item: SidebarItem, state: JobStoreState): string {
  const isActive = sidebarItemIsActive(item, state);
  const active = isActive ? " active" : "";
  const pressed = isActive ? "true" : "false";
  const itemCount = sidebarItemCount(item, state);
  const count = itemCount
    ? `<span class="sb-count">${escapeHtml(itemCount ?? "")}</span>`
    : "";

  return `
    <button class="sb-item${active}" type="button" data-sidebar-id="${escapeHtml(
      item.id,
    )}" aria-pressed="${pressed}">
      ${icon(item.icon, { className: "sb-icon" })}
      <span>${escapeHtml(item.label)}</span>
      ${count}
    </button>
  `;
}

function renderSidebar(state: JobStoreState): string {
  const sections = sidebarSections
    .map((section) => {
      const divider = section.separated ? '<div class="sb-divider"></div>' : "";
      return `
        ${divider}
        <div class="sb-section">
          <div class="sb-label">${escapeHtml(section.label)}</div>
          ${section.items.map((item) => renderSidebarItem(item, state)).join("")}
        </div>
      `;
    })
    .join("");

  return `
    <nav class="sidebar" aria-label="Tallow Copy navigation">
      ${sections}
      <div class="sb-spacer"></div>
      <div class="sb-footer">
        <div class="sb-disk">
          ${icon("database", { className: "sb-disk-icon" })}
          <div class="sb-disk-info">
            <div class="sb-disk-name">${escapeHtml(state.targetPath ? "Target" : "No target selected")}</div>
            <div class="sb-disk-size">${escapeHtml(state.targetPath || "Choose a target folder")}</div>
          </div>
        </div>
        <div class="sb-disk-bar" aria-hidden="true">
          <div class="sb-disk-fill" style="width:${state.currentPlan ? 100 : 0}%"></div>
        </div>
      </div>
    </nav>
  `;
}

function isActionDisabled(action: ToolbarAction, state: JobStoreState): boolean {
  if (action.id === "execute") {
    return !canExecuteJobState(state);
  }

  if (action.id === "cancel") {
    return !jobStore.canCancel();
  }

  if (action.id === "plan") {
    return state.loadingCommand !== null;
  }

  if (action.id === "verify") {
    // Nothing to compare until both folders are known, and one command at a time.
    return (
      state.loadingCommand !== null ||
      !state.sourcePath.trim() ||
      !state.targetPath.trim()
    );
  }

  return false;
}

function executeBlockedReason(state: JobStoreState): string | null {
  if (canExecuteJobState(state)) {
    return null;
  }

  if (state.loadingCommand) {
    return "A command is already running.";
  }

  if (!state.sourcePath.trim() || !state.targetPath.trim()) {
    return "Choose or paste both source and target folders, then create a plan.";
  }

  if (!state.currentPlan) {
    return "Create a plan before executing.";
  }

  if (!planMatchesCurrentRequest(state)) {
    return "The request changed. Create a new plan before executing.";
  }

  if (jobStore.needsMirrorDeleteReview()) {
    return "Open plan review and accept the mirror delete review before executing.";
  }

  if (state.currentJob) {
    return "A transfer job is already active.";
  }

  return "Execute is waiting for a valid plan.";
}

function actionLabel(action: ToolbarAction, state: JobStoreState): string {
  if (state.loadingCommand === action.id) {
    return `${action.label}...`;
  }

  return action.label;
}

function renderToolbarAction(action: ToolbarAction, state: JobStoreState): string {
  const promotedPlan = action.id === "plan" && !state.currentPlan && !state.loadingCommand;
  const variant = action.variant ? ` ${action.variant}` : promotedPlan ? " primary" : "";
  const disabled = isActionDisabled(action, state);
  const hardDisabled = disabled && action.id !== "execute";
  const loading = state.loadingCommand === action.id ? " loading" : "";
  const title = action.id === "execute" ? executeBlockedReason(state) : null;

  return `
    <button
      class="tool-btn${variant}${loading}"
      type="button"
      data-command-id="${escapeHtml(action.id)}"
      aria-disabled="${disabled ? "true" : "false"}"
      ${hardDisabled ? "disabled" : ""}
      ${title ? `title="${escapeHtml(title)}"` : ""}
    >
      ${icon(action.icon, { size: 13 })}
      <span>${escapeHtml(actionLabel(action, state))}</span>
    </button>
  `;
}

function renderToolbar(state: JobStoreState): string {
  const reviewButton = state.currentPlan
    ? `
      <button
        class="tool-btn"
        type="button"
        data-command-id="open-plan-review"
        aria-pressed="${state.activeView === "planReview" ? "true" : "false"}"
      >
        ${icon("clock", { size: 13 })}
        <span>Review</span>
      </button>
    `
    : "";

  return `
    <div class="toolbar" role="toolbar" aria-label="Transfer controls">
      ${toolbarActionGroups
        .map(
          (group, index) => `
            ${index > 0 ? '<div class="tool-sep" aria-hidden="true"></div>' : ""}
            <div class="tool-group" data-toolbar-group="${group.id}">
              ${group.actions.map((action) => renderToolbarAction(action, state)).join("")}
              ${group.id === "planning" ? reviewButton : ""}
            </div>
          `,
        )
        .join("")}
      <div class="toolbar-spacer"></div>
      <div class="mode-switch" aria-label="Transfer mode">
        ${toolbarModes
          .map(
            (mode) => `
              <button
                class="mode-pill${state.mode === mode.id ? " active" : ""}"
                type="button"
                data-mode-id="${mode.id}"
                aria-label="Set transfer mode to ${escapeHtml(mode.label)}"
                aria-pressed="${state.mode === mode.id ? "true" : "false"}"
              >
                ${escapeHtml(mode.label)}
              </button>
            `,
          )
          .join("")}
      </div>
      <div class="engine-chip">${escapeHtml(state.options.backendMode.toUpperCase())} &middot; ${
        state.options.threadCount === 0
          ? state.currentPlan?.effectiveThreads === undefined
            ? "auto"
            : `auto \u2192 ${state.currentPlan.effectiveThreads}`
          : state.options.threadCount
      } workers</div>
    </div>
  `;
}

function renderPathSide(label: "Source" | "Target", tone: "from" | "to", path: string): string {
  const pathKind = label.toLowerCase();

  return `
    <div class="path-side">
      <span class="path-side-label ${tone}">${escapeHtml(label)}</span>
      <label class="path-display">
        <span class="sr-only">${escapeHtml(label)} folder path</span>
        <input
          class="path-input"
          type="text"
          data-path-input-kind="${escapeHtml(pathKind)}"
          value="${escapeHtml(path)}"
          placeholder="${escapeHtml(`Choose or paste ${label.toLowerCase()} folder`)}"
          spellcheck="false"
        />
      </label>
      <button class="path-browse" type="button" aria-label="Browse ${escapeHtml(
        label.toLowerCase(),
      )} path" data-path-kind="${escapeHtml(pathKind)}">Browse</button>
    </div>
  `;
}

function renderPathBar(state: JobStoreState): string {
  const feedback = pathFeedbackMessages(state);
  return `
    <div class="path-bar">
      ${renderPathSide("Source", "from", state.sourcePath)}
      <div class="path-divider" aria-hidden="true"></div>
      ${renderPathSide("Target", "to", state.targetPath)}
    </div>
    ${
      feedback.length > 0
        ? `<div class="path-feedback" role="status">${feedback
            .map(
              (item) =>
                `<span class="path-feedback-item ${item.tone}">${escapeHtml(item.message)}</span>`,
            )
            .join("")}</div>`
        : ""
    }
  `;
}

function renderFileRow(row: FileRow): string {
  return `
    <div
      class="file-row${row.selected ? " selected" : ""}"
      data-row-id="${escapeHtml(row.id)}"
    >
      <button
        class="row-select"
        type="button"
        aria-label="Select ${escapeHtml(row.name)}"
        aria-pressed="${row.selected ? "true" : "false"}"
      >
        <span class="cell"><span class="file-type-icon ${escapeHtml(
          row.typeClass,
        )}">${escapeHtml(row.type)}</span></span>
        <span class="cell cell-name">${escapeHtml(row.name)}</span>
        <span class="cell cell-size">${row.size === "-" ? "&mdash;" : escapeHtml(
          row.size,
        )}</span>
        <span class="cell cell-modified">${escapeHtml(row.modified)}</span>
        <span class="cell cell-status">
          <span class="status-dot ${row.statusTone}" aria-hidden="true"></span>
          <span>${escapeHtml(row.status)}</span>
        </span>
      </button>
      <div class="cell">
        <button class="row-action" type="button" aria-label="Open actions for ${escapeHtml(
          row.name,
        )}">
          ${icon("more", { size: 12 })}
        </button>
      </div>
    </div>
  `;
}

function renderFileList(state: JobStoreState): string {
  const rows = fileRowsForState(state);
  return `
    <section class="file-list" aria-label="Transfer file list">
      <div class="list-header" role="row">
        <div class="list-h-cell" role="columnheader"></div>
        <button class="list-h-cell sorted" type="button" role="columnheader">Name</button>
        <button class="list-h-cell" type="button" role="columnheader">Size</button>
        <button class="list-h-cell" type="button" role="columnheader">Modified</button>
        <button class="list-h-cell" type="button" role="columnheader">Status</button>
        <div class="list-h-cell" role="columnheader"></div>
      </div>
      ${
        rows.length > 0
          ? rows.map(renderFileRow).join("")
          : `<div class="file-empty">${icon("addFolder", { size: 22 })}<span>No files planned yet.</span></div>`
      }
    </section>
  `;
}

function renderStats(state: JobStoreState): string {
  const progress = state.currentProgress;
  const planned = state.currentPlan;
  const stats: DisplayStat[] = progress
    ? [
        { label: "Speed", value: formatBytes(progress.rateBytesPerSecond), unit: "/s", tone: "accent" },
        {
          label: "ETA",
          value: progress.etaSeconds == null ? "--" : `${Math.ceil(progress.etaSeconds)}`,
          unit: "s",
          tone: "blue",
        },
        { label: "Files", value: `${progress.filesCopied}`, unit: `/ ${progress.filesTotal}` },
        { label: "Errors", value: `${progress.errorCount}`, tone: "orange" },
      ]
      : planned
        ? [
            { label: "Bytes", value: formatBytes(planned.totals.bytes), tone: "accent" },
            { label: "Deletes", value: `${planned.riskSummary.deleteCount}`, tone: "orange" },
            { label: "Files", value: `${planned.totals.files}` },
            { label: "Errors", value: `${planned.riskSummary.estimatedErrorCount}`, tone: "orange" },
          ]
        : [
            { label: "Speed", value: "0 B", unit: "/s", tone: "accent" },
            { label: "ETA", value: "--", tone: "blue" },
            { label: "Files", value: "0" },
            { label: "Errors", value: "0", tone: "orange" },
          ];

  return `
    <div class="stat-grid">
      ${stats
        .map(
          (stat) => `
            <div class="stat-box">
              <div class="stat-label">${escapeHtml(stat.label)}</div>
              <div class="stat-val${stat.tone ? ` ${stat.tone}` : ""}">
                ${escapeHtml(stat.value)}
                ${stat.unit ? `<span class="sm"> ${escapeHtml(stat.unit)}</span>` : ""}
              </div>
            </div>
          `,
        )
        .join("")}
    </div>
    <div class="speed-mini" id="miniGraph" aria-label="Recent transfer speed graph">
      ${(progress ? speedBarsForRate(progress.rateBytesPerSecond) : emptySpeedBars)
        .map((height) => `<span class="speed-bar" style="height:${height}%"></span>`)
        .join("")}
    </div>
  `;
}

function speedBarsForRate(bytesPerSecond: number): number[] {
  const normalized = Math.max(10, Math.min(92, Math.round(bytesPerSecond / 1024 / 1024)));
  return emptySpeedBars.map((_, index) => Math.max(8, Math.min(92, normalized - ((index * 7) % 28))));
}

function queueItemsForState(state: JobStoreState) {
  if (!state.currentProgress) {
    return state.currentPlan?.operations.slice(0, 3).map((operation, index) => {
      const path = operation.target ?? operation.source ?? operation.reason;
      const name = filenameFromPath(path);
      const type = fileTypeFromName(name);
      return {
        type: type.type,
        typeClass: type.typeClass,
        name,
        meta: `${formatBytes(operation.bytes)} - ${operation.kind.replace(/_/g, " ")}`,
        progress: 0,
        id: `queue-${index}-${path}`,
      };
    }) ?? [];
  }

  const active = state.activeFiles.map((file) => {
    const name = filenameFromPath(file.path);
    const type = fileTypeFromName(name);
    return {
      type: type.type,
      typeClass: type.typeClass,
      name,
      meta: `${formatBytes(file.bytesTotal)} - Active`,
      progress:
        file.bytesTotal > 0
          ? Math.min(100, Math.round((file.bytesCopied / file.bytesTotal) * 100))
          : 0,
    };
  });

  const finished = state.recentlyFinishedFiles.slice(0, 2).map((file) => {
    const name = filenameFromPath(file.path);
    const type = fileTypeFromName(name);
    return {
      type: type.type,
      typeClass: type.typeClass,
      name,
      meta: `${formatBytes(file.bytesTotal)} - Done`,
      progress: 100,
    };
  });

  return [...active, ...finished].slice(0, 3);
}

function renderQueue(state: JobStoreState): string {
  const items = queueItemsForState(state);
  if (items.length === 0) {
    return `
      <div class="queue-empty">
        ${icon("addFolder", { size: 18 })}
        <span>Select a source and target, then create a plan.</span>
      </div>
    `;
  }

  return items
    .map(
      (item) => `
        <div class="queue-item">
          <div class="qi-icon ${escapeHtml(item.typeClass)}">${escapeHtml(item.type)}</div>
          <div class="qi-info">
            <div class="qi-name">${escapeHtml(item.name)}</div>
            <div class="qi-meta">${escapeHtml(item.meta).replace(" - ", " &bull; ")}</div>
          </div>
          <div class="qi-progress" aria-hidden="true">
            <div class="qi-fill" style="width:${item.progress}%"></div>
          </div>
        </div>
      `,
    )
    .join("");
}

function settingValue(setting: Setting, state: JobStoreState): string | boolean {
  if (setting.id === "hash") {
    return state.options.verifyMode === "full_hash";
  }
  if (setting.id === "resume") {
    return state.options.resume;
  }
  if (setting.id === "stopOnError") {
    return state.options.stopOnError;
  }
  if (setting.id === "manifestPath") {
    return state.options.manifestPath;
  }
  if (setting.id === "deletes") {
    return state.options.deletePolicy !== "never";
  }
  if (setting.id === "buffer") {
    return String(state.options.bufferSizeBytes);
  }
  if (setting.id === "threads") {
    if (state.options.threadCount === 0) {
      const resolved = state.currentPlan?.effectiveThreads;
      return resolved === undefined ? "Auto" : `Auto (${resolved} workers)`;
    }
    return String(state.options.threadCount);
  }
  if (setting.id === "backend") {
    return state.options.backendMode;
  }
  if (setting.id === "metadata") {
    return state.options.metadataMode;
  }

  return setting.kind === "switch" ? setting.enabled : setting.value;
}

function selectOptions(setting: Setting): Array<{ label: string; value: string }> {
  if (setting.id === "buffer") {
    return [
      { label: "8 MB", value: "8388608" },
      { label: "32 MB", value: "33554432" },
      { label: "128 MB", value: "134217728" },
    ];
  }

  if (setting.id === "backend") {
    return [
      { label: "Auto", value: "auto" },
      { label: "Thread Pool", value: "thread_pool" },
    ];
  }

  if (setting.id === "metadata") {
    return [
      { label: "Data only", value: "data_only" },
      { label: "Timestamps", value: "timestamps" },
      { label: "Attributes", value: "attributes" },
      { label: "All", value: "all" },
    ];
  }

  return setting.kind === "select"
    ? setting.options.map((option) => ({ label: option, value: option }))
    : [];
}

function renderSetting(setting: Setting, state: JobStoreState): string {
  const value = settingValue(setting, state);

  if (setting.kind === "switch") {
    const enabled = value === true;
    return `
      <div class="rp-setting">
        <span class="rp-setting-label" id="setting-${escapeHtml(setting.id)}">${escapeHtml(
          setting.label,
        )}</span>
        <button
          class="switch${enabled ? " on" : ""}"
          type="button"
          data-setting-id="${escapeHtml(setting.id)}"
          aria-label="Toggle ${escapeHtml(setting.label)}"
          aria-labelledby="setting-${escapeHtml(setting.id)}"
          aria-pressed="${enabled ? "true" : "false"}"
        ></button>
      </div>
    `;
  }

  if (setting.kind === "text") {
    return `
      <label class="rp-setting">
        <span class="rp-setting-label">${escapeHtml(setting.label)}</span>
        <input
          class="rp-input"
          type="text"
          value="${escapeHtml(String(value))}"
          placeholder="${escapeHtml(setting.placeholder)}"
          data-setting-id="${escapeHtml(setting.id)}"
          aria-label="${escapeHtml(setting.label)}"
        />
      </label>
    `;
  }

  return `
    <label class="rp-setting">
      <span class="rp-setting-label">${escapeHtml(setting.label)}</span>
      <select class="rp-select" data-setting-id="${escapeHtml(setting.id)}" aria-label="${escapeHtml(
        setting.label,
      )}">
        ${selectOptions(setting)
          .map(
            (option) =>
              `<option value="${escapeHtml(option.value)}"${
                option.value === value ? " selected" : ""
              }>${escapeHtml(option.label)}</option>`,
          )
          .join("")}
      </select>
    </label>
  `;
}

function renderPanelNotice(state: JobStoreState): string {
  const error = latestError(state);
  const reviewError = reviewFeedbackMessage(state);

  if (reviewError) {
    return `<p class="rp-status warning" role="status">${escapeHtml(reviewError)}</p>`;
  }

  if (error) {
    return `<p class="rp-status error" role="status">${escapeHtml(error)}</p>`;
  }

  if (jobStore.needsMirrorDeleteReview()) {
    return `
      <div class="rp-status warning" role="status">
        <span>Mirror deletes need review before Execute.</span>
        <button class="rp-inline-action" type="button" data-command-id="open-plan-review">
          Open plan review
        </button>
      </div>
    `;
  }

  if (state.currentPlan && !planMatchesCurrentRequest(state)) {
    return `
      <div class="rp-status warning" role="status">
        <span>Request changed. Create a new plan before Execute.</span>
        <button class="rp-inline-action" type="button" data-command-id="plan">
          Plan again
        </button>
      </div>
    `;
  }

  if (state.currentPlan) {
    return `
      <div class="rp-status" role="status">
        <span>Plan ${escapeHtml(state.currentPlan.planId)} is ready.</span>
        <button class="rp-inline-action" type="button" data-command-id="open-plan-review">
          Open plan review
        </button>
      </div>
    `;
  }

  if (!state.sourcePath.trim() || !state.targetPath.trim()) {
    return `
      <div class="rp-status" role="status">
        <span>Choose or paste a source and target folder, then press Plan.</span>
      </div>
    `;
  }

  return `
    <div class="rp-status" role="status">
      <span>Create a plan before Execute.</span>
      <button class="rp-inline-action" type="button" data-command-id="plan">
        Plan transfer
      </button>
    </div>
  `;
}

function renderDeveloperLogs(state: JobStoreState): string {
  const path = state.diagnosticsLogPath ?? "Log path will appear after the native app starts.";
  return `
    <div class="developer-log-panel">
      <div class="developer-log-head">
        <span class="developer-log-icon">${icon("settings", { size: 14 })}</span>
        <div>
          <div class="developer-log-title">Runtime trace</div>
          <div class="developer-log-subtitle">Commands, copy events, errors, and throttled progress.</div>
        </div>
      </div>
      <div class="developer-log-path" title="${escapeHtml(path)}">${escapeHtml(path)}</div>
      <button class="tool-btn developer-log-action" type="button" data-command-id="open-diagnostics-log">
        ${icon("folder", { size: 13 })}
        <span>Open logs</span>
      </button>
    </div>
  `;
}

function renderRightPanel(state: JobStoreState): string {
  return `
    <aside class="right-panel" aria-label="Transfer details">
      <section class="rp-section">
        <h2 class="rp-title">Transfer Engine</h2>
        ${renderStats(state)}
      </section>
      <section class="rp-section">
        <h2 class="rp-title">Next Up</h2>
        ${renderQueue(state)}
      </section>
      <section class="rp-section">
        <h2 class="rp-title">Options</h2>
        ${settings.map((setting) => renderSetting(setting, state)).join("")}
        ${renderPanelNotice(state)}
      </section>
      <section class="rp-section">
        <h2 class="rp-title">Developer Logs</h2>
        ${renderDeveloperLogs(state)}
      </section>
    </aside>
  `;
}

function renderStatusbar(state: JobStoreState): string {
  const progress = progressPercent(state);
  const errorText =
    state.errors.length > 0
      ? `${state.errors.length} error${state.errors.length === 1 ? "" : "s"}`
      : `${state.runtimeErrors.length} runtime error${state.runtimeErrors.length === 1 ? "" : "s"}`;
  const speed = state.currentProgress
    ? `${state.currentProgress.backend.toUpperCase()} ${formatRate(state.currentProgress.rateBytesPerSecond)}`
    : `${state.options.backendMode.toUpperCase()} idle`;
  const summary = state.currentPlan
    ? state.currentProgress
      ? `${state.currentProgress.filesCopied.toLocaleString()} / ${state.currentProgress.filesTotal.toLocaleString()} files - ${formatBytes(
          state.currentProgress.bytesCopied,
        )} / ${formatBytes(state.currentProgress.bytesTotal)}`
      : `${state.currentPlan.totals.files.toLocaleString()} files - ${formatBytes(
          state.currentPlan.totals.bytes,
        )}`
    : "No transfer planned";

  return `
    <footer class="statusbar">
      <div class="sb-item-status">
        <span class="dot ${state.errors.length > 0 ? "orange" : "green"}" aria-hidden="true"></span>
        <span>${escapeHtml(statusText(state))}</span>
      </div>
      <div class="sb-progress-wrap" aria-label="Transfer progress ${progress}%">
        <div class="sb-progress" aria-hidden="true">
          <div class="sb-progress-fill" style="width:${progress}%"></div>
        </div>
        <span class="sb-progress-pct">${progress}%</span>
      </div>
      <div class="sb-item-status">
        <span class="dot orange" aria-hidden="true"></span>
        <span>${escapeHtml(errorText)}</span>
      </div>
      <div class="statusbar-spacer"></div>
      <span class="sb-speed">${escapeHtml(speed)}</span>
      <div class="sb-item-status">${escapeHtml(summary).replace(" - ", " &bull; ")}</div>
    </footer>
  `;
}

function historyStatusTone(status: JobHistoryEntry["status"]): string {
  if (status === "completed") {
    return "done";
  }
  if (status === "cancelled") {
    return "queued";
  }

  return "error";
}

function renderHistoryMetric(label: string, value: string): string {
  return `
    <div class="history-metric">
      <span>${escapeHtml(label)}</span>
      <strong>${escapeHtml(value)}</strong>
    </div>
  `;
}

function renderHistoryEntry(entry: JobHistoryEntry): string {
  const completedAt = new Date(entry.completedAt);
  const timestamp = Number.isNaN(completedAt.getTime())
    ? "Recently"
    : completedAt.toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
      });

  return `
    <article class="history-card" aria-labelledby="history-${escapeHtml(entry.jobId)}">
      <div class="history-orb ${historyStatusTone(entry.status)}" aria-hidden="true">
        ${icon(entry.status === "completed" ? "check" : entry.status === "failed" ? "cancel" : "pause", {
          size: 18,
        })}
      </div>
      <div class="history-main">
        <div class="history-head">
          <div>
            <h2 id="history-${escapeHtml(entry.jobId)}">${escapeHtml(entry.planId)}</h2>
            <p>${escapeHtml(entry.source)} <span aria-hidden="true">&rarr;</span> ${escapeHtml(entry.target)}</p>
          </div>
          <span class="history-status ${historyStatusTone(entry.status)}">${escapeHtml(entry.status)}</span>
        </div>
        <div class="history-meta">
          <span>${escapeHtml(entry.mode)}</span>
          <span>${escapeHtml(timestamp)}</span>
        </div>
        <div class="history-metrics">
          ${renderHistoryMetric("Bytes", `${formatBytes(entry.bytesCopied)} / ${formatBytes(entry.bytesTotal)}`)}
          ${renderHistoryMetric("Files", `${entry.filesCopied.toLocaleString()} / ${entry.filesTotal.toLocaleString()}`)}
          ${renderHistoryMetric("Errors", entry.errorCount.toLocaleString())}
          ${renderHistoryMetric("Rate", formatRate(entry.averageRateBytesPerSecond))}
          ${renderHistoryMetric("Elapsed", formatDuration(entry.elapsedSeconds))}
        </div>
      </div>
    </article>
  `;
}

function renderHistoryView(state: JobStoreState): string {
  const entries = state.history;
  return `
    <main class="report-view" aria-label="History">
      <header class="report-header">
        <div>
          <span class="pr-kicker">History</span>
          <h1>Completed and recent jobs</h1>
        </div>
        <button class="tool-btn" type="button" data-command-id="return-console">
          ${icon("activity", { size: 13 })}
          <span>Active</span>
        </button>
      </header>
      ${
        entries.length > 0
          ? `<div class="history-list">${entries.map(renderHistoryEntry).join("")}</div>`
          : `
            <section class="report-empty" aria-labelledby="history-empty-title">
              <div class="pr-empty-icon" aria-hidden="true">${icon("history", { size: 24 })}</div>
              <div>
                <h2 id="history-empty-title">No completed jobs yet</h2>
                <p>Completed, failed, and cancelled transfer jobs will appear here for this session.</p>
              </div>
              <button class="tool-btn primary" type="button" data-command-id="return-console">
                ${icon("activity", { size: 13 })}
                <span>Active</span>
              </button>
            </section>
          `
      }
    </main>
  `;
}

function renderReviewItem(item: NeedsReviewItem): string {
  const category = String(item.category).replace(/_/g, " ");
  const command = item.source === "plan" ? "open-plan-review" : "return-console";
  const label = item.source === "plan" ? "Open plan review" : "Back to active job";
  const itemIcon = item.source === "plan" ? "clock" : "activity";

  return `
    <article class="review-card" aria-labelledby="review-${escapeHtml(item.id)}">
      <div class="review-symbol ${item.retryable ? "retryable" : "review"}" aria-hidden="true">
        ${icon(item.retryable ? "refresh" : "history", { size: 17 })}
      </div>
      <div class="review-main">
        <div class="review-head">
          <div>
            <h2 id="review-${escapeHtml(item.id)}">${escapeHtml(category)}</h2>
            <p>${escapeHtml(item.message)}</p>
          </div>
          <span class="review-source">${escapeHtml(item.source)}</span>
        </div>
        <div class="review-path">${escapeHtml(item.path)}</div>
        <div class="review-actions" aria-label="Review actions for ${escapeHtml(item.path)}">
          <button class="tool-btn primary" type="button" data-command-id="${command}">
            ${icon(itemIcon, { size: 13 })}
            <span>${escapeHtml(label)}</span>
          </button>
        </div>
      </div>
    </article>
  `;
}

function renderNeedsReviewView(state: JobStoreState): string {
  const items = jobStore.getNeedsReviewItems();
  return `
    <main class="report-view" aria-label="Needs Review">
      <header class="report-header">
        <div>
          <span class="pr-kicker">Needs Review</span>
          <h1>Recoverable items</h1>
        </div>
        <button class="tool-btn" type="button" data-command-id="return-console">
          ${icon("activity", { size: 13 })}
          <span>Active</span>
        </button>
      </header>
      ${
        state.audit
          ? `<section class="report-audit" aria-label="Last audit">
        <h2>${
          state.audit.clean
            ? "No differences found"
            : `${state.audit.differences} difference(s) found`
        }</h2>
        <p class="audit-summary">Compared with <strong>${escapeHtml(state.audit.verify)}</strong>: ${
          state.audit.matchingFiles
        } matching, ${state.audit.differingFiles} differing, ${state.audit.extraFiles} only on the target${
          state.audit.errorFiles > 0 ? `, ${state.audit.errorFiles} unreadable` : ""
        }.</p>
        ${
          state.audit.verify === "hash-all"
            ? ""
            : `<p class="audit-caveat">A ${escapeHtml(state.audit.verify)} comparison compares size and time, so it cannot prove that two same-size files hold the same bytes. Choose Full hash verify for that.</p>`
        }
        ${
          state.audit.differing.length > 0
            ? `<ul class="audit-list">${state.audit.differing
                .map((path) => `<li>${escapeHtml(path)}</li>`)
                .join("")}</ul>`
            : ""
        }
        ${
          state.audit.extra.length > 0
            ? `<ul class="audit-list">${state.audit.extra
                .map((path) => `<li>only on the target: ${escapeHtml(path)}</li>`)
                .join("")}</ul>`
            : ""
        }
      </section>`
          : ""
      }
      ${
        state.reviewActionFeedback
          ? `<p class="review-feedback" role="status">${escapeHtml(state.reviewActionFeedback)}</p>`
          : ""
      }
      ${
        items.length > 0
          ? `<div class="review-list">${items.map(renderReviewItem).join("")}</div>`
          : `
            <section class="report-empty" aria-labelledby="review-empty-title">
              <div class="pr-empty-icon" aria-hidden="true">${icon("check", { size: 24 })}</div>
              <div>
                <h2 id="review-empty-title">No runtime errors to review</h2>
                <p>Recoverable runtime errors and review-required plan items appear here. Audit findings above are a comparison, not a queue.</p>
              </div>
              <button class="tool-btn primary" type="button" data-command-id="return-console">
                ${icon("activity", { size: 13 })}
                <span>Active</span>
              </button>
            </section>
          `
      }
    </main>
  `;
}

function renderMainView(state: JobStoreState): string {
  if (state.activeView === "planReview") {
    return renderPlanReview(state);
  }
  if (state.activeView === "history") {
    return renderHistoryView(state);
  }
  if (state.activeView === "needsReview") {
    return renderNeedsReviewView(state);
  }
  if (state.activeView === "scriptPresets") {
    return renderScriptPresetsView(state);
  }

  return `
    ${renderPathBar(state)}
    <div class="content">
      ${renderFileList(state)}
      ${renderRightPanel(state)}
    </div>
  `;
}

function handleSettingChange(id: string, value: string | boolean): void {
  if (id === "hash") {
    jobStore.setOption("verifyMode", value ? "full_hash" : "size");
  } else if (id === "deletes") {
    jobStore.setOption("deletePolicy", value ? "review" : "never");
  } else if (id === "buffer") {
    jobStore.setOption("bufferSizeBytes", Number(value));
  } else if (id === "threads") {
    jobStore.setOption("threadCount", value === "auto" ? 0 : Number(value));
  } else if (id === "resume") {
    jobStore.setOption("resume", Boolean(value));
  } else if (id === "stopOnError") {
    jobStore.setOption("stopOnError", Boolean(value));
  } else if (id === "manifestPath") {
    jobStore.setOption("manifestPath", String(value));
  } else if (id === "backend") {
    jobStore.setOption("backendMode", value as BackendMode);
  } else if (id === "metadata") {
    jobStore.setOption("metadataMode", value as MetadataMode);
  }
}

function bindInteractions(root: HTMLElement): () => void {
  let graphInterval: number | undefined;

  root.querySelectorAll<HTMLButtonElement>(".sb-item").forEach((item) => {
    item.addEventListener("click", () => {
      if (item.dataset.sidebarId === "plans") {
        jobStore.openPlanReview();
      } else if (item.dataset.sidebarId === "active") {
        jobStore.returnToConsole();
      } else if (item.dataset.sidebarId === "history") {
        jobStore.openHistory();
      } else if (item.dataset.sidebarId === "needs-review") {
        jobStore.openNeedsReview();
      } else if (item.dataset.sidebarId === "script-presets") {
        jobStore.openScriptPresets();
      }
    });
  });

  root.querySelectorAll<HTMLButtonElement>(".tool-btn, .rp-inline-action, .pr-review-toggle").forEach((button) => {
    button.addEventListener("click", () => {
      if (button.disabled) {
        return;
      }

      const command = button.dataset.commandId;
      if (command === "plan") {
        void jobStore.planCurrentJob();
      } else if (command === "verify") {
        void jobStore.runAudit();
      } else if (command === "execute") {
        void jobStore.executeCurrentPlan();
      } else if (command === "pause") {
        void jobStore.pauseCurrentJob();
      } else if (command === "resume") {
        void jobStore.resumeCurrentJob();
      } else if (command === "cancel") {
        void jobStore.cancelCurrentJob();
      } else if (command === "toggle-delete-review") {
        jobStore.setMirrorDeletesReviewed(!jobStore.getSnapshot().mirrorDeletesReviewed);
      } else if (command === "open-plan-review") {
        jobStore.openPlanReview();
      } else if (command === "return-console") {
        jobStore.returnToConsole();
      } else if (command === "open-diagnostics-log") {
        void jobStore.openDiagnosticsLog();
      }
    });
  });

  const cleanupPlanReview = bindPlanReviewInteractions(root);
  const cleanupScriptPresets = bindScriptPresetsInteractions(root);

  root.querySelectorAll<HTMLButtonElement>(".mode-pill").forEach((mode) => {
    mode.addEventListener("click", () => {
      const modeId = mode.dataset.modeId as TransferMode | undefined;
      if (modeId) {
        jobStore.setMode(modeId);
      }
    });
  });

  root.querySelectorAll<HTMLButtonElement>(".path-browse").forEach((button) => {
    button.addEventListener("click", () => {
      const kind = button.dataset.pathKind;
      if (kind === "source" || kind === "target") {
        void jobStore.selectPath(kind);
      }
    });
  });

  root.querySelectorAll<HTMLInputElement>(".path-input").forEach((input) => {
    const commitPath = () => {
      const kind = input.dataset.pathInputKind;
      const value = input.value.trim();
      if (kind === "source") {
        jobStore.setSourcePath(value);
      } else if (kind === "target") {
        jobStore.setTargetPath(value);
      }
    };

    input.addEventListener("change", commitPath);
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        commitPath();
      }
    });
  });

  root.querySelectorAll<HTMLButtonElement>(".switch").forEach((toggle) => {
    toggle.addEventListener("click", () => {
      const settingId = toggle.dataset.settingId;
      if (settingId) {
        handleSettingChange(settingId, !toggle.classList.contains("on"));
      }
    });
  });

  root.querySelectorAll<HTMLInputElement>(".rp-input").forEach((input) => {
    input.addEventListener("change", () => {
      const settingId = input.dataset.settingId;
      if (settingId) {
        handleSettingChange(settingId, input.value);
      }
    });
  });

  root.querySelectorAll<HTMLSelectElement>(".rp-select").forEach((select) => {
    select.addEventListener("change", () => {
      const settingId = select.dataset.settingId;
      if (settingId) {
        handleSettingChange(settingId, select.value);
      }
    });
  });

  root.querySelectorAll<HTMLButtonElement>(".row-select").forEach((rowSelect) => {
    const row = rowSelect.closest<HTMLElement>(".file-row");
    if (!row) {
      return;
    }

    const selectRow = () => {
      root.querySelectorAll<HTMLElement>(".file-row").forEach((other) => {
        other.classList.remove("selected");
        other
          .querySelector<HTMLButtonElement>(".row-select")
          ?.setAttribute("aria-pressed", "false");
      });
      row.classList.add("selected");
      rowSelect.setAttribute("aria-pressed", "true");
    };

    rowSelect.addEventListener("click", () => {
      selectRow();
    });
  });

  if (!window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    const graphBars = Array.from(root.querySelectorAll<HTMLElement>(".speed-bar"));
    graphInterval = window.setInterval(() => {
      graphBars.forEach((bar) => {
        const nextHeight = 22 + Math.round(Math.random() * 74);
        bar.style.height = `${nextHeight}%`;
      });
    }, 2000);
  }

  return () => {
    cleanupPlanReview();
    cleanupScriptPresets();
    if (graphInterval !== undefined) {
      window.clearInterval(graphInterval);
    }
  };
}

function focusViewEntry(root: HTMLElement, state: JobStoreState): void {
  window.setTimeout(() => {
    let target: HTMLElement | null = null;

    if (state.activeView === "planReview") {
      target =
        root.querySelector<HTMLElement>(
          `[data-plan-review-group="${state.selectedPlanReviewGroup}"]`,
        ) ?? root.querySelector<HTMLElement>('.plan-review [data-command-id="return-console"]');
    } else if (state.activeView === "history") {
      target = root.querySelector<HTMLElement>('.report-view [data-command-id="return-console"]');
    } else if (state.activeView === "needsReview") {
      target =
        root.querySelector<HTMLElement>(".review-card button") ??
        root.querySelector<HTMLElement>('.report-view [data-command-id="return-console"]');
    } else if (state.activeView === "scriptPresets") {
      target =
        root.querySelector<HTMLElement>(`[data-preset-id="${state.selectedScriptPresetId}"]`) ??
        root.querySelector<HTMLElement>('.script-presets-view [data-command-id="return-console"]');
    } else {
      target =
        root.querySelector<HTMLElement>('[data-sidebar-id="active"]') ??
        root.querySelector<HTMLElement>(".path-browse");
    }

    target?.focus({ preventScroll: true });
  });
}

export function createApp(): HTMLElement {
  const container = document.createElement("div");
  container.className = "app-stage";

  let cleanupInteractions = () => {};
  let previousView: AppView | null = null;

  const render = () => {
    const state = jobStore.getSnapshot();
    const viewChanged = previousView !== null && previousView !== state.activeView;
    previousView = state.activeView;

    cleanupInteractions();
    container.innerHTML = `
      <section class="window" aria-label="Tallow Copy premium transfer console">
        ${renderTitlebar()}
        <div class="window-body">
          ${renderSidebar(state)}
          <div class="main-area">
            ${renderToolbar(state)}
            ${renderMainView(state)}
            ${renderStatusbar(state)}
          </div>
        </div>
      </section>
    `;
    cleanupInteractions = bindInteractions(container);
    if (viewChanged) {
      focusViewEntry(container, state);
    }
  };

  const unsubscribe = jobStore.subscribe(render);
  const cleanup = () => {
    cleanupInteractions();
    unsubscribe();
    jobStore.stopEventSubscriptions();
  };

  render();
  void jobStore.startEventSubscriptions();
  window.addEventListener("beforeunload", cleanup, { once: true });

  if (window.location.hostname === "127.0.0.1" || window.location.hostname === "localhost") {
    const testStore = jobStore as unknown as {
      setState: (patch: Partial<JobStoreState>) => void;
      getSnapshot: () => JobStoreState;
    };
    window.__tallowCopyVisualTest = {
      seedState: (patch) => testStore.setState(patch),
      getState: () => testStore.getSnapshot(),
    };
  }

  if (import.meta.hot) {
    import.meta.hot.dispose(cleanup);
  }

  return container;
}
