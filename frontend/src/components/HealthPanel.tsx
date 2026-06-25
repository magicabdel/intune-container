import { useEffect, useState } from "react";
import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { Check } from "../api";
import { t, eyebrow } from "../theme";

/* The vessel's live instrument readout: the diagnostics, always on screen. */

const spin = keyframes`
  to { transform: rotate(360deg); }
`;

const Panel = styled.section`
  display: flex;
  flex-direction: column;
  border: 1px solid ${t.color.lineSoft};
  border-radius: ${t.radius.lg};
  background: ${t.color.panel};
  padding: 16px 18px 6px;
`;

const Head = styled.header`
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 12px;
  padding-bottom: 14px;
  border-bottom: 1px solid ${t.color.lineSoft};
`;

const Verdict = styled.div<{ hue: string }>`
  display: flex;
  align-items: baseline;
  gap: 9px;
  font-family: ${t.font.display};
  font-size: 17px;
  font-weight: 600;
  color: ${(p) => p.hue};
  &::before {
    content: "";
    align-self: center;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: ${(p) => p.hue};
    box-shadow: 0 0 0 4px ${(p) => p.hue}22;
  }
`;

const Refresh = styled.button`
  display: inline-flex;
  align-items: center;
  gap: 7px;
  border: none;
  background: transparent;
  cursor: pointer;
  font-family: ${t.font.mono};
  font-size: 10.5px;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: ${t.color.faint};
  padding: 4px 2px;
  transition: color 0.15s ease;
  &:hover {
    color: ${t.color.dim};
  }
  svg {
    width: 12px;
    height: 12px;
  }
  &[data-busy="true"] svg {
    animation: ${spin} 0.8s linear infinite;
    color: ${t.color.seal};
  }
`;

const Meta = styled.div`
  display: flex;
  flex-wrap: wrap;
  gap: 6px 16px;
  padding: 12px 2px 2px;
  span {
    font-family: ${t.font.mono};
    font-size: 11px;
  }
  b {
    font-weight: 400;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: ${t.color.faint};
    margin-right: 6px;
  }
`;

const List = styled.ul`
  list-style: none;
  margin: 0;
  padding: 6px 0 0;
`;

const GLYPH: Record<Check["status"], string> = {
  ok: "●",
  warn: "▲",
  fail: "✕",
  skip: "○",
};
const HUE: Record<Check["status"], string> = {
  ok: t.color.seal,
  warn: t.color.breach,
  fail: t.color.alert,
  skip: t.color.faint,
};

const Row = styled.li`
  display: grid;
  grid-template-columns: 16px 1fr;
  gap: 13px;
  padding: 12px 2px;
  border-bottom: 1px solid ${t.color.lineSoft};
  &:last-of-type {
    border-bottom: none;
  }
`;

const Glyph = styled.span`
  font-size: 10px;
  line-height: 20px;
  text-align: center;
`;

const Label = styled.div`
  font-family: ${t.font.mono};
  font-size: 12px;
  letter-spacing: 0.05em;
  color: ${t.color.text};
`;

const Detail = styled.div`
  font-size: 12.5px;
  color: ${t.color.dim};
  margin-top: 3px;
  line-height: 1.45;
  word-break: break-word;
`;

const Empty = styled.div`
  padding: 40px 8px;
  text-align: center;
  color: ${t.color.faint};
  font-family: ${t.font.mono};
  font-size: 12px;
  letter-spacing: 0.04em;
`;

const RefreshIcon = (
  <svg viewBox="0 0 16 16" fill="none" aria-hidden>
    <path
      d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9M13.5 2v3h-3"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  </svg>
);

export interface Health {
  hue: string;
  label: string;
}

/** A one-line verdict over the checks, worst status wins. */
export function summarize(checks: Check[] | null, loading: boolean): Health {
  if (!checks)
    return {
      hue: t.color.faint,
      label: loading ? "Checking…" : "Not checked yet",
    };
  const fail = checks.filter((c) => c.status === "fail").length;
  const warn = checks.filter((c) => c.status === "warn").length;
  if (fail)
    return {
      hue: t.color.alert,
      label: `${fail} ${fail === 1 ? "issue needs" : "issues need"} attention`,
    };
  if (warn)
    return {
      hue: t.color.breach,
      label: `${warn} ${warn === 1 ? "warning" : "warnings"}`,
    };
  return { hue: t.color.seal, label: "All checks passing" };
}

interface MetaItem {
  k: string;
  v: string;
  hue?: string;
}

interface Props {
  checks: Check[] | null;
  loading: boolean;
  checkedAt: number | null;
  meta?: MetaItem[];
  onRefresh: () => void;
}

export function HealthPanel({
  checks,
  loading,
  checkedAt,
  meta,
  onRefresh,
}: Props) {
  const health = summarize(checks, loading);
  const ago = useRelativeTime(checkedAt);

  return (
    <Panel>
      <Head>
        <div>
          <div css={eyebrow}>System health</div>
          <Verdict hue={health.hue}>{health.label}</Verdict>
        </div>
        <Refresh
          onClick={onRefresh}
          data-busy={loading}
          disabled={loading}
          aria-label="Re-run checks"
        >
          {RefreshIcon}
          {loading ? "Checking" : checkedAt ? ago : "Run checks"}
        </Refresh>
      </Head>
      {meta && meta.length > 0 && (
        <Meta>
          {meta.map((m) => (
            <span key={m.k}>
              <b>{m.k}</b>
              <span style={m.hue ? { color: m.hue } : { color: t.color.text }}>
                {m.v}
              </span>
            </span>
          ))}
        </Meta>
      )}
      <List>
        {!checks && !loading && (
          <Empty>Run the checks to read the vessel's instruments.</Empty>
        )}
        {!checks && loading && <Empty>Reading instruments…</Empty>}
        {checks?.length === 0 && <Empty>No results.</Empty>}
        {checks?.map((c, i) => (
          <Row key={i}>
            <Glyph style={{ color: HUE[c.status] }}>{GLYPH[c.status]}</Glyph>
            <div>
              <Label>{c.label}</Label>
              {c.detail && <Detail>{c.detail}</Detail>}
            </div>
          </Row>
        ))}
      </List>
    </Panel>
  );
}

/** "just now" / "12s ago" / "3m ago", ticking once a second. */
function useRelativeTime(at: number | null): string {
  const [, setNow] = useState(Date.now());
  useEffect(() => {
    if (at === null) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [at]);
  if (at === null) return "";
  const s = Math.max(0, Math.round((Date.now() - at) / 1000));
  if (s < 3) return "just now";
  if (s < 60) return `${s}s ago`;
  return `${Math.round(s / 60)}m ago`;
}
