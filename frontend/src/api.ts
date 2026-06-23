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

/** Mirrors `intune_container::ops::DaemonReport`. */
export interface DaemonReport {
  manifests: string[];
}

export const api = {
  getStatus: () => invoke<StatusReport>("get_status"),
  getDoctor: () => invoke<Check[]>("get_doctor"),
  isInitialized: () => invoke<boolean>("is_initialized"),
  init: (password: string) => invoke<void>("init", { password }),
  enroll: () => invoke<boolean>("enroll"),
  edge: () => invoke<void>("edge"),
  daemon: () => invoke<DaemonReport>("daemon"),
  stop: () => invoke<void>("stop"),
  detachDisplay: () => invoke<void>("detach_display"),
  backup: (path?: string) => invoke<string>("backup", { path: path ?? null }),
  restore: (path?: string) => invoke<void>("restore", { path: path ?? null }),
  destroy: (purge: boolean) => invoke<void>("destroy", { purge }),
  openShell: () => invoke<void>("open_shell"),
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
  unprovisioned: { state: "Not set up", isolation: "No container provisioned" },
  dormant: { state: "Stopped", isolation: "Container powered off" },
  sealed: { state: "Running", isolation: "Sealed · headless" },
  open: { state: "Running", isolation: "Viewport open · display attached" },
};
