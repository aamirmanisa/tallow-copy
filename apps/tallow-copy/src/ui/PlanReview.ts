import {
  canExecuteJobState,
  jobStore,
  type JobStoreState,
  type PlanReviewGroupId,
} from "../state/jobStore";
import type { JobPlan, OperationKind, PlannedOperation } from "../types/jobs";
import { icon } from "./icons";

type PlanReviewGroup = {
  id: PlanReviewGroupId;
  label: string;
  shortLabel: string;
  description: string;
  emptyText: string;
  tone?: "accent" | "blue" | "orange" | "red";
  kinds: OperationKind[];
};

type GroupSummary = PlanReviewGroup & {
  operations: PlannedOperation[];
  count: number;
  bytes: number;
};

const planReviewGroups: PlanReviewGroup[] = [
  {
    id: "copy",
    label: "Copy",
    shortLabel: "Copy",
    description: "Files that will be copied or updated at the target.",
    emptyText: "No copy or update operations are currently planned.",
    tone: "accent",
    kinds: ["copy", "update"],
  },
  {
    id: "skip",
    label: "Skip",
    shortLabel: "Skip",
    description: "Files already aligned with the requested transfer policy.",
    emptyText: "No skip operations are currently planned.",
    kinds: ["skip"],
  },
  {
    id: "verify",
    label: "Verify",
    shortLabel: "Verify",
    description: "Files that will be read back or checked by the selected verify mode.",
    emptyText: "No verify-only operations are currently planned.",
    tone: "blue",
    kinds: ["verify"],
  },
  {
    id: "delete",
    label: "Delete",
    shortLabel: "Delete",
    description: "Target files that mirror mode may remove.",
    emptyText: "No target deletes are currently planned.",
    tone: "orange",
    kinds: ["delete"],
  },
  {
    id: "conflict",
    label: "Conflict",
    shortLabel: "Conflict",
    description: "Path, metadata, or policy conflicts that need attention.",
    emptyText: "No conflicts are currently planned.",
    tone: "orange",
    kinds: ["conflict"],
  },
  {
    id: "error_risk",
    label: "Error risk",
    shortLabel: "Risk",
    description: "Operations likely to encounter locked files, permissions, or IO failures.",
    emptyText: "No error-risk operations are currently planned.",
    tone: "red",
    kinds: ["error_risk"],
  },
  {
    id: "metadata_only",
    label: "Metadata only",
    shortLabel: "Metadata",
    description: "Operations that only update timestamps or basic file attributes.",
    emptyText: "No metadata-only operations are currently planned.",
    kinds: ["metadata_only"],
  },
];

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

function operationPath(operation: PlannedOperation): string {
  return operation.source ?? operation.target ?? "Operation path unavailable";
}

function fallbackCount(plan: JobPlan, groupId: PlanReviewGroupId): number {
  if (groupId === "copy") {
    return plan.totals.copies + plan.totals.updates;
  }
  if (groupId === "skip") {
    return plan.totals.skips;
  }
  if (groupId === "verify") {
    return plan.totals.verifies;
  }
  if (groupId === "delete") {
    return plan.riskSummary.deleteCount || plan.totals.deletes;
  }
  if (groupId === "conflict") {
    return plan.riskSummary.conflictCount || plan.totals.conflicts;
  }
  if (groupId === "error_risk") {
    return plan.riskSummary.estimatedErrorCount + plan.riskSummary.lockedFileCount;
  }

  return 0;
}

function summarizeGroups(plan: JobPlan | null): GroupSummary[] {
  return planReviewGroups.map((group) => {
    const operations =
      plan?.operations.filter((operation) => group.kinds.includes(operation.kind)) ?? [];
    const count = operations.length > 0 ? operations.length : plan ? fallbackCount(plan, group.id) : 0;
    const bytes = operations.reduce((total, operation) => total + operation.bytes, 0);

    return {
      ...group,
      operations,
      count,
      bytes,
    };
  });
}

function selectedGroup(state: JobStoreState, groups: GroupSummary[]): GroupSummary {
  return (
    groups.find((group) => group.id === state.selectedPlanReviewGroup) ??
    groups[0]
  );
}

