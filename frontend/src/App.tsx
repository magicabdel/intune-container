import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import styled from "@emotion/styled";
import { open, save } from "@tauri-apps/plugin-dialog";
import { api, Check, PHASE_COPY, phaseOf, StatusReport } from "./api";
import { GlobalStyles, t, eyebrow } from "./theme";
import { ContainmentCore } from "./components/ContainmentCore";
import { ControlButton } from "./components/ControlButton";
import { DoctorPanel } from "./components/DoctorPanel";
import { PasswordModal } from "./components/PasswordModal";
import { LogsView } from "./components/LogsView";
import { ToastData, ToastTone, Toasts } from "./components/Toast";

/* ------------------------------------------------------------------ layout */

const Shell = styled.div`
  height: 100vh;
  display: flex;
  flex-direction: column;
`;

const Header = styled.header`
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px 24px;
  border-bottom: 1px solid ${t.color.lineSoft};
  flex: none;
`;

const Brand = styled.div`
  display: flex;
  align-items: center;
  gap: 12px;
`;

const Mark = styled.div`
  width: 30px;
  height: 30px;
  display: grid;
  place-items: center;
  color: ${t.color.seal};
`;

const WordMark = styled.div`
  font-family: ${t.font.display};
  font-weight: 700;
  font-size: 15px;
  letter-spacing: 0.02em;
  line-height: 1;
  span {
    color: ${t.color.dim};
    font-weight: 400;
  }
`;

const Pill = styled.div<{ hue: string }>`
  display: inline-flex;
  align-items: center;
  gap: 8px;
  padding: 6px 13px;
  border-radius: 999px;
  border: 1px solid ${(p) => p.hue};
  background: ${t.color.panel};
  font-family: ${t.font.mono};
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: ${t.color.text};
  &::before {
    content: "";
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: ${(p) => p.hue};
  }
`;

const Tabs = styled.nav`
  display: inline-flex;
  gap: 2px;
  padding: 3px;
  border-radius: 999px;
  border: 1px solid ${t.color.lineSoft};
  background: ${t.color.panel};
`;

const Tab = styled.button<{ active: boolean }>`
  padding: 6px 16px;
  border: none;
  border-radius: 999px;
  cursor: pointer;
  font-family: ${t.font.mono};
  font-size: 11px;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  background: ${(p) => (p.active ? t.color.raise : "transparent")};
  color: ${(p) => (p.active ? t.color.text : t.color.faint)};
  transition:
    color 0.15s ease,
    background 0.15s ease;
  &:hover {
    color: ${t.color.text};
  }
`;

const Body = styled.main`
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: 360px 1fr;
  @media (max-width: 820px) {
    grid-template-columns: 1fr;
    overflow-y: auto;
  }
`;

const CorePanel = styled.section`
  border-right: 1px solid ${t.color.lineSoft};
  padding: 30px 28px;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 20px;
  @media (max-width: 820px) {
    border-right: none;
    border-bottom: 1px solid ${t.color.lineSoft};
  }
`;

const CoreState = styled.div`
  text-align: center;
`;

const StateWord = styled.div<{ hue: string }>`
  font-family: ${t.font.display};
  font-weight: 700;
  font-size: 26px;
  letter-spacing: 0.01em;
  color: ${(p) => p.hue};
`;

const Isolation = styled.div`
  margin-top: 4px;
  font-size: 13px;
  color: ${t.color.dim};
`;

const Readout = styled.dl`
  width: 100%;
  margin: 6px 0 0;
  border-top: 1px solid ${t.color.lineSoft};
`;

const ReadRow = styled.div`
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 14px;
  padding: 9px 2px;
  border-bottom: 1px solid ${t.color.lineSoft};

  dt {
    font-family: ${t.font.mono};
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: ${t.color.faint};
    margin: 0;
    white-space: nowrap;
  }
  dd {
    margin: 0;
    font-family: ${t.font.mono};
    font-size: 12px;
    color: ${t.color.text};
    text-align: right;
    word-break: break-word;
  }
`;

const Controls = styled.section`
  padding: 24px 28px 30px;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 22px;
`;

const Group = styled.div``;

const GroupHead = styled.div`
  margin-bottom: 11px;
`;

const Grid = styled.div`
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(238px, 1fr));
  gap: 11px;
`;

const Footer = styled.footer`
  flex: none;
  padding: 11px 24px;
  border-top: 1px solid ${t.color.lineSoft};
  font-family: ${t.font.mono};
  font-size: 10.5px;
  letter-spacing: 0.05em;
  color: ${t.color.faint};
  text-align: center;
`;

/* destroy confirmation */

const Backdrop = styled.div`
  position: fixed;
  inset: 0;
  background: rgba(4, 6, 9, 0.62);
  display: grid;
  place-items: center;
  z-index: 40;
`;

