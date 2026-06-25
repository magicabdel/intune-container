import { invoke } from "@tauri-apps/api/core";

/** Mirrors `intune_container::ops::StatusReport`. */
export interface StatusReport {
  configured: boolean;
  initialized: boolean;
  running: boolean;
  display_forwarding: boolean;
  expose_bus: boolean;
  machine_name: string;
  rootfs_path: string;
  host_user: string;
  host_uid: number;
  compositor: string;
  wayland: string | null;
  x11_display: string | null;
  has_abstract_x11: boolean;
  xauthority: string | null;
}

/** Mirrors `intune_container::doctor::Check`. */
export interface Check {
  status: "ok" | "warn" | "fail" | "skip";
  label: string;
  detail: string;
}

/** Mirrors `intune_container::native_host::AccountInfo`. */
export interface Account {
  name: string;
  username: string;
  tenant: string;
}

/** Mirrors `intune_container::ops::StartReport`. */
export interface StartReport {
  manifests: string[];
}

export const api = {
  getStatus: () => invoke<StatusReport>("get_status"),
  getDoctor: () => invoke<Check[]>("get_doctor"),
  getAccount: () => invoke<Account | null>("get_account"),
  isInitialized: () => invoke<boolean>("is_initialized"),
  init: (password: string) => invoke<void>("init", { password }),
  enroll: () => invoke<boolean>("enroll"),
  edge: () => invoke<void>("edge"),
  stop: () => invoke<void>("stop"),
  start: () => invoke<StartReport>("start"),
  detachDisplay: () => invoke<void>("detach_display"),
  backup: (path?: string) => invoke<string>("backup", { path: path ?? null }),
  restore: (path?: string) => invoke<void>("restore", { path: path ?? null }),
  destroy: (purge: boolean) => invoke<void>("destroy", { purge }),
  readLog: (maxLines: number) => invoke<string>("read_log", { maxLines }),
  clearLog: () => invoke<void>("clear_log"),
  defaultBackupPath: () => invoke<string>("default_backup_path"),
};

/** The vessel's containment state, derived from the live status. */
export type Phase = "unprovisioned" | "dormant" | "sealed" | "open";

export function phaseOf(s: StatusReport | null): Phase {
  if (!s || !s.configured || !s.initialized) return "unprovisioned";
  if (!s.running) return "dormant";
  return s.display_forwarding ? "open" : "sealed";
}

export const PHASE_COPY: Record<Phase, { state: string; isolation: string }> = {
  unprovisioned: {
    state: "Not set up",
    isolation: "Provision the container and enroll this device to begin.",
  },
  dormant: {
    state: "Stopped",
    isolation: "Powered off. Start it to check in with Intune.",
  },
  sealed: {
    state: "Running",
    isolation: "Running headless — isolated from your desktop.",
  },
  open: {
    state: "Viewport open",
    isolation: "Host display attached. Reseal when you're done.",
  },
};