function renderEmptyPlanState(): string {
  return `
    <section class="plan-review-empty" aria-labelledby="plan-review-empty-title">
      <div class="pr-empty-icon" aria-hidden="true">${icon("plan", { size: 24 })}</div>
      <div>
        <h2 id="plan-review-empty-title">No plan ready</h2>
        <p>Create a transfer plan first, then return here to inspect operation groups and delete risk.</p>
      </div>
      <div class="pr-empty-actions">
        <button class="tool-btn primary" type="button" data-command-id="plan">
          ${icon("plan", { size: 13 })}
          <span>Plan</span>
        </button>
        <button class="tool-btn" type="button" data-command-id="return-console">
          ${icon("activity", { size: 13 })}
          <span>Console</span>
        </button>
      </div>
    </section>
  `;
}

function renderRiskSummary(plan: JobPlan): string {
  const risk = plan.riskSummary;
  const warnings = risk.warnings.length > 0
    ? risk.warnings.map((warning) => `<li>${escapeHtml(warning)}</li>`).join("")
    : "<li>No backend warnings were reported for this plan.</li>";

  return `
    <section class="pr-risk" aria-label="Plan risk summary">
      <div class="pr-risk-grid">
        <div class="pr-risk-item">
          <span class="pr-kicker">Deletes</span>
          <strong>${risk.deleteCount.toLocaleString()}</strong>
        </div>
        <div class="pr-risk-item">
          <span class="pr-kicker">Conflicts</span>
          <strong>${risk.conflictCount.toLocaleString()}</strong>
        </div>
        <div class="pr-risk-item">
          <span class="pr-kicker">Locked</span>
          <strong>${risk.lockedFileCount.toLocaleString()}</strong>
        </div>
        <div class="pr-risk-item">
          <span class="pr-kicker">Errors</span>
          <strong>${risk.estimatedErrorCount.toLocaleString()}</strong>
        </div>
      </div>
      <ul class="pr-warnings">${warnings}</ul>
    </section>
  `;
}

function renderDeleteReview(state: JobStoreState): string {
  const plan = state.currentPlan;
  if (!plan || !plan.riskSummary.requiresReview) {
    return "";
  }

  const reviewed = state.mirrorDeletesReviewed;
  return `
    <section class="pr-delete-review${reviewed ? " accepted" : ""}" aria-label="Mirror delete review">
      <div>
        <span class="pr-kicker">Destructive review</span>
        <p>
          This mirror plan can delete ${plan.riskSummary.deleteCount.toLocaleString()} target item${plan.riskSummary.deleteCount === 1 ? "" : "s"}.
          Execute stays disabled until this review is accepted.
        </p>
      </div>
      <button
        class="pr-review-toggle${reviewed ? " on" : ""}"
        type="button"
        data-command-id="toggle-delete-review"
        aria-pressed="${reviewed ? "true" : "false"}"
      >
        <span class="switch${reviewed ? " on" : ""}" aria-hidden="true"></span>
        <span>${reviewed ? "Delete review accepted" : "Accept delete review"}</span>
      </button>
    </section>
  `;
}

function renderGroupTab(group: GroupSummary, active: boolean): string {
  const tone = group.tone ? ` ${group.tone}` : "";
  return `
    <button
      class="pr-group-tab${active ? " active" : ""}${tone}"
      type="button"
      role="tab"
      id="plan-review-tab-${group.id}"
      aria-selected="${active ? "true" : "false"}"
      aria-controls="plan-review-panel"
      data-plan-review-group="${group.id}"
      tabindex="${active ? "0" : "-1"}"
    >
      <span>${escapeHtml(group.shortLabel)}</span>
      <strong>${group.count.toLocaleString()}</strong>
    </button>
  `;
}

function renderOperation(operation: PlannedOperation): string {
  const target = operation.target && operation.target !== operation.source
    ? `<span class="pr-op-target">${escapeHtml(operation.target)}</span>`
    : "";

  return `
    <li class="pr-operation">
      <span class="pr-op-kind">${escapeHtml(operation.kind.replace(/_/g, " "))}</span>
      <span class="pr-op-main">
        <span class="pr-op-path">${escapeHtml(operationPath(operation))}</span>
        ${target}
        <span class="pr-op-reason">${escapeHtml(operation.reason)}</span>
      </span>
      <span class="pr-op-bytes">${formatBytes(operation.bytes)}</span>
    </li>
  `;
}

