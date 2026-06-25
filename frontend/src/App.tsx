import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { api, Check, PHASE_COPY, phaseOf, StatusReport } from "./api";
import { GlobalStyles, t, eyebrow } from "./theme";
import { ContainmentCore } from "./components/ContainmentCore";
import { HealthPanel } from "./components/HealthPanel";
import { PasswordModal } from "./components/PasswordModal";
import { LogsView } from "./components/LogsView";
import { ShellView } from "./components/ShellView";
import { BackupView } from "./components/BackupView";
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
  padding: 15px 24px;
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

const Main = styled.main`
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  align-items: center;
  padding: 34px 24px 16px;
`;

const Col = styled.div`
  width: 100%;
  max-width: 560px;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 16px;
`;

const CoreWrap = styled.div`
  width: 168px;
`;

const StateWord = styled.div<{ hue: string }>`
  font-family: ${t.font.display};
  font-weight: 700;
  font-size: 27px;
  letter-spacing: 0.01em;
  color: ${(p) => p.hue};
  text-align: center;
`;

const Sub = styled.p`
  margin: 5px 0 0;
  font-size: 13px;
  line-height: 1.5;
  color: ${t.color.dim};
  text-align: center;
  max-width: 30ch;
`;

const CTARow = styled.div`
  width: 100%;
  max-width: 380px;
  display: flex;
  flex-direction: column;
  gap: 9px;
  margin-top: 2px;
`;

const Pair = styled.div`
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 9px;
`;

const spin = keyframes`
  to { transform: rotate(360deg); }
`;

const Primary = styled.button`
  position: relative;
  width: 100%;
  padding: 13px 16px;
  border: none;
  border-radius: ${t.radius.md};
  background: ${t.color.seal};
  color: #06201d;
  font-family: ${t.font.display};
  font-weight: 600;
  font-size: 14.5px;
  letter-spacing: 0.01em;
  cursor: pointer;
  transition:
    filter 0.15s ease,
    transform 0.07s ease;
  &:hover:not(:disabled) {
    filter: brightness(1.07);
  }
  &:active:not(:disabled) {
    transform: translateY(1px);
  }
  &:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
`;

const Secondary = styled.button`
  width: 100%;
  padding: 11px 16px;
  border: 1px solid ${t.color.line};
  border-radius: ${t.radius.md};
  background: transparent;
  color: ${t.color.text};
  font-family: ${t.font.display};
  font-weight: 500;
  font-size: 13.5px;
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    background 0.15s ease;
  &:hover:not(:disabled) {
    border-color: ${t.color.seal};
    background: ${t.color.panel2};
  }
  &:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
`;

const Spinner = styled.span`
  display: inline-block;
  width: 12px;
  height: 12px;
  margin-right: 8px;
  vertical-align: -1px;
  border-radius: 50%;
  border: 2px solid rgba(6, 32, 29, 0.35);
  border-top-color: #06201d;
  animation: ${spin} 0.7s linear infinite;
`;

const HealthWrap = styled.div`
  width: 100%;
  margin-top: 6px;
`;

/* ----- bottom utility bar: the quiet, secondary actions ----- */

const UtilityBar = styled.div`
  flex: none;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  padding: 12px 24px;
  border-top: 1px solid ${t.color.lineSoft};
  background: ${t.color.panel};
`;

const BarLabel = styled.span`
  font-family: ${t.font.mono};
  font-size: 10px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
  color: ${t.color.faint};
  margin-right: 4px;
`;

const Spacer = styled.span`
  flex: 1;
`;

const Chip = styled.button<{ danger?: boolean }>`
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 7px 13px;
  border-radius: 999px;
  border: 1px solid ${(p) => (p.danger ? t.color.breachDim : t.color.line)};
  background: transparent;
  color: ${(p) => (p.danger ? t.color.alert : t.color.text)};
  font-family: ${t.font.body};
  font-size: 12.5px;
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    background 0.15s ease,
    color 0.15s ease;
  &:hover:not(:disabled) {
    border-color: ${(p) => (p.danger ? t.color.alert : "#3a4658")};
    background: ${t.color.panel2};
  }
  &:disabled {
    opacity: 0.38;
    cursor: not-allowed;
  }
`;

