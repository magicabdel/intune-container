import { useEffect, useRef } from "react";
import styled from "@emotion/styled";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "@xterm/xterm/css/xterm.css";
import { t } from "../theme";

/* An interactive shell inside the container, on a real PTY. Output streams in as
   base64 `shell://data` events; keystrokes go out via `shell_input`. */

const Root = styled.section`
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  background: ${t.color.void};
`;

const Hint = styled.div`
  flex: none;
  padding: 9px 24px;
  border-bottom: 1px solid ${t.color.lineSoft};
  font-family: ${t.font.mono};
  font-size: 10.5px;
  letter-spacing: 0.06em;
  color: ${t.color.faint};
`;

const Term = styled.div`
  flex: 1;
  min-height: 0;
  padding: 8px 14px;
  .xterm {
    height: 100%;
  }
  .xterm-viewport {
    background: transparent !important;
  }
`;

function bytesToBase64(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return btoa(s);
}

function base64ToBytes(b64: string): Uint8Array {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

export function ShellView() {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = ref.current;
    if (!host) return;

    const term = new Terminal({
      cursorBlink: true,
      fontFamily: t.font.mono,
      fontSize: 13,
      lineHeight: 1.2,
      theme: {
        background: t.color.void,
        foreground: t.color.text,
        cursor: t.color.seal,
        cursorAccent: t.color.void,
        selectionBackground: "rgba(35, 201, 184, 0.28)",
        black: "#0b0e13",
        brightBlack: t.color.faint,
        red: t.color.alert,
        green: t.color.seal,
        yellow: t.color.breach,
        blue: "#6aa6ff",
        white: t.color.text,
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);

    const enc = new TextEncoder();
    let alive = true;
    const unlisteners: Array<() => void> = [];

    const start = async () => {
      fit.fit();
      try {
        await invoke("shell_open", { rows: term.rows, cols: term.cols });
      } catch (e) {
        term.writeln(`\r\n\x1b[31mCouldn't open a shell: ${e}\x1b[0m`);
        return;
      }
      unlisteners.push(
        await listen<string>("shell://data", (ev) => {
          if (alive) term.write(base64ToBytes(ev.payload));
        }),
      );
      unlisteners.push(
        await listen("shell://exit", () => {
          if (alive) term.writeln("\r\n\x1b[2m— session ended —\x1b[0m");
        }),
      );
    };

    const onData = term.onData((data) => {
      invoke("shell_input", { data: bytesToBase64(enc.encode(data)) }).catch(() => {});
    });

    const ro = new ResizeObserver(() => {
      try {
        fit.fit();
        invoke("shell_resize", { rows: term.rows, cols: term.cols }).catch(() => {});
      } catch {
        /* terminal not ready */
      }
    });
    ro.observe(host);

    start();
    term.focus();

    return () => {
      alive = false;
      ro.disconnect();
      onData.dispose();
      unlisteners.forEach((u) => u());
      invoke("shell_close").catch(() => {});
      term.dispose();
    };
  }, []);

  return (
    <Root>
      <Hint>Interactive shell · runs as root inside the container · closing this tab ends the session</Hint>
      <Term ref={ref} />
    </Root>
  );
}
