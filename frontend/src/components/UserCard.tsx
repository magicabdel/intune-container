import styled from "@emotion/styled";
import { Account } from "../api";
import { t } from "../theme";

/* The signed-in identity, read from the broker (same source as sso-test). When
   no one is signed in it's a quiet placeholder, not an error. */

const Card = styled.div`
  width: 100%;
  display: flex;
  align-items: center;
  gap: 13px;
  padding: 12px 15px;
  border: 1px solid ${t.color.lineSoft};
  border-radius: ${t.radius.lg};
  background: ${t.color.panel};
`;

const Avatar = styled.div<{ empty?: boolean }>`
  flex: none;
  width: 38px;
  height: 38px;
  border-radius: 50%;
  display: grid;
  place-items: center;
  font-family: ${t.font.display};
  font-weight: 600;
  font-size: 14px;
  letter-spacing: 0.02em;
  color: ${(p) => (p.empty ? t.color.faint : t.color.seal)};
  background: ${(p) => (p.empty ? "transparent" : "rgba(35, 201, 184, 0.13)")};
  border: 1px solid ${(p) => (p.empty ? t.color.line : "transparent")};
`;

const Who = styled.div`
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
`;

const Name = styled.div<{ muted?: boolean }>`
  font-family: ${t.font.display};
  font-weight: 600;
  font-size: 14px;
  color: ${(p) => (p.muted ? t.color.dim : t.color.text)};
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

const Mail = styled.div`
  font-family: ${t.font.mono};
  font-size: 11.5px;
  color: ${t.color.faint};
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

const Bar = styled.div`
  height: 11px;
  border-radius: 3px;
  background: ${t.color.raise};
`;

function initials(name: string, username: string): string {
  const n = name.trim();
  if (n) {
    const parts = n.split(/\s+/);
    const first = parts[0]?.[0] ?? "";
    const last = parts.length > 1 ? (parts[parts.length - 1][0] ?? "") : "";
    const both = (first + last).toUpperCase();
    if (both) return both;
  }
  return (username.trim()[0] ?? "?").toUpperCase();
}

interface Props {
  account: Account | null;
  loading: boolean;
}

export function UserCard({ account, loading }: Props) {
  if (!account && loading) {
    return (
      <Card aria-hidden>
        <Avatar empty />
        <Who style={{ flex: 1 }}>
          <Bar style={{ width: "45%" }} />
          <Bar style={{ width: "65%" }} />
        </Who>
      </Card>
    );
  }

  if (!account) {
    return (
      <Card>
        <Avatar empty>·</Avatar>
        <Who>
          <Name muted>Not signed in</Name>
        </Who>
      </Card>
    );
  }

  return (
    <Card>
      <Avatar>{initials(account.name, account.username)}</Avatar>
      <Who>
        <Name>{account.name || account.username}</Name>
        {account.username && <Mail title={account.username}>{account.username}</Mail>}
      </Who>
    </Card>
  );
}
