import { useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import styled from "@emotion/styled";
import { api } from "../api";
import { t } from "../theme";

const MAX_FETCH = 2000;
const MAX_SHOWN = 800;

const Root = styled.section`
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
`;

const Toolbar = styled.div`
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 14px 24px;
  border-bottom: 1px solid ${t.color.lineSoft};
  flex: none;
`;

const Search = styled.input`
  flex: 1;
  min-width: 120px;
  padding: 8px 12px;
  border-radius: ${t.radius.sm};
  border: 1px solid ${t.color.line};
  background: ${t.color.void};
  color: ${t.color.text};
  font-family: ${t.font.mono};
  font-size: 12.5px;
  &:focus {
    outline: none;
    border-color: ${t.color.seal};
  }
  &::placeholder {
    color: ${t.color.faint};
  }
`;

const Toggle = styled.button<{ on: boolean }>`
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 7px 13px;
  border-radius: 999px;
  cursor: pointer;
  font-family: ${t.font.mono};
  font-size: 11px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  border: 1px solid ${(p) => (p.on ? t.color.seal : t.color.line)};
  background: ${(p) => (p.on ? "rgba(35, 201, 184, 0.1)" : "transparent")};
  color: ${(p) => (p.on ? t.color.seal : t.color.dim)};
  &::before {
    content: "";
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: ${(p) => (p.on ? t.color.seal : t.color.faint)};
  }
`;

const Action = styled.button`
  padding: 7px 13px;
  border-radius: ${t.radius.sm};
  border: 1px solid ${t.color.line};
  background: transparent;
  color: ${t.color.dim};
  cursor: pointer;
  font-family: ${t.font.mono};
  font-size: 11px;
  letter-spacing: 0.06em;
  text-transform: uppercase;
  &:hover {
    color: ${t.color.text};
    border-color: #3a4658;
  }
`;

const Stream = styled.div`
  flex: 1;
  min-height: 0;
  overflow: auto;
  padding: 10px 0;
  font-family: ${t.font.mono};
  font-size: 12px;
  line-height: 1.55;
`;

const Line = styled.div<{ hue: string }>`
  padding: 0 24px;
  white-space: pre-wrap;
  word-break: break-word;
  color: ${(p) => p.hue};
  mark {
    background: rgba(232, 162, 61, 0.28);
    color: ${t.color.text};
    border-radius: 2px;
  }
  &:hover {
    background: ${t.color.panel};
  }
`;

const Empty = styled.div`
  padding: 40px 24px;
  text-align: center;
  color: ${t.color.faint};
  font-family: ${t.font.mono};
  font-size: 12px;
`;

const Count = styled.span`
  font-family: ${t.font.mono};
  font-size: 11px;
  color: ${t.color.faint};
  white-space: nowrap;
`;

function levelHue(line: string): string {
  if (/\bERROR\b/.test(line)) return t.color.alert;
  if (/\bWARN\b/.test(line)) return t.color.breach;
  if (/\bDEBUG\b|\bTRACE\b/.test(line)) return t.color.faint;
  if (/\bINFO\b/.test(line)) return t.color.text;
  return t.color.dim;
}

function highlight(line: string, query: string): ReactNode {
  if (!query) return line;
  const q = query.toLowerCase();
  const out: ReactNode[] = [];
  let i = 0;
  let key = 0;
  const lower = line.toLowerCase();
  while (i < line.length) {
    const at = lower.indexOf(q, i);
    if (at === -1) {
      out.push(line.slice(i));
      break;
    }
    if (at > i) out.push(line.slice(i, at));
    out.push(<mark key={key++}>{line.slice(at, at + q.length)}</mark>);
    i = at + q.length;
  }
  return out;
}

export function LogsView() {
  const [text, setText] = useState("");
  const [query, setQuery] = useState("");
  const [follow, setFollow] = useState(true);
  const streamRef = useRef<HTMLDivElement>(null);

  const fetchLog = async () => {
    try {
      setText(await api.readLog(MAX_FETCH));
    } catch {
      /* keep previous */
    }
  };

  useEffect(() => {
    fetchLog();
    if (!follow) return;
    const id = setInterval(fetchLog, 2000);
    return () => clearInterval(id);
  }, [follow]);

  const allLines = useMemo(() => (text ? text.split("\n") : []), [text]);

  const lines = useMemo(() => {
    if (query) {
      const q = query.toLowerCase();
      return allLines
        .filter((l) => l.toLowerCase().includes(q))
        .slice(-MAX_SHOWN);
    }
    return allLines.slice(-MAX_SHOWN);
  }, [allLines, query]);

  useEffect(() => {
    if (follow && !query && streamRef.current) {
      streamRef.current.scrollTop = streamRef.current.scrollHeight;
    }
  }, [lines, follow, query]);

  const clear = async () => {
    try {
      await api.clearLog();
      setText("");
    } catch {
      /* ignore */
    }
  };

  return (
    <Root>
      <Toolbar>
        <Search
          placeholder="Filter log…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <Count>
          {query
            ? `${lines.length} match${lines.length === 1 ? "" : "es"}`
            : `${allLines.length} lines`}
        </Count>
        <Toggle
          on={follow}
          onClick={() => setFollow((f) => !f)}
          title="Auto-refresh and scroll to newest"
        >
          Follow
        </Toggle>
        <Action onClick={fetchLog} title="Refresh now">
          Refresh
        </Action>
        <Action onClick={clear} title="Truncate the log file">
          Clear
        </Action>
      </Toolbar>

      <Stream ref={streamRef}>
        {lines.length === 0 ? (
          <Empty>{query ? "No matching lines." : "No log output yet."}</Empty>
        ) : (
          lines.map((line, i) => (
            <Line key={i} hue={levelHue(line)}>
              {highlight(line, query)}
            </Line>
          ))
        )}
      </Stream>
    </Root>
  );
}
