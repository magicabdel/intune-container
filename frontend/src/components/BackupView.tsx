import { useEffect, useState } from "react";
import styled from "@emotion/styled";
import { api } from "../api";
import { t, eyebrow } from "../theme";

/* Back up & restore enrollment state — its own page so the two actions have room
   to explain what they do. The device registration + keyring live outside the
   rootfs, so a backup lets you skip re-enrolling after a rebuild or on a new box. */

const Root = styled.section`
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  justify-content: center;
`;

const Inner = styled.div`
  width: min(640px, 100%);
  padding: 40px 32px;
`;

const Title = styled.h1`
  margin: 6px 0 10px;
  font-family: ${t.font.display};
  font-weight: 700;
  font-size: 26px;
  letter-spacing: 0.01em;
`;

const Lede = styled.p`
  margin: 0 0 24px;
  font-size: 14px;
  line-height: 1.6;
  color: ${t.color.dim};
  max-width: 56ch;
`;

const PathCard = styled.div`
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 16px;
  padding: 13px 16px;
  border: 1px solid ${t.color.lineSoft};
  border-radius: ${t.radius.md};
  background: ${t.color.panel};
  margin-bottom: 26px;
  dt {
    font-family: ${t.font.mono};
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: ${t.color.faint};
  }
  dd {
    margin: 0;
    font-family: ${t.font.mono};
    font-size: 12px;
    color: ${t.color.text};
    text-align: right;
    word-break: break-all;
  }
`;

const Actions = styled.div`
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 14px;
  @media (max-width: 560px) {
    grid-template-columns: 1fr;
  }
`;

const Card = styled.div`
  border: 1px solid ${t.color.line};
  border-radius: ${t.radius.lg};
  background: ${t.color.panel2};
  padding: 20px 20px 22px;
  display: flex;
  flex-direction: column;
  h3 {
    margin: 8px 0 6px;
    font-family: ${t.font.display};
    font-size: 16px;
    font-weight: 600;
  }
  p {
    margin: 0 0 18px;
    font-size: 13px;
    line-height: 1.5;
    color: ${t.color.dim};
    flex: 1;
  }
`;

const Primary = styled.button`
  padding: 11px 16px;
  border: none;
  border-radius: ${t.radius.md};
  background: ${t.color.seal};
  color: #06201d;
  font-family: ${t.font.display};
  font-weight: 600;
  font-size: 13.5px;
  cursor: pointer;
  transition: filter 0.15s ease;
  &:hover:not(:disabled) {
    filter: brightness(1.07);
  }
  &:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
`;

const Outline = styled(Primary)`
  background: transparent;
  border: 1px solid ${t.color.line};
  color: ${t.color.text};
  font-weight: 500;
  &:hover:not(:disabled) {
    filter: none;
    border-color: ${t.color.seal};
    background: ${t.color.panel};
  }
`;

const Note = styled.div`
  margin-top: 18px;
  font-size: 12.5px;
  color: ${t.color.faint};
`;

interface Props {
  ready: boolean;
  backingUp: boolean;
  restoring: boolean;
  onBackup: () => void;
  onRestore: () => void;
}

export function BackupView({ ready, backingUp, restoring, onBackup, onRestore }: Props) {
  const [defaultPath, setDefaultPath] = useState<string>("");

  useEffect(() => {
    api
      .defaultBackupPath()
      .then(setDefaultPath)
      .catch(() => setDefaultPath(""));
  }, []);

  return (
    <Root>
      <Inner>
        <div css={eyebrow}>Enrollment data</div>
        <Title>Back up &amp; restore</Title>
        <Lede>
          Your Entra device registration and keyring live outside the container's
          filesystem, so they survive rebuilds. A backup bundles them into a single
          archive — restore it to skip re-enrolling after a reset or on another machine.
        </Lede>

        <PathCard>
          <dt>Default location</dt>
          <dd>{defaultPath || "—"}</dd>
        </PathCard>

        <Actions>
          <Card>
            <div css={eyebrow}>Save</div>
            <h3>Back up enrollment</h3>
            <p>Write the current registration and keyring to an archive you choose.</p>
            <Primary disabled={!ready || backingUp} onClick={onBackup}>
              {backingUp ? "Backing up…" : "Back up enrollment"}
            </Primary>
          </Card>

          <Card>
            <div css={eyebrow}>Load</div>
            <h3>Restore enrollment</h3>
            <p>Replace the current state with a backup archive. The container restarts.</p>
            <Outline disabled={!ready || restoring} onClick={onRestore}>
              {restoring ? "Restoring…" : "Restore from a backup"}
            </Outline>
          </Card>
        </Actions>

        {!ready && (
          <Note>Set up the container first — enroll this device from the Console.</Note>
        )}
      </Inner>
    </Root>
  );
}