function renderSelectedGroup(group: GroupSummary, plan: JobPlan): string {
  const operations = group.operations.slice(0, 8);
  const hasMore = group.operations.length > operations.length;
  const backendEmpty = plan.operations.length === 0 && group.count > 0;

  return `
    <section
      class="pr-group-detail"
      role="tabpanel"
      id="plan-review-panel"
      aria-labelledby="plan-review-tab-${group.id}"
    >
      <div class="pr-detail-head">
        <div>
          <span class="pr-kicker">Operation group</span>
          <h2>${escapeHtml(group.label)}</h2>
          <p>${escapeHtml(group.description)}</p>
        </div>
        <div class="pr-detail-count">
          <strong>${group.count.toLocaleString()}</strong>
          <span>${formatBytes(group.bytes)}</span>
        </div>
      </div>
      ${
        operations.length > 0
          ? `<ul class="pr-operation-list">${operations.map(renderOperation).join("")}</ul>`
          : `<div class="pr-group-empty">${backendEmpty ? "The backend reported counts without itemized operations yet." : escapeHtml(group.emptyText)}</div>`
      }
      ${hasMore ? `<p class="pr-more">Showing first ${operations.length} of ${group.operations.length.toLocaleString()} operations.</p>` : ""}
    </section>
  `;
}

function renderPlanSummary(plan: JobPlan): string {
  return `
    <div class="pr-summary" aria-label="Plan summary">
      <div>
        <span class="pr-kicker">Plan</span>
        <strong>${escapeHtml(plan.planId)}</strong>
      </div>
      <div>
        <span class="pr-kicker">Mode</span>
        <strong>${escapeHtml(plan.request.mode)}</strong>
      </div>
      <div>
        <span class="pr-kicker">Files</span>
        <strong>${plan.totals.files.toLocaleString()}</strong>
      </div>
      <div>
        <span class="pr-kicker">Bytes</span>
        <strong>${formatBytes(plan.totals.bytes)}</strong>
      </div>
    </div>
  `;
}

export function renderPlanReview(state: JobStoreState): string {
  const plan = state.currentPlan;
  const groups = summarizeGroups(plan);
  const activeGroup = selectedGroup(state, groups);
  const executeEnabled = canExecuteJobState(state);

  return `
    <main class="plan-review" aria-label="Plan Review">
      <header class="pr-header">
        <div>
          <span class="pr-kicker">Plan Review</span>
          <h1>Review planned operations</h1>
        </div>
        <div class="pr-header-actions">
          <button class="tool-btn" type="button" data-command-id="return-console">
            ${icon("activity", { size: 13 })}
            <span>Console</span>
          </button>
          <button class="tool-btn primary" type="button" data-command-id="execute" ${executeEnabled ? "" : "disabled"} aria-disabled="${executeEnabled ? "false" : "true"}">
            ${icon("arrowRight", { size: 13 })}
            <span>Execute</span>
          </button>
        </div>
      </header>
      ${
        plan
          ? `
            ${renderPlanSummary(plan)}
            ${renderDeleteReview(state)}
            ${renderRiskSummary(plan)}
            <div class="pr-body">
              <div class="pr-group-list" role="tablist" aria-label="Operation groups" aria-orientation="vertical">
                ${groups.map((group) => renderGroupTab(group, group.id === activeGroup.id)).join("")}
              </div>
              ${renderSelectedGroup(activeGroup, plan)}
            </div>
          `
          : renderEmptyPlanState()
      }
    </main>
  `;
}

export function bindPlanReviewInteractions(root: HTMLElement): () => void {
  const review = root.querySelector<HTMLElement>(".plan-review");

  const activateGroup = (button: HTMLButtonElement) => {
    const group = button.dataset.planReviewGroup as PlanReviewGroupId | undefined;
    if (!group) {
      return;
    }

    jobStore.setSelectedPlanReviewGroup(group);
    window.setTimeout(() => {
      root.querySelector<HTMLButtonElement>(`[data-plan-review-group="${group}"]`)?.focus();
    });
  };

  root.querySelectorAll<HTMLButtonElement>(".pr-group-tab").forEach((button, index, buttons) => {
    button.addEventListener("click", () => activateGroup(button));
    button.addEventListener("keydown", (event) => {
      let nextIndex = index;

      if (event.key === "ArrowDown" || event.key === "ArrowRight") {
        nextIndex = (index + 1) % buttons.length;
      } else if (event.key === "ArrowUp" || event.key === "ArrowLeft") {
        nextIndex = (index - 1 + buttons.length) % buttons.length;
      } else if (event.key === "Home") {
        nextIndex = 0;
      } else if (event.key === "End") {
        nextIndex = buttons.length - 1;
      } else {
        return;
      }

      event.preventDefault();
      activateGroup(buttons[nextIndex]);
    });
  });

  review?.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      jobStore.returnToConsole();
    }
  });

  return () => {};
}
