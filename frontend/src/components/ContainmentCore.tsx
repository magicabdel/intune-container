import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { Phase } from "../api";
import { t } from "../theme";

const breathe = keyframes`
  0%, 100% { opacity: 0.35; transform: scale(1); }
  50%      { opacity: 0.7;  transform: scale(1.04); }
`;

const Svg = styled.svg`
  width: 100%;
  height: auto;
  max-width: 240px;
  display: block;
  overflow: visible;

  .glow {
    transform-origin: 120px 120px;
    opacity: 0.18;
  }
  &[data-active="true"] .glow {
    animation: ${breathe} 3.6s ease-in-out infinite;
  }
  .arc {
    transition:
      stroke-dasharray 0.6s ease,
      stroke 0.4s ease;
  }
`;

interface Palette {
  ring: string;
  hex: string;
  cube: string;
  glow: string;
  frac: number;
  active: boolean;
}

function palette(phase: Phase): Palette {
  switch (phase) {
    case "sealed":
      return {
        ring: t.color.seal,
        hex: t.color.seal,
        cube: t.color.seal,
        glow: t.color.seal,
        frac: 1,
        active: true,
      };
    case "open":
      return {
        ring: t.color.breach,
        hex: t.color.breach,
        cube: t.color.breach,
        glow: t.color.breach,
        frac: 0.68,
        active: true,
      };
    case "dormant":
      return {
        ring: "#54657a",
        hex: "#3b4757",
        cube: "#54657a",
        glow: "#54657a",
        frac: 0,
        active: false,
      };
    default:
      return {
        ring: "#2c3543",
        hex: "#2c3543",
        cube: "#39434f",
        glow: "#2c3543",
        frac: 0,
        active: false,
      };
  }
}

const R = 72;
const C = 2 * Math.PI * R;
const HEX = "M214 120 L167 201.4 L73 201.4 L26 120 L73 38.6 L167 38.6 Z";

export function ContainmentCore({ phase }: { phase: Phase }) {
  const p = palette(phase);
  const dash = `${(p.frac * C).toFixed(1)} ${C.toFixed(1)}`;

  return (
    <Svg
      viewBox="0 0 240 240"
      data-active={p.active ? "true" : "false"}
      role="img"
      aria-label="Containment status"
    >
      <defs>
        <filter id="coreGlow" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="9" />
        </filter>
      </defs>

      {/* ambient glow (breathes when the vessel is live) */}
      <circle
        className="glow"
        cx="120"
        cy="120"
        r="70"
        fill={p.glow}
        filter="url(#coreGlow)"
      />

      {/* chamber wall */}
      <path
        d={HEX}
        fill="none"
        stroke={p.hex}
        strokeWidth="2"
        strokeLinejoin="round"
        opacity="0.85"
      />
      <path d={HEX} fill={p.hex} opacity="0.05" />

      {/* tick ring */}
      <circle
        cx="120"
        cy="120"
        r={R}
        fill="none"
        stroke={t.color.line}
        strokeWidth="2"
        strokeDasharray="1.5 8.5"
        opacity="0.6"
      />

      {/* containment arc — full when sealed, broken (gap) when the viewport is open */}
      <circle
        className="arc"
        cx="120"
        cy="120"
        r={R}
        fill="none"
        stroke={p.ring}
        strokeWidth="3.5"
        strokeLinecap="round"
        strokeDasharray={dash}
        transform="rotate(-90 120 120)"
      />

      {/* the contained agent (a small cube) */}
      <g opacity={phase === "unprovisioned" ? 0.5 : 1}>
        <path
          d="M120 96 L146 110 L120 124 L94 110 Z"
          fill={p.cube}
          opacity="0.95"
        />
        <path
          d="M94 110 L120 124 L120 154 L94 140 Z"
          fill={p.cube}
          opacity="0.42"
        />
        <path
          d="M146 110 L120 124 L120 154 L146 140 Z"
          fill={p.cube}
          opacity="0.65"
        />
      </g>
    </Svg>
  );
}
