import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { t } from "../theme";

export type Tone = "default" | "primary" | "danger";

const spin = keyframes`
  to { transform: rotate(360deg); }
`;

const Btn = styled.button<{ tone: Tone }>`
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 3px;
  text-align: left;
  width: 100%;
  padding: 13px 15px 14px;
  border-radius: ${t.radius.md};
  border: 1px solid ${t.color.line};
  background: ${t.color.panel2};
  color: ${t.color.text};
  cursor: pointer;
  font-family: inherit;
  transition:
    transform 0.07s ease,
    border-color 0.16s ease,
    background 0.16s ease;

  /* a thin "rail" on the left encodes the button's tone */
  &::before {
    content: "";
    position: absolute;
    left: 0;
    top: 12px;
    bottom: 12px;
    width: 2px;
    border-radius: 0 2px 2px 0;
    background: ${(p) =>
      p.tone === "primary" ? t.color.seal : p.tone === "danger" ? t.color.alert : t.color.line};
    transition: background 0.16s ease;
  }

  &:hover:not(:disabled) {
    background: ${t.color.raise};
    border-color: ${(p) =>
      p.tone === "danger" ? t.color.alert : p.tone === "primary" ? t.color.seal : "#3a4658"};
    transform: translateY(-1px);
  }
  &:active:not(:disabled) {
    transform: translateY(0);
  }
  &:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
`;

const Title = styled.span<{ tone: Tone }>`
  font-family: ${t.font.display};
  font-weight: 500;
  font-size: 14.5px;
  letter-spacing: 0.01em;
  color: ${(p) => (p.tone === "danger" ? t.color.alert : t.color.text)};
`;

const Hint = styled.span`
  font-size: 12px;
  color: ${t.color.dim};
  line-height: 1.35;
`;

const Spinner = styled.span`
  position: absolute;
  top: 13px;
  right: 14px;
  width: 13px;
  height: 13px;
  border-radius: 50%;
  border: 2px solid ${t.color.line};
  border-top-color: ${t.color.seal};
  animation: ${spin} 0.7s linear infinite;
`;

interface Props {
  title: string;
  hint: string;
  tone?: Tone;
  disabled?: boolean;
  busy?: boolean;
  onClick: () => void;
}

export function ControlButton({ title, hint, tone = "default", disabled, busy, onClick }: Props) {
  return (
    <Btn tone={tone} disabled={disabled || busy} onClick={onClick} type="button">
      {busy && <Spinner aria-hidden />}
      <Title tone={tone}>{title}</Title>
      <Hint>{hint}</Hint>
    </Btn>
  );
}
