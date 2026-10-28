// Pure helpers for the `csx tabs --json` report. No Raycast or Node APIs here,
// so `node --test` can load this file directly.

export type AgentStatus = "busy" | "idle" | "unknown";

export interface Agent {
  tool: string;
  pid: number;
  tty: string | null;
  cwd: string | null;
  session_id: string | null;
  title: string | null;
  last_prompt: string | null;
  status: AgentStatus;
  updated_at: number | null;
  resume_command: string | null;
}

export interface Row {
  terminal: string;
  ref: string | null;
  title: string;
  cwd: string | null;
  tty: string | null;
  window: number;
  index: number;
  agent: Agent | null;
}

export interface Report {
  rows: Row[];
  warnings: string[];
}

const PROMPT_MAX = 80;

const TERMINAL_NAMES: Record<string, string> = {
  ghostty: "Ghostty",
  iterm2: "iTerm2",
  terminal: "Terminal",
  kitty: "kitty",
  wezterm: "WezTerm",
  tmux: "tmux",
};

const TOOL_NAMES: Record<string, string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
  devin: "Devin",
};

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function parseAgent(v: unknown): Agent | null {
  if (!isObject(v) || typeof v.tool !== "string") return null;
  const status = v.status === "busy" || v.status === "idle" ? v.status : "unknown";
  return {
    tool: v.tool,
    pid: num(v.pid) ?? 0,
    tty: str(v.tty),
    cwd: str(v.cwd),
    session_id: str(v.session_id),
    title: str(v.title),
    last_prompt: str(v.last_prompt),
    status,
    updated_at: num(v.updated_at),
    resume_command: str(v.resume_command),
  };
}

function parseRow(v: unknown): Row | null {
  if (!isObject(v)) return null;
  return {
    terminal: str(v.terminal) ?? "",
    ref: str(v.ref),
    title: str(v.title) ?? "",
    cwd: str(v.cwd),
    tty: str(v.tty),
    window: num(v.window) ?? 0,
    index: num(v.index) ?? 0,
    agent: parseAgent(v.agent),
  };
}

/** Parse `csx tabs --json` output. Throws when it is not a report. */
export function parseReport(text: string): Report {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new Error("csx printed something that is not JSON");
  }
  if (!isObject(v) || !Array.isArray(v.rows)) {
    throw new Error("csx printed JSON without a rows list");
  }
  const rows = v.rows.map(parseRow).filter((r): r is Row => r !== null);
  const warnings = Array.isArray(v.warnings) ? v.warnings.filter((w): w is string => typeof w === "string") : [];
  return { rows, warnings };
}

export function oneLine(s: string): string {
  return s.split(/\s+/).filter(Boolean).join(" ");
}

export function cut(s: string, max: number): string {
  const chars = Array.from(s);
  return chars.length <= max ? s : chars.slice(0, max - 1).join("") + "…";
}

/** `path` with `home` shortened to `~`. */
export function shortenHome(path: string, home: string | undefined): string {
  if (!home) return path;
  if (path === home) return "~";
  return path.startsWith(home + "/") ? "~" + path.slice(home.length) : path;
}

/** The agent's session title, else the tab title, else the tool id. */
export function rowTitle(r: Row): string {
  if (r.agent?.title) return r.agent.title;
  if (r.title) return r.title;
  return r.agent?.tool ?? "";
}

export function rowCwd(r: Row): string | null {
  return r.cwd ?? r.agent?.cwd ?? null;
}

export function terminalName(id: string): string {
  return TERMINAL_NAMES[id] ?? id;
}

export function toolName(id: string): string {
  return TOOL_NAMES[id] ?? id;
}

/** The last prompt on one line, cut to fit, or null. */
export function rowPrompt(r: Row): string | null {
  const p = r.agent?.last_prompt ? oneLine(r.agent.last_prompt) : "";
  return p ? cut(p, PROMPT_MAX) : null;
}

/** Words Raycast matches the search text against, besides the title. */
export function rowKeywords(r: Row): string[] {
  const words = [r.terminal, terminalName(r.terminal), r.title, rowCwd(r) ?? ""];
  if (r.agent) {
    words.push(r.agent.tool, toolName(r.agent.tool), r.agent.session_id ?? "", r.agent.last_prompt ?? "");
  }
  return words.flatMap((w) => oneLine(w).split(" ")).filter(Boolean);
}

/** A stable list key, also for rows without a ref. */
export function rowKey(r: Row, i: number): string {
  return r.ref ?? `${r.terminal}:${r.agent?.pid ?? ""}:${i}`;
}
