// Pure helpers for the `csx tabs --json` report. No Raycast or Node APIs here,
// so `node --test` can load this file directly.

export type AgentStatus = "busy" | "idle" | "unknown";

/** Who started a session: the person, a program, or another agent. `null`
 * when csx could not tell. */
export type Origin = "user" | "automated" | "subagent" | null;

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
  origin: Origin;
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

/** A session whose tab is closed, that csx can resume. */
export interface ClosedSession {
  tool: string;
  session_id: string;
  title: string | null;
  cwd: string;
  last_prompt: string | null;
  updated_at: number | null;
  resume_command: string;
  origin: Origin;
}

export interface Report {
  rows: Row[];
  warnings: string[];
  closed: ClosedSession[];
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
  gemini: "Gemini CLI",
  opencode: "opencode",
  copilot: "Copilot CLI",
  "cursor-agent": "Cursor Agent",
  aider: "Aider",
  qwen: "Qwen Code",
  goose: "Goose",
  amp: "Amp",
  crush: "Crush",
  droid: "Droid",
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

function parseOrigin(v: unknown): Origin {
  return v === "user" || v === "automated" || v === "subagent" ? v : null;
}

/** The origin word to show for a session that csx hides by default
 * (`automated` or `subagent`), else null. */
export function originTag(origin: Origin): string | null {
  return origin === "automated" || origin === "subagent" ? origin : null;
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
    origin: parseOrigin(v.origin),
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

function parseClosed(v: unknown): ClosedSession | null {
  if (!isObject(v) || typeof v.tool !== "string" || typeof v.session_id !== "string") return null;
  return {
    tool: v.tool,
    session_id: v.session_id,
    title: str(v.title),
    cwd: str(v.cwd) ?? "",
    last_prompt: str(v.last_prompt),
    updated_at: num(v.updated_at),
    resume_command: str(v.resume_command) ?? "",
    origin: parseOrigin(v.origin),
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
  const closed = Array.isArray(v.closed) ? v.closed.map(parseClosed).filter((c): c is ClosedSession => c !== null) : [];
  return { rows, warnings, closed };
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

/** Words Raycast matches the search text against, besides the title.
 * Raycast matches a keyword from its start, so the cwd also goes in as its
 * folder names. */
export function rowKeywords(r: Row): string[] {
  const cwd = rowCwd(r) ?? "";
  const words = [r.terminal, terminalName(r.terminal), r.title, cwd, ...cwd.split("/")];
  if (r.agent) {
    words.push(r.agent.tool, toolName(r.agent.tool), r.agent.session_id ?? "", r.agent.last_prompt ?? "");
  }
  return [...new Set(words.flatMap((w) => oneLine(w).split(" ")).filter(Boolean))];
}

/** A list key that stays unique when two rows share a ref, such as two
 * agents on one tty. */
export function rowKey(r: Row, i: number): string {
  return `${r.ref ?? `${r.terminal}:${r.agent?.pid ?? ""}`}#${i}`;
}

/** The session title, else its id. */
export function closedTitle(c: ClosedSession): string {
  return c.title || c.session_id;
}

/** The last prompt of a closed session on one line, cut to fit, or null. */
export function closedPrompt(c: ClosedSession): string | null {
  const p = c.last_prompt ? oneLine(c.last_prompt) : "";
  return p ? cut(p, PROMPT_MAX) : null;
}

/** Search words for a closed session, like `rowKeywords`. */
export function closedKeywords(c: ClosedSession): string[] {
  const words = [c.tool, toolName(c.tool), c.cwd, ...c.cwd.split("/"), c.session_id, c.last_prompt ?? ""];
  return [...new Set(words.flatMap((w) => oneLine(w).split(" ")).filter(Boolean))];
}
