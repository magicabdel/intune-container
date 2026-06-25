import { useState } from "react";
import styled from "@emotion/styled";
import { keyframes } from "@emotion/react";
import { t, eyebrow } from "../theme";

const fade = keyframes`from { opacity: 0 } to { opacity: 1 }`;
const rise = keyframes`
  from { transform: translateY(10px) scale(0.99); opacity: 0; }
  to   { transform: translateY(0) scale(1); opacity: 1; }
`;

const Backdrop = styled.div`
  position: fixed;
  inset: 0;
  background: rgba(4, 6, 9, 0.62);
  display: grid;
  place-items: center;
  z-index: 40;
  animation: ${fade} 0.16s ease;
`;

const Card = styled.div`
  width: min(430px, 92vw);
  background: ${t.color.panel};
  border: 1px solid ${t.color.line};
  border-radius: ${t.radius.lg};
  padding: 26px 26px 22px;
  box-shadow: 0 24px 60px rgba(0, 0, 0, 0.5);
  animation: ${rise} 0.2s ease;
`;

const Title = styled.h3`
  margin: 6px 0 8px;
  font-family: ${t.font.display};
  font-size: 19px;
  font-weight: 600;
`;

const Lead = styled.p`
  margin: 0 0 18px;
  color: ${t.color.dim};
  font-size: 13px;
  line-height: 1.5;
`;

const Field = styled.input`
  width: 100%;
  margin-bottom: 10px;
  padding: 11px 13px;
  border-radius: ${t.radius.sm};
  border: 1px solid ${t.color.line};
  background: ${t.color.void};
  color: ${t.color.text};
  font-family: ${t.font.mono};
  font-size: 13px;
  letter-spacing: 0.04em;
  &:focus {
    outline: none;
    border-color: ${t.color.seal};
  }
  &::placeholder {
    color: ${t.color.faint};
    letter-spacing: normal;
    font-family: ${t.font.body};
  }
`;

const Error = styled.div`
  color: ${t.color.alert};
  font-size: 12.5px;
  margin: 2px 0 12px;
`;

const Row = styled.div`
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  margin-top: 8px;
`;

const Button = styled.button<{ primary?: boolean }>`
  font-family: ${t.font.display};
  font-size: 13.5px;
  font-weight: 500;
  padding: 9px 18px;
  border-radius: ${t.radius.sm};
  cursor: pointer;
  border: 1px solid ${(p) => (p.primary ? t.color.seal : t.color.line)};
  background: ${(p) => (p.primary ? t.color.seal : "transparent")};
  color: ${(p) => (p.primary ? "#03110f" : t.color.text)};
  &:hover {
    filter: brightness(1.08);
    border-color: ${(p) => (p.primary ? t.color.seal : "#3a4658")};
  }
`;

interface Props {
  onCancel: () => void;
  onSubmit: (password: string) => void;
}

export function PasswordModal({ onCancel, onSubmit }: Props) {
  const [pw, setPw] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);

  const submit = () => {
    if (!pw) return setError("Enter a password.");
    if (pw !== confirm) return setError("Passwords don't match.");
    onSubmit(pw);
  };

  return (
    <Backdrop onMouseDown={onCancel}>
      <Card onMouseDown={(e) => e.stopPropagation()} role="dialog" aria-label="First-time setup">
        <div css={eyebrow}>First-time setup</div>
        <Title>Provision the container</Title>
        <Lead>
          Choose a password for the container user account. It's used only inside
          the sealed container — your sudo password is asked separately when needed.
        </Lead>
        <Field
          type="password"
          placeholder="Container password"
          autoFocus
          value={pw}
          onChange={(e) => setPw(e.target.value)}
        />
        <Field
          type="password"
          placeholder="Confirm password"
          value={confirm}
          onChange={(e) => setConfirm(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
        />
        {error && <Error>{error}</Error>}
        <Row>
          <Button onClick={onCancel}>Cancel</Button>
          <Button primary onClick={submit}>
            Provision &amp; enroll
          </Button>
        </Row>
      </Card>
    </Backdrop>
  );
}
