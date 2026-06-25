import { useCallback, useEffect, useState } from "react";
import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, PHASE_COPY, phaseOf, StatusReport } from "./api";
import { GlobalStyles, t } from "./theme";
import { ContainmentCore } from "./components/ContainmentCore";

/* The tray quick-panel: a compact hero + the few actions you reach for most,
   shown on a single tray click. Anything deeper lives in the full interface. */

const Wrap = styled.div`
  height: 100vh;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 20px 18px 14px;
  background: ${t.color.panel};
  border: 1px solid ${t.color.line};
`;

const CoreWrap = styled.div`
  width: 116px;
  margin-top: 2px;
`;

const StateWord = styled.div<{ hue: string }>`
  font-family: ${t.font.display};
  font-weight: 700;
  font-size: 21px;
  color: ${(p) => p.hue};
  text-align: center;
`;

const Buttons = styled.div`
  width: 100%;
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-top: 4px;
`;

const Pair = styled.div`
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px;
`;

const spin = keyframes`
  to { transform: rotate(360deg); }
`;

const Primary = styled.button`
  position: relative;
  width: 100%;
  padding: 11px 14px;
  border: none;
  border-radius: ${t.radius.md};
  background: ${t.color.seal};
  color: #06201d;
  font-family: ${t.font.display};
  font-weight: 600;
  font-size: 13.5px;
  cursor: pointer;
  &:hover:not(:disabled) {
    filter: brightness(1.07);
  }
  &:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
`;

const Secondary = styled.button`
  width: 100%;
  padding: 10px 12px;
  border: 1px solid ${t.color.line};
  border-radius: ${t.radius.md};
  background: transparent;
  color: ${t.color.text};
  font-family: ${t.font.display};
  font-weight: 500;
  font-size: 12.5px;
  cursor: pointer;
  &:hover:not(:disabled) {
    border-color: ${t.color.seal};
    background: ${t.color.panel2};
  }
  &:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
`;

const Link = styled.button`
  margin-top: auto;
  border: none;
  background: transparent;
  color: ${t.color.faint};
  font-family: ${t.font.mono};
  font-size: 10.5px;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  cursor: pointer;
  padding: 6px;
  &:hover {
    color: ${t.color.dim};
  }
`;

const Spinner = styled.span`
  display: inline-block;
  width: 11px;
  height: 11px;
  margin-right: 7px;
  vertical-align: -1px;
  border-radius: 50%;
  border: 2px solid rgba(6, 32, 29, 0.35);
  border-top-color: #06201d;
  animation: ${spin} 0.7s linear infinite;
`;

export function Overlay() {
  const [status, setStatus] = useState<StatusReport | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const win = getCurrentWindow();

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.getStatus());
    } catch {
      /* keep previous */
    }
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 3000);
    return () => clearInterval(id);
  }, [refresh]);

  const phase = phaseOf(status);
  const copy = PHASE_COPY[phase];
  const hue =
    phase === "sealed"
      ? t.color.seal
      : phase === "open"
        ? t.color.breach
        : t.color.faint;
  const ready = !!status?.initialized;
  const running = !!status?.running;

  // Power the container on/off in place (overlay stays open).
  const power = (key: string, fn: () => Promise<unknown>) => {
    setBusy(key);
    fn()
      .catch(() => {})
      .finally(() => {
        setBusy(null);
        refresh();
      });
  };

  // Launch a GUI flow, then dismiss the overlay (the app opens its own window).
  const launch = (fn: () => Promise<unknown>) => {
    win.hide();
    fn().catch(() => {});
  };

  const openInterface = () => {
    invoke("show_interface").catch(() => {});
  };

  return (
    <>
      <GlobalStyles />
      <Wrap>
        <CoreWrap>
          <ContainmentCore phase={phase} />
        </CoreWrap>
        <div>
          <StateWord hue={hue}>{copy.state}</StateWord>
        </div>

        <Buttons>
          {!ready ? (
            <Primary onClick={openInterface}>Set up in the interface</Primary>
          ) : running ? (
            <Primary
              disabled={busy === "stop"}
              onClick={() => power("stop", api.stop)}
            >
              {busy === "stop" && <Spinner aria-hidden />}
              Stop container
            </Primary>
          ) : (
            <Primary
              disabled={busy === "start"}
              onClick={() => power("start", api.start)}
            >
              {busy === "start" && <Spinner aria-hidden />}
              Start container
            </Primary>
          )}

          <Pair>
            <Secondary disabled={!ready} onClick={() => launch(api.enroll)}>
              Open portal
            </Secondary>
            <Secondary disabled={!ready} onClick={() => launch(api.edge)}>
              Open Edge
            </Secondary>
          </Pair>
        </Buttons>

        <Link onClick={openInterface}>Open full interface</Link>
      </Wrap>
    </>
  );
}
