import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { t } from "../theme";

export type ToastTone = "info" | "ok" | "error";

export interface ToastData {
  id: number;
  tone: ToastTone;
  message: string;
}

const rise = keyframes`
  from { transform: translateY(12px); opacity: 0; }
  to   { transform: translateY(0); opacity: 1; }
`;

const Wrap = styled.div`
  position: fixed;
  left: 50%;
  bottom: 22px;
  transform: translateX(-50%);
  z-index: 50;
  display: flex;
  flex-direction: column;
  gap: 8px;
  align-items: center;
  pointer-events: none;
`;

const HUE: Record<ToastTone, string> = {
  info: t.color.line,
  ok: t.color.seal,
  error: t.color.alert,
};

const Item = styled.div<{ tone: ToastTone }>`
  display: flex;
  align-items: center;
  gap: 10px;
  max-width: 78vw;
  padding: 10px 16px 10px 14px;
  border-radius: 999px;
  background: ${t.color.panel2};
  border: 1px solid ${(p) => HUE[p.tone]};
  color: ${t.color.text};
  font-size: 13px;
  box-shadow: 0 12px 30px rgba(0, 0, 0, 0.4);
  animation: ${rise} 0.18s ease;

  &::before {
    content: "";
    width: 7px;
    height: 7px;
    border-radius: 50%;
    flex: none;
    background: ${(p) => HUE[p.tone]};
  }
`;

export function Toasts({ items }: { items: ToastData[] }) {
  return (
    <Wrap aria-live="polite">
      {items.map((x) => (
        <Item key={x.id} tone={x.tone}>
          {x.message}
        </Item>
      ))}
    </Wrap>
  );
}