const Dialog = styled.div`
  width: min(440px, 92vw);
  background: ${t.color.panel};
  border: 1px solid ${t.color.alert};
  border-radius: ${t.radius.lg};
  padding: 24px 26px 20px;
  box-shadow: 0 24px 60px rgba(0, 0, 0, 0.5);

  h3 {
    margin: 6px 0 8px;
    font-family: ${t.font.display};
    font-size: 18px;
    color: ${t.color.alert};
  }
  p {
    margin: 0 0 14px;
    color: ${t.color.dim};
    font-size: 13px;
    line-height: 1.5;
  }
`;

const Checkbox = styled.label`
  display: flex;
  gap: 9px;
  align-items: flex-start;
  font-size: 13px;
  color: ${t.color.text};
  margin-bottom: 18px;
  cursor: pointer;
  input {
    margin-top: 2px;
    accent-color: ${t.color.alert};
  }
  span {
    color: ${t.color.dim};
  }
`;

const DialogRow = styled.div`
  display: flex;
  justify-content: flex-end;
  gap: 10px;
`;

const Ghost = styled.button`
  font-family: ${t.font.display};
  font-size: 13.5px;
  padding: 9px 18px;
  border-radius: ${t.radius.sm};
  border: 1px solid ${t.color.line};
  background: transparent;
  color: ${t.color.text};
  cursor: pointer;
  &:hover {
    border-color: #3a4658;
  }
`;

const Destructive = styled.button`
  font-family: ${t.font.display};
  font-weight: 500;
  font-size: 13.5px;
  padding: 9px 18px;
  border-radius: ${t.radius.sm};
  border: 1px solid ${t.color.alert};
  background: ${t.color.alert};
  color: #190406;
  cursor: pointer;
  &:hover {
    filter: brightness(1.08);
  }
`;

/* ------------------------------------------------------------------- logic */

const hexGlyph = (
  <svg viewBox="0 0 32 32" width="30" height="30" aria-hidden>
    <path
      d="M16 3 L27 9.5 L27 22.5 L16 29 L5 22.5 L5 9.5 Z"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinejoin="round"
    />
    <path d="M16 11 L21 14 L16 17 L11 14 Z" fill="currentColor" />
    <path
      d="M11 14 L16 17 L16 23 L11 20 Z"
      fill="currentColor"
      opacity="0.45"
    />
    <path d="M21 14 L16 17 L16 23 L21 20 Z" fill="currentColor" opacity="0.7" />
  </svg>
);

