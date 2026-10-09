import type { JobFilters, JobRequest } from "../types/jobs";

export type PresetJobRequest = Pick<
  JobRequest,
  | "source"
  | "target"
  | "mode"
  | "verifyMode"
  | "metadataMode"
  | "backendMode"
  | "threadCount"
  | "bufferSizeBytes"
  | "deletePolicy"
> & {
  filters: Pick<JobFilters, "includeHidden" | "followSymlinks">;
};

export type DefaultPresetId =
  | "media-mirror"
  | "verified-backup"
  | "build-artifact-sync";

export interface ScriptPreset {
  id: DefaultPresetId;
  name: string;
  tagline: string;
  description: string;
  request: PresetJobRequest;
}

const mb = 1024 * 1024;

export const defaultPresets: ScriptPreset[] = [
  {
    id: "media-mirror",
    name: "Media Mirror",
    tagline: "Mirror camera media with review-gated deletes.",
    description:
      "Designed for large media trees where target drift should be surfaced before anything destructive happens.",
    request: {
      source: "",
      target: "",
      mode: "mirror",
      verifyMode: "full_hash",
      metadataMode: "all",
      backendMode: "thread_pool",
      threadCount: 16,
      bufferSizeBytes: 32 * mb,
      deletePolicy: "review",
      filters: {
        includeHidden: false,
        followSymlinks: false,
      },
    },
  },
  {
    id: "verified-backup",
    name: "Verified Backup",
    tagline: "Copy backup sets with read-after-write verification.",
    description:
      "Keeps destination-only files intact while prioritizing conservative verification and complete metadata capture.",
    request: {
      source: "",
      target: "",
      mode: "copy",
      verifyMode: "read_after_write",
      metadataMode: "all",
      backendMode: "auto",
      threadCount: 8,
      bufferSizeBytes: 32 * mb,
      deletePolicy: "never",
      filters: {
        includeHidden: true,
        followSymlinks: false,
      },
    },
  },
  {
    id: "build-artifact-sync",
    name: "Build Artifact Sync",
    tagline: "Fast non-destructive sync for generated outputs.",
    description:
      "Moves build outputs quickly without following links or deleting target files that may belong to another run.",
    request: {
      source: "",
      target: "",
      mode: "sync",
      verifyMode: "size",
      metadataMode: "timestamps",
      backendMode: "thread_pool",
      threadCount: 32,
      bufferSizeBytes: 128 * mb,
      deletePolicy: "never",
      filters: {
        includeHidden: false,
        followSymlinks: false,
      },
    },
  },
];
