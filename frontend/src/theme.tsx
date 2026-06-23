import { css, Global } from "@emotion/react";

/**
 * Design tokens — "containment console".
 *
 * The product seals a corporate device-management agent inside an isolated
 * container; the UI is the instrument panel of that sealed vessel. Three colors
 * carry meaning, not decoration:
 *   seal   — contained / headless (the desired, isolated state)
 *   breach — host display attached (the isolation boundary is temporarily open)
 *   alert  — destructive
 */
export const t = {
  color: {
    void: "#0b0e13",
    panel: "#11161f",
    panel2: "#171e29",
    raise: "#1d2733",
    line: "#28313f",
    lineSoft: "#1b232f",
    text: "#e8edf4",
    dim: "#93a1b3",
    faint: "#5e6b7c",
    seal: "#23c9b8",
    sealDim: "#155f58",
    breach: "#e8a23d",
    breachDim: "#7a5a22",
    alert: "#ef5d6b",
  },
  font: {
    display: "'Space Grotesk', system-ui, sans-serif",
    body: "'IBM Plex Sans', system-ui, sans-serif",
    mono: "'IBM Plex Mono', ui-monospace, 'SF Mono', monospace",
  },
  radius: {
    sm: "8px",
    md: "12px",
    lg: "16px",
  },
} as const;

export const GlobalStyles = () => (
  <Global
    styles={css`
      *,
      *::before,
      *::after {
        box-sizing: border-box;
      }

      html,
      body,
      #root {
        margin: 0;
        height: 100%;
      }

      body {
        background:
          radial-gradient(
            900px 520px at 26% 8%,
            rgba(35, 201, 184, 0.08),
            transparent 60%
          ),
          ${t.color.void};
        color: ${t.color.text};
        font-family: ${t.font.body};
        font-size: 14px;
        line-height: 1.5;
        -webkit-font-smoothing: antialiased;
        text-rendering: optimizeLegibility;
        user-select: none;
        cursor: default;
        overflow: hidden;
      }

      ::-webkit-scrollbar {
        width: 10px;
        height: 10px;
      }
      ::-webkit-scrollbar-thumb {
        background: ${t.color.line};
        border-radius: 999px;
        border: 2px solid transparent;
        background-clip: padding-box;
      }
      ::-webkit-scrollbar-thumb:hover {
        background: ${t.color.raise};
        background-clip: padding-box;
      }

      :focus-visible {
        outline: 2px solid ${t.color.seal};
        outline-offset: 2px;
      }

      @media (prefers-reduced-motion: reduce) {
        * {
          animation-duration: 0.001ms !important;
          animation-iteration-count: 1 !important;
          transition-duration: 0.001ms !important;
        }
      }
    `}
  />
);

/** Shared utility: a tracked-out monospace "telemetry" label. */
export const eyebrow = css`
  font-family: ${t.font.mono};
  font-size: 10px;
  font-weight: 500;
  letter-spacing: 0.22em;
  text-transform: uppercase;
  color: ${t.color.faint};
`;