export function App() {
  const [status, setStatus] = useState<StatusReport | null>(null);
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [toasts, setToasts] = useState<ToastData[]>([]);
  const [pwOpen, setPwOpen] = useState(false);
  const [destroyOpen, setDestroyOpen] = useState(false);
  const [purge, setPurge] = useState(false);
  const [doctorOpen, setDoctorOpen] = useState(false);
  const [doctorLoading, setDoctorLoading] = useState(false);
  const [checks, setChecks] = useState<Check[] | null>(null);
  const [view, setView] = useState<"console" | "logs">("console");
  const toastId = useRef(0);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.getStatus());
    } catch {
      /* keep previous state */
    }
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 3000);
    return () => clearInterval(id);
  }, [refresh]);

  const toast = (tone: ToastTone, message: string) => {
    const id = ++toastId.current;
    setToasts((xs) => [...xs, { id, tone, message }]);
    setTimeout(() => setToasts((xs) => xs.filter((x) => x.id !== id)), 4200);
  };

  async function run<T>(
    key: string,
    fn: () => Promise<T>,
    done?: (r: T) => string,
  ): Promise<void> {
    setPending((p) => ({ ...p, [key]: true }));
    try {
      const r = await fn();
      if (done) toast("ok", done(r));
    } catch (e) {
      toast("error", typeof e === "string" ? e : String(e));
    } finally {
      setPending((p) => ({ ...p, [key]: false }));
      refresh();
    }
  }

  const enrollFlow = () =>
    run("enroll", api.enroll, (closed) =>
      closed
        ? "Enrollment window closed."
        : "The portal didn't open — try again.",
    );

  const doEnroll = async () => {
    let initialized = false;
    try {
      initialized = await api.isInitialized();
    } catch {
      /* treat as not initialized */
    }
    if (!initialized) {
      setPwOpen(true);
      return;
    }
    toast("info", "Opening the Intune portal… first launch can take ~30s.");
    enrollFlow();
  };

  const provisionAndEnroll = async (password: string) => {
    setPwOpen(false);
    toast(
      "info",
      "Provisioning the container… you may be asked for your sudo password.",
    );
    try {
      await api.init(password);
    } catch (e) {
      toast("error", typeof e === "string" ? e : String(e));
      refresh();
      return;
    }
    toast("info", "Opening the Intune portal… first launch can take ~30s.");
    enrollFlow();
  };

  const doDoctor = async () => {
    setDoctorOpen(true);
    setDoctorLoading(true);
    try {
      setChecks(await api.getDoctor());
    } catch (e) {
      toast("error", typeof e === "string" ? e : String(e));
    } finally {
      setDoctorLoading(false);
    }
  };

  const doBackup = async () => {
    let defaultPath: string | undefined;
    try {
      defaultPath = await api.defaultBackupPath();
    } catch {
      /* no default available */
    }
    let dest: string | null;
    try {
      dest = await save({
        title: "Save enrollment backup",
        defaultPath,
        filters: [{ name: "Gzip archive", extensions: ["gz", "tgz"] }],
      });
    } catch (e) {
      toast("error", typeof e === "string" ? e : String(e));
      return;
    }
    if (!dest) return; // cancelled
    const target = dest;
    run(
      "backup",
      () => api.backup(target),
      (p) => `Backed up to ${p}`,
    );
  };

  const doRestore = async () => {
    let defaultPath: string | undefined;
    try {
      defaultPath = await api.defaultBackupPath();
    } catch {
      /* no default available */
    }
    let picked: string | string[] | null;
    try {
      picked = await open({
        title: "Select a backup to restore",
        defaultPath,
        multiple: false,
        directory: false,
        filters: [{ name: "Gzip archive", extensions: ["gz", "tgz"] }],
      });
    } catch (e) {
      toast("error", typeof e === "string" ? e : String(e));
      return;
    }
    if (!picked || Array.isArray(picked)) return; // cancelled
    const file = picked;
    run(
      "restore",
      () => api.restore(file),
      () => "Enrollment restored.",
    );
  };

  const confirmDestroy = () => {
    const wipe = purge;
    setDestroyOpen(false);
    setPurge(false);
    run(
      "destroy",
      () => api.destroy(wipe),
      () =>
        wipe ? "Container destroyed and data purged." : "Container destroyed.",
    );
  };

  const s = status;
  const phase = phaseOf(s);
  const hue =
    phase === "sealed"
      ? t.color.seal
      : phase === "open"
        ? t.color.breach
        : t.color.faint;
  const copy = PHASE_COPY[phase];
  const ready = !!s?.initialized;
  const running = !!s?.running;
  const busy = (k: string) => !!pending[k];

  const readout = useMemo(
    () => [
      { k: "State", v: copy.state },
      { k: "Isolation", v: copy.isolation },
      { k: "Browser SSO", v: s?.expose_bus ? "Enabled" : "Off" },
      { k: "Machine", v: s?.machine_name ?? "—" },
      { k: "Host", v: s ? `${s.host_user} · uid ${s.host_uid}` : "—" },
      { k: "Compositor", v: s?.compositor ?? "—" },
    ],
    [s, copy],
  );

  return (
    <>
      <GlobalStyles />
      <Shell>
        <Header>
          <Brand>
            <Mark>{hexGlyph}</Mark>
            <WordMark>
              INTUNE<span>·</span>CONTAINER
            </WordMark>
          </Brand>
          <Tabs>
            <Tab active={view === "console"} onClick={() => setView("console")}>
              Console
            </Tab>
            <Tab active={view === "logs"} onClick={() => setView("logs")}>
              Logs
            </Tab>
          </Tabs>
          <Pill hue={hue}>{copy.state}</Pill>
        </Header>

        {view === "logs" ? (
          <LogsView />
        ) : (
          <Body>
            <CorePanel>
              <div css={eyebrow}>Containment</div>
              <ContainmentCore phase={phase} />
              <CoreState>
                <StateWord hue={hue}>{copy.state}</StateWord>
                <Isolation>{copy.isolation}</Isolation>
              </CoreState>
              <Readout>
                {readout.map((r) => (
                  <ReadRow key={r.k}>
                    <dt>{r.k}</dt>
                    <dd>{r.v}</dd>
                  </ReadRow>
                ))}
              </Readout>
            </CorePanel>

            <Controls>
              <Group>
                <GroupHead>
                  <div css={eyebrow}>Provision &amp; access</div>
                </GroupHead>
                <Grid>
                  <ControlButton
                    tone="primary"
                    title={ready ? "Open Intune portal" : "Enroll this device"}
                    hint={
                      ready
                        ? "Reopen the company portal to manage enrollment."
                        : "One-time setup: provision the container and sign in."
                    }
                    busy={busy("enroll")}
                    onClick={doEnroll}
                  />
                  <ControlButton
                    title="Launch Microsoft Edge"
                    hint="Open Edge inside the container; the host display attaches."
                    disabled={!ready}
                    busy={busy("edge")}
                    onClick={() => run("edge", api.edge)}
                  />
                  <ControlButton
                    title="Open a shell"
                    hint="Interactive shell inside the container, in a terminal."
                    disabled={!ready}
                    busy={busy("shell")}
                    onClick={() => run("shell", api.openShell)}
                  />
                </Grid>
              </Group>

              <Group>
                <GroupHead>
                  <div css={eyebrow}>Connectivity</div>
                </GroupHead>
                <Grid>
                  <ControlButton
                    title="Enable browser SSO"
                    hint="Sign in to Teams and M365 from your host browser."
                    disabled={!ready}
                    busy={busy("daemon")}
                    onClick={() =>
                      run("daemon", api.daemon, (r) => {
                        const n = r.manifests.length;
                        return `Browser SSO ready (${n} manifest${n === 1 ? "" : "s"}). Install the linux-entra-sso extension.`;
                      })
                    }
                  />
                </Grid>
              </Group>

              <Group>
                <GroupHead>
                  <div css={eyebrow}>Session</div>
                </GroupHead>
                <Grid>
                  <ControlButton
                    title="Stop container"
                    hint="Power off the running machine."
                    disabled={!running}
                    busy={busy("stop")}
                    onClick={() =>
                      run("stop", api.stop, () => "Container stopped.")
                    }
                  />
                  <ControlButton
                    title="Return to headless"
                    hint="Detach the host display and reseal the container."
                    disabled={!running || !s?.display_forwarding}
                    busy={busy("detach")}
                    onClick={() =>
                      run(
                        "detach",
                        api.detachDisplay,
                        () => "Resealed — back to headless.",
                      )
                    }
                  />
                </Grid>
              </Group>

              <Group>
                <GroupHead>
                  <div css={eyebrow}>Enrollment data</div>
                </GroupHead>
                <Grid>
                  <ControlButton
                    title="Run diagnostics"
                    hint="Check config, container, DNS, broker and SSO."
                    busy={doctorLoading}
                    onClick={doDoctor}
                  />
                  <ControlButton
                    title="Back up enrollment"
                    hint="Choose where to save the registration archive."
                    disabled={!ready}
                    busy={busy("backup")}
                    onClick={doBackup}
                  />
                  <ControlButton
                    title="Restore enrollment"
                    hint="Pick a backup archive to restore from."
                    disabled={!ready}
                    busy={busy("restore")}
                    onClick={doRestore}
                  />
                </Grid>
              </Group>

              <Group>
                <GroupHead>
                  <div css={eyebrow}>Danger zone</div>
                </GroupHead>
                <Grid>
                  <ControlButton
                    tone="danger"
                    title="Destroy container"
                    hint="Remove the rootfs, config and host integration."
                    disabled={!s?.configured}
                    busy={busy("destroy")}
                    onClick={() => setDestroyOpen(true)}
                  />
                </Grid>
              </Group>
            </Controls>
          </Body>
        )}

        <Footer>
          OPERATIONS RUN IN-PROCESS · GRAPHICAL SUDO WHEN NEEDED · CLOSING KEEPS
          THE APP IN YOUR TRAY
        </Footer>
      </Shell>

      {pwOpen && (
        <PasswordModal
          onCancel={() => setPwOpen(false)}
          onSubmit={provisionAndEnroll}
        />
      )}

      {doctorOpen && (
        <DoctorPanel
          checks={checks}
          loading={doctorLoading}
          onClose={() => setDoctorOpen(false)}
        />
      )}

      {destroyOpen && (
        <Backdrop onMouseDown={() => setDestroyOpen(false)}>
          <Dialog
            onMouseDown={(e) => e.stopPropagation()}
            role="dialog"
            aria-label="Destroy container"
          >
            <div css={eyebrow}>Irreversible</div>
            <h3>Destroy the container?</h3>
            <p>
              This removes the container rootfs, its configuration, the sudoers
              helper and browser SSO manifests. You'll need to enroll again.
            </p>
            <Checkbox>
              <input
                type="checkbox"
                checked={purge}
                onChange={(e) => setPurge(e.target.checked)}
              />
              <span>
                Also purge enrollment data (~/Intune) and persistent
                device-state. Leave unchecked to keep them for a future rebuild.
              </span>
            </Checkbox>
            <DialogRow>
              <Ghost onClick={() => setDestroyOpen(false)}>Cancel</Ghost>
              <Destructive onClick={confirmDestroy}>Destroy</Destructive>
            </DialogRow>
          </Dialog>
        </Backdrop>
      )}

      <Toasts items={toasts} />
    </>
  );
}
