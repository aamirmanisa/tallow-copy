import { defaultPresets, type ScriptPreset } from "../presets/defaultPresets";
import { jobStore, type JobStoreState } from "../state/jobStore";
import { icon } from "./icons";

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function formatBuffer(bytes: number): string {
  const mb = bytes / (1024 * 1024);
  return `${Number.isInteger(mb) ? mb.toFixed(0) : mb.toFixed(1)} MB`;
}

function selectedPreset(state: JobStoreState): ScriptPreset {
  return (
    defaultPresets.find((preset) => preset.id === state.selectedScriptPresetId) ??
    defaultPresets[0]
  );
}

function renderPresetButton(preset: ScriptPreset, active: boolean): string {
  return `
    <button
      class="preset-card${active ? " active" : ""}"
      type="button"
      data-preset-id="${escapeHtml(preset.id)}"
      aria-pressed="${active ? "true" : "false"}"
    >
      <span class="preset-card-icon" aria-hidden="true">${icon("script", { size: 17 })}</span>
      <span class="preset-card-main">
        <strong>${escapeHtml(preset.name)}</strong>
        <span>${escapeHtml(preset.tagline)}</span>
      </span>
      <span class="preset-card-mode">${escapeHtml(preset.request.mode)}</span>
    </button>
  `;
}

function renderRequestFact(label: string, value: string): string {
  return `
    <div class="preset-fact">
      <span>${escapeHtml(label)}</span>
      <strong>${escapeHtml(value)}</strong>
    </div>
  `;
}

function renderSelectedPreset(preset: ScriptPreset): string {
  const request = preset.request;
  const filters = [
    request.filters.includeHidden ? "hidden included" : "hidden skipped",
    request.filters.followSymlinks ? "follow symlinks" : "no symlinks",
  ].join(" / ");

  return `
    <section class="preset-detail" aria-labelledby="selected-preset-title">
      <div class="preset-detail-head">
        <div>
          <span class="pr-kicker">Selected Preset</span>
          <h2 id="selected-preset-title">${escapeHtml(preset.name)}</h2>
          <p>${escapeHtml(preset.description)}</p>
        </div>
        <button class="tool-btn primary" type="button" data-command-id="apply-script-preset" data-preset-id="${escapeHtml(
          preset.id,
        )}">
          ${icon("check", { size: 13 })}
          <span>Apply</span>
        </button>
      </div>
      <div class="preset-facts" aria-label="Preset request defaults">
        ${renderRequestFact("Mode", request.mode)}
        ${renderRequestFact("Verify", request.verifyMode.replace(/_/g, " "))}
        ${renderRequestFact("Metadata", request.metadataMode.replace(/_/g, " "))}
        ${renderRequestFact("Backend", `${request.backendMode.replace(/_/g, " ")} / ${request.threadCount} workers`)}
        ${renderRequestFact("Buffer", formatBuffer(request.bufferSizeBytes))}
        ${renderRequestFact("Deletes", request.deletePolicy)}
        ${renderRequestFact("Filters", filters)}
      </div>
      <div class="preset-paths" aria-label="Preset paths">
        <div>
          <span>Source</span>
          <strong>${escapeHtml(request.source || "Choose when applying")}</strong>
        </div>
        <div>
          <span>Target</span>
          <strong>${escapeHtml(request.target || "Choose when applying")}</strong>
        </div>
      </div>
    </section>
  `;
}

export function renderScriptPresetsView(state: JobStoreState): string {
  const activePreset = selectedPreset(state);

  return `
    <main class="script-presets-view" aria-label="Transfer Presets">
      <header class="report-header">
        <div>
          <span class="pr-kicker">Templates</span>
          <h1>Transfer presets</h1>
        </div>
        <button class="tool-btn" type="button" data-command-id="return-console">
          ${icon("activity", { size: 13 })}
          <span>Active</span>
        </button>
      </header>
      <div class="script-presets-body">
        <div class="preset-list" aria-label="Built-in presets">
          ${defaultPresets
            .map((preset) => renderPresetButton(preset, preset.id === activePreset.id))
            .join("")}
        </div>
        ${renderSelectedPreset(activePreset)}
      </div>
    </main>
  `;
}

export function bindScriptPresetsInteractions(root: HTMLElement): () => void {
  const buttons = Array.from(root.querySelectorAll<HTMLButtonElement>(".preset-card"));

  const selectPreset = (button: HTMLButtonElement) => {
    const presetId = button.dataset.presetId;
    if (!presetId) {
      return;
    }
    jobStore.setSelectedScriptPreset(presetId);
    window.setTimeout(() => {
      root.querySelector<HTMLButtonElement>(`[data-preset-id="${presetId}"]`)?.focus({
        preventScroll: true,
      });
    });
  };

  buttons.forEach((button, index) => {
    button.addEventListener("click", () => selectPreset(button));
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
      selectPreset(buttons[nextIndex]);
    });
  });

  root.querySelectorAll<HTMLButtonElement>('[data-command-id="apply-script-preset"]').forEach((button) => {
    button.addEventListener("click", () => {
      const preset = defaultPresets.find((candidate) => candidate.id === button.dataset.presetId);
      if (preset) {
        jobStore.applyScriptPreset(preset.id, preset.request);
      }
    });
  });

  root.querySelector<HTMLElement>(".script-presets-view")?.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      jobStore.returnToConsole();
    }
  });

  return () => {};
}
