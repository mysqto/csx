import assert from "node:assert/strict";
import { test } from "node:test";
import {
  cut,
  oneLine,
  parseReport,
  rowCwd,
  rowKey,
  rowKeywords,
  rowPrompt,
  rowTitle,
  shortenHome,
  terminalName,
  toolName,
  type Row,
} from "../src/lib/rows.ts";

const agentRow = {
  terminal: "ghostty",
  ref: "ghostty:AB-1",
  title: "✳ fix login",
  cwd: "/Users/me/work/acme",
  tty: null,
  window: 1,
  index: 3,
  agent: {
    tool: "claude-code",
    pid: 42,
    tty: "ttys001",
    cwd: "/Users/me/work/acme",
    session_id: "0f1e2d3c",
    title: "Fix login",
    last_prompt: "run the\n  tests",
    status: "busy",
    updated_at: 1793156134829,
    resume_command: "cd '/Users/me/work/acme' && claude --resume 0f1e2d3c",
  },
};

const plainRow = {
  terminal: "iterm2",
  ref: "iterm2:w0t1p0",
  title: "zsh",
  cwd: null,
  tty: "ttys004",
  window: 2,
  index: 1,
  agent: null,
};

function report(rows: unknown[], warnings: unknown[] = []): string {
  return JSON.stringify({ rows, warnings });
}

test("parseReport keeps rows and warnings", () => {
  const r = parseReport(report([agentRow, plainRow], ["kitty: no socket"]));
  assert.equal(r.rows.length, 2);
  assert.deepEqual(r.warnings, ["kitty: no socket"]);
  assert.equal(r.rows[0].agent?.status, "busy");
  assert.equal(r.rows[1].agent, null);
});

test("parseReport rejects text that is not a report", () => {
  assert.throws(() => parseReport("error: boom"), /not JSON/);
  assert.throws(() => parseReport("{}"), /rows list/);
  assert.throws(() => parseReport("[]"), /rows list/);
});

test("parseReport tolerates missing and odd fields", () => {
  const r = parseReport(
    JSON.stringify({
      rows: [null, 7, { title: 5, agent: { tool: "codex", status: "asleep", pid: "x", updated_at: Infinity } }],
      warnings: ["ok", 3],
    }),
  );
  assert.equal(r.rows.length, 1);
  const row = r.rows[0];
  assert.equal(row.terminal, "");
  assert.equal(row.title, "");
  assert.equal(row.ref, null);
  assert.equal(row.agent?.status, "unknown");
  assert.equal(row.agent?.pid, 0);
  assert.equal(row.agent?.updated_at, null);
  assert.deepEqual(r.warnings, ["ok"]);
});

test("parseReport drops an agent without a tool", () => {
  const r = parseReport(report([{ ...plainRow, agent: { pid: 1 } }]));
  assert.equal(r.rows[0].agent, null);
});

test("parseReport keeps hostile text as is", () => {
  const title = 'He said "hi" \\ </script> 🚀';
  const r = parseReport(report([{ ...plainRow, title }]));
  assert.equal(r.rows[0].title, title);
});

test("oneLine and cut", () => {
  assert.equal(oneLine("  a\n\tb  c "), "a b c");
  assert.equal(cut("abc", 3), "abc");
  assert.equal(cut("abcd", 3), "ab…");
  assert.equal(cut("🚀🚀🚀🚀", 3), "🚀🚀…");
});

test("shortenHome", () => {
  assert.equal(shortenHome("/Users/me/x", "/Users/me"), "~/x");
  assert.equal(shortenHome("/Users/me", "/Users/me"), "~");
  assert.equal(shortenHome("/Users/meta/x", "/Users/me"), "/Users/meta/x");
  assert.equal(shortenHome("/Users/me/x", undefined), "/Users/me/x");
  assert.equal(shortenHome("/Users/me/x", ""), "/Users/me/x");
});

test("rowTitle prefers the agent title, then the tab title, then the tool", () => {
  const [a, p] = parseReport(report([agentRow, plainRow])).rows;
  assert.equal(rowTitle(a), "Fix login");
  assert.equal(rowTitle(p), "zsh");
  const bare: Row = { ...p, title: "", agent: { ...a.agent!, title: null } };
  assert.equal(rowTitle(bare), "claude-code");
  assert.equal(rowTitle({ ...p, title: "" }), "");
});

test("rowCwd falls back to the agent cwd", () => {
  const [a, p] = parseReport(report([agentRow, plainRow])).rows;
  assert.equal(rowCwd(a), "/Users/me/work/acme");
  assert.equal(rowCwd(p), null);
  assert.equal(rowCwd({ ...a, cwd: null, agent: { ...a.agent!, cwd: "/srv" } }), "/srv");
});

test("terminal and tool names", () => {
  assert.equal(terminalName("iterm2"), "iTerm2");
  assert.equal(terminalName("alacritty"), "alacritty");
  assert.equal(toolName("claude-code"), "Claude Code");
  assert.equal(toolName("devin"), "Devin");
  assert.equal(toolName("aider"), "aider");
});

test("rowPrompt flattens and cuts the last prompt", () => {
  const [a, p] = parseReport(report([agentRow, plainRow])).rows;
  assert.equal(rowPrompt(a), "run the tests");
  assert.equal(rowPrompt(p), null);
  assert.equal(rowPrompt({ ...a, agent: { ...a.agent!, last_prompt: " \n " } }), null);
  const long = rowPrompt({ ...a, agent: { ...a.agent!, last_prompt: "x".repeat(200) } });
  assert.equal(Array.from(long!).length, 80);
});

test("rowKeywords cover terminal, cwd, tool, session and prompt", () => {
  const [a, p] = parseReport(report([agentRow, plainRow])).rows;
  const kw = rowKeywords(a);
  for (const w of ["ghostty", "Ghostty", "/Users/me/work/acme", "claude-code", "Claude", "Code", "0f1e2d3c", "tests"]) {
    assert.ok(kw.includes(w), w);
  }
  assert.ok(!kw.includes(""));
  assert.deepEqual(rowKeywords(p), ["iterm2", "iTerm2", "zsh"]);
});

test("rowKey uses the ref, else a fallback that stays unique", () => {
  const [a, p] = parseReport(report([agentRow, plainRow])).rows;
  assert.equal(rowKey(a, 0), "ghostty:AB-1");
  assert.equal(rowKey({ ...a, ref: null }, 4), "ghostty:42:4");
  assert.equal(rowKey({ ...p, ref: null }, 5), "iterm2::5");
});
