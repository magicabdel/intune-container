import styled from "@emotion/styled";
import { t, eyebrow } from "../theme";

/* Destroy is its own tab (only shown when there's something to destroy). The
   confirmation lives on the page rather than in a modal. */

const Root = styled.section`
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  justify-content: center;
`;

const Inner = styled.div`
  width: min(560px, 100%);
  padding: 40px 32px;
`;

const Title = styled.h1`
  margin: 6px 0 10px;
  font-family: ${t.font.display};
  font-weight: 700;
  font-size: 26px;
  color: ${t.color.alert};
`;

const Lede = styled.p`
  margin: 0 0 22px;
  font-size: 14px;
  line-height: 1.6;
  color: ${t.color.dim};
  max-width: 56ch;
`;

const Choice = styled.label`
  display: flex;
  gap: 11px;
  align-items: flex-start;
  padding: 16px;
  border: 1px solid ${t.color.lineSoft};
  border-radius: ${t.radius.md};
  background: ${t.color.panel};
  cursor: pointer;
  margin-bottom: 24px;
  input {
    margin-top: 2px;
    accent-color: ${t.color.alert};
  }
  div {
    font-size: 13px;
    color: ${t.color.text};
  }
  span {
    display: block;
    margin-top: 3px;
    color: ${t.color.dim};
    line-height: 1.5;
  }
`;

const Destruct = styled.button`
  padding: 11px 20px;
  border: 1px solid ${t.color.alert};
  border-radius: ${t.radius.md};
  background: ${t.color.alert};
  color: #190406;
  font-family: ${t.font.display};
  font-weight: 600;
  font-size: 14px;
  cursor: pointer;
  &:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  &:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
`;

interface Props {
  purge: boolean;
  onPurgeChange: (v: boolean) => void;
  onDestroy: () => void;
  busy: boolean;
}

export function DestroyView({ purge, onPurgeChange, onDestroy, busy }: Props) {
  return (
    <Root>
      <Inner>
        <div css={eyebrow}>Irreversible</div>
        <Title>Destroy the container</Title>
        <Lede>
          Removes the container filesystem, its configuration, and the browser SSO
          manifests. You'll need to enroll again afterwards.
        </Lede>

        <Choice>
          <input
            type="checkbox"
            checked={purge}
            onChange={(e) => onPurgeChange(e.target.checked)}
          />
          <div>
            Also purge enrollment data
            <span>
              Deletes the persisted device registration and keyring. Leave unchecked to
              keep them for a future rebuild.
            </span>
          </div>
        </Choice>

        <Destruct disabled={busy} onClick={onDestroy}>
          {busy ? "Destroying…" : purge ? "Destroy and purge data" : "Destroy container"}
        </Destruct>
      </Inner>
    </Root>
  );
}