const Footer = styled.footer`
  flex: none;
  padding: 9px 24px;
  border-top: 1px solid ${t.color.lineSoft};
  font-family: ${t.font.mono};
  font-size: 10px;
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

/* ------------------------------------------------------------------- glyph */

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

/* -------------------------------------------------------------------- logic */

export function App() {
  const [status, setStatus] = useState<StatusReport | null>(null);
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [toasts, setToasts] = useState<ToastData[]>([]);
  const [pwOpen, setPwOpen] = useState(false);
  const [destroyOpen, setDestroyOpen] = useState(false);
  const [purge, setPurge] = useState(false);
  const [view, setView] = useState<"console" | "logs" | "shell" | "backup">(
    "console",
  );

  const [checks, setChecks] = useState<Check[] | null>(null);
  const [doctorLoading, setDoctorLoading] = useState(false);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  const doctorBusy = useRef(false);

  const toastId = useRef(0);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.getStatus());
    } catch {
      /* keep previous state */
    }
  }, []);

  const refreshDoctor = useCallback(async () => {
    if (doctorBusy.current) return;
    doctorBusy.current = true;
    setDoctorLoading(true);
    try {
      setChecks(await api.getDoctor());
      setCheckedAt(Date.now());
    } catch {
      /* keep previous checks */
    } finally {
      setDoctorLoading(false);
      doctorBusy.current = false;
    }
  }, []);

  useEffect(() => {
    refresh();
    refreshDoctor();
    const a = setInterval(refresh, 3000);
    const b = setInterval(refreshDoctor, 20000);
    return () => {
      clearInterval(a);
      clearInterval(b);
    };
  }, [refresh, refreshDoctor]);

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
      refreshDoctor();
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
    if (!dest) return;
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
    if (!picked || Array.isArray(picked)) return;
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

  // Primary control: power the container on/off once it's set up; before that,
  // the one thing to do is enroll.
  const primaryKey = !ready ? "enroll" : running ? "stop" : "start";
  const primaryLabel = !ready
    ? "Enroll this device"
    : running
      ? "Stop container"
      : "Start container";
  const onPrimary = () => {
    if (!ready) return doEnroll();
    if (running) return run("stop", api.stop, () => "Container stopped.");
    return run("start", api.start, () => "Container started.");
  };

  const meta = useMemo(
    () => [
      {
        k: "SSO",
        v: s?.expose_bus ? "on" : "off",
        hue: s?.expose_bus ? t.color.seal : t.color.faint,
      },
      { k: "host", v: s?.host_user ?? "—" },
      { k: "machine", v: s?.machine_name ?? "—" },
      { k: "display", v: s?.compositor ?? "—" },
    ],
    [s],
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
            <Tab active={view === "shell"} onClick={() => setView("shell")}>
              Shell
            </Tab>
            <Tab active={view === "backup"} onClick={() => setView("backup")}>
              Backup
            </Tab>
            <Tab active={view === "logs"} onClick={() => setView("logs")}>
              Logs
            </Tab>
          </Tabs>
          <Pill hue={hue}>{copy.state}</Pill>
        </Header>

        {view === "logs" ? (
          <LogsView />
        ) : view === "shell" ? (
          <ShellView />
        ) : view === "backup" ? (
          <BackupView
            ready={ready}
            backingUp={busy("backup")}
            restoring={busy("restore")}
            onBackup={doBackup}
            onRestore={doRestore}
          />
        ) : (
          <>
            <Main>
              <Col>
                <div css={eyebrow}>Containment</div>
                <CoreWrap>
                  <ContainmentCore phase={phase} />
                </CoreWrap>
                <div style={{ textAlign: "center" }}>
                  <StateWord hue={hue}>{copy.state}</StateWord>
                  <Sub>{copy.isolation}</Sub>
                </div>

                <CTARow>
                  <Primary onClick={onPrimary} disabled={busy(primaryKey)}>
                    {busy(primaryKey) && <Spinner aria-hidden />}
                    {primaryLabel}
                  </Primary>
                  <Pair>
                    <Secondary
                      disabled={!ready || busy("enroll")}
                      onClick={doEnroll}
                    >
                      Open portal
                    </Secondary>
                    <Secondary
                      disabled={!ready || busy("edge")}
                      onClick={() => run("edge", api.edge)}
                    >
                      Open Edge
                    </Secondary>
                  </Pair>
                  {ready && !s?.expose_bus && (
                    <Secondary
                      onClick={() =>
                        run("daemon", api.daemon, (r) => {
                          const n = r.manifests.length;
                          return `Browser SSO ready (${n} manifest${n === 1 ? "" : "s"}). Install the linux-entra-sso extension.`;
                        })
                      }
                      disabled={busy("daemon")}
                    >
                      Enable browser SSO
                    </Secondary>
                  )}
                </CTARow>

                <HealthWrap>
                  <HealthPanel
                    checks={checks}
                    loading={doctorLoading}
                    checkedAt={checkedAt}
                    meta={meta}
                    onRefresh={refreshDoctor}
                  />
                </HealthWrap>
              </Col>
            </Main>

            <UtilityBar>
              <BarLabel>Actions</BarLabel>
              {running && s?.display_forwarding && (
                <Chip
                  disabled={busy("detach")}
                  onClick={() =>
                    run(
                      "detach",
                      api.detachDisplay,
                      () => "Resealed — back to headless.",
                    )
                  }
                >
                  Return to headless
                </Chip>
              )}
              <Spacer />
              <Chip
                danger
                disabled={!s?.configured || busy("destroy")}
                onClick={() => setDestroyOpen(true)}
              >
                Destroy
              </Chip>
            </UtilityBar>
          </>
        )}

        <Footer>
          OPERATIONS RUN IN-PROCESS · CLOSING THE WINDOW KEEPS THE APP IN YOUR
          TRAY
        </Footer>
      </Shell>

      {pwOpen && (
        <PasswordModal
          onCancel={() => setPwOpen(false)}
          onSubmit={provisionAndEnroll}
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
              This removes the container rootfs, its configuration, and the
              browser SSO manifests. You'll need to enroll again.
            </p>
            <Checkbox>
              <input
                type="checkbox"
                checked={purge}
                onChange={(e) => setPurge(e.target.checked)}
              />
              <span>
                Also purge enrollment data and persistent device state. Leave
                unchecked to keep them for a future rebuild.
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
