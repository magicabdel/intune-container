import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { Check } from "../api";
import { t, eyebrow } from "../theme";

const slideIn = keyframes`
  from { transform: translateX(16px); opacity: 0; }
  to   { transform: translateX(0);    opacity: 1; }
`;

const Backdrop = styled.div`
  position: fixed;
  inset: 0;
  background: rgba(4, 6, 9, 0.55);
  z-index: 30;
  display: flex;
  justify-content: flex-end;
`;

const Panel = styled.aside`
  width: min(420px, 92vw);
  height: 100%;
  background: ${t.color.panel};
  border-left: 1px solid ${t.color.line};
  display: flex;
  flex-direction: column;
  animation: ${slideIn} 0.22s ease;
`;

const Head = styled.header`
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 20px 22px 16px;
  border-bottom: 1px solid ${t.color.lineSoft};
`;

const Title = styled.h2`
  margin: 4px 0 0;
  font-family: ${t.font.display};
  font-size: 18px;
  font-weight: 600;
`;

const Close = styled.button`
  border: 1px solid ${t.color.line};
  background: transparent;
  color: ${t.color.dim};
  border-radius: 8px;
  width: 30px;
  height: 30px;
  font-size: 16px;
  cursor: pointer;
  &:hover {
    color: ${t.color.text};
    background: ${t.color.panel2};
  }
`;

const List = styled.ul`
  list-style: none;
  margin: 0;
  padding: 8px 14px 20px;
  overflow-y: auto;
`;

const GLYPH: Record<Check["status"], string> = { ok: "●", warn: "▲", fail: "✕", skip: "○" };
const HUE: Record<Check["status"], string> = {
  ok: t.color.seal,
  warn: t.color.breach,
  fail: t.color.alert,
  skip: t.color.faint,
};

const Row = styled.li`
  display: grid;
  grid-template-columns: 16px 1fr;
  gap: 12px;
  padding: 12px 8px;
  border-bottom: 1px solid ${t.color.lineSoft};
  &:last-of-type {
    border-bottom: none;
  }
`;

const Glyph = styled.span`
  font-size: 11px;
  line-height: 22px;
`;

const Label = styled.div`
  font-family: ${t.font.mono};
  font-size: 12px;
  letter-spacing: 0.04em;
  color: ${t.color.text};
`;

const Detail = styled.div`
  font-size: 12.5px;
  color: ${t.color.dim};
  margin-top: 2px;
  word-break: break-word;
`;

const Empty = styled.div`
  padding: 28px 8px;
  text-align: center;
  color: ${t.color.faint};
  font-family: ${t.font.mono};
  font-size: 12px;
`;

interface Props {
  checks: Check[] | null;
  loading: boolean;
  onClose: () => void;
}

export function DoctorPanel({ checks, loading, onClose }: Props) {
  return (
    <Backdrop onMouseDown={onClose}>
      <Panel onMouseDown={(e) => e.stopPropagation()} role="dialog" aria-label="Diagnostics">
        <Head>
          <div>
            <div css={eyebrow}>System diagnostics</div>
            <Title>Health report</Title>
          </div>
          <Close onClick={onClose} aria-label="Close diagnostics">
            ×
          </Close>
        </Head>
        <List>
          {loading && <Empty>Running checks…</Empty>}
          {!loading && checks && checks.length === 0 && <Empty>No results.</Empty>}
          {!loading &&
            checks?.map((c, i) => (
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
    </Backdrop>
  );
}
