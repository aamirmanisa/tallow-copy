export type IconName =
  | "activity"
  | "addFolder"
  | "arrowRight"
  | "cancel"
  | "check"
  | "clock"
  | "database"
  | "download"
  | "folder"
  | "grid"
  | "history"
  | "more"
  | "pause"
  | "plan"
  | "refresh"
  | "script"
  | "search"
  | "settings"
  | "upload";

type IconOptions = {
  className?: string;
  size?: number;
  decorative?: boolean;
};

const paths: Record<IconName, string> = {
  activity:
    '<path d="M5 12h14"/><polyline points="12,5 19,12 12,19"/>',
  addFolder:
    '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>',
  arrowRight:
    '<path d="M5 12h14"/><polyline points="12,5 19,12 12,19"/>',
  cancel: '<rect width="14" height="14" x="5" y="5" rx="2"/>',
  check: '<polyline points="20,6 9,17 4,12"/>',
  clock: '<circle cx="12" cy="12" r="10"/><polyline points="12,6 12,12 16,14"/>',
  database:
    '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14c0 1.66 4.03 3 9 3s9-1.34 9-3V5"/><path d="M3 12c0 1.66 4.03 3 9 3s9-1.34 9-3"/>',
  download:
    '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7,10 12,15 17,10"/><line x1="12" y1="15" x2="12" y2="3"/>',
  folder:
    '<path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/>',
  grid: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 9h18"/><path d="M9 21V9"/>',
  history:
    '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/>',
  more: '<circle cx="12" cy="12" r="1"/><circle cx="12" cy="5" r="1"/><circle cx="12" cy="19" r="1"/>',
  pause: '<rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/>',
  plan:
    '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17,8 12,3 7,8"/><line x1="12" y1="3" x2="12" y2="15"/>',
  refresh:
    '<polyline points="17,1 21,5 17,9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7,23 3,19 7,15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/>',
  script:
    '<polyline points="17,1 21,5 17,9"/><path d="M3 11V9a4 4 0 0 1 4-4h14"/><polyline points="7,23 3,19 7,15"/><path d="M21 13v2a4 4 0 0 1-4 4H3"/>',
  search:
    '<circle cx="11" cy="11" r="8"/><path d="m21 21-4.35-4.35"/>',
  settings:
    '<circle cx="12" cy="12" r="3"/><path d="M12 1v6m0 10v6m11-11h-6M7 12H1m18.36-7.36-4.24 4.24M8.88 15.12l-4.24 4.24m14.72 0-4.24-4.24M8.88 8.88 4.64 4.64"/>',
  upload:
    '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17,8 12,3 7,8"/><line x1="12" y1="3" x2="12" y2="15"/>',
};

export function icon(name: IconName, options: IconOptions = {}): string {
  const size = options.size ?? 16;
  const className = options.className ? ` class="${options.className}"` : "";
  const aria = options.decorative === false ? "" : ' aria-hidden="true"';
  const fill = name === "pause" ? "currentColor" : "none";
  const stroke = name === "pause" ? "none" : "currentColor";

  return `<svg${className}${aria} width="${size}" height="${size}" viewBox="0 0 24 24" fill="${fill}" stroke="${stroke}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${paths[name]}</svg>`;
}
