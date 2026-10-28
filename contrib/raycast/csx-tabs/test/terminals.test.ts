import assert from "node:assert/strict";
import { test } from "node:test";
import { parseTerminals, resumeFailure, resumeTitle, savedChoice } from "../src/lib/terminals.ts";

const installed = [
  { id: "ghostty", name: "Ghostty" },
  { id: "iterm2", name: "iTerm2" },
];

test("parseTerminals reads the csx list", () => {
  assert.deepEqual(parseTerminals(JSON.stringify(installed)), installed);
});

test("parseTerminals skips malformed entries", () => {
  const text = JSON.stringify([{ id: "kitty", name: "kitty" }, { id: 3 }, "x", null, { id: "a" }, { name: "b" }]);
  assert.deepEqual(parseTerminals(text), [{ id: "kitty", name: "kitty" }]);
});

test("parseTerminals throws on text that is not a JSON list", () => {
  assert.throws(() => parseTerminals("nope"), /not JSON/);
  assert.throws(() => parseTerminals("{}"), /not a list/);
});

test("resumeTitle names the saved terminal or asks for one", () => {
  assert.equal(resumeTitle({ id: "ghostty", name: "Ghostty" }), "Resume in Ghostty");
  assert.equal(resumeTitle(undefined), "Resume Session…");
});

test("savedChoice finds the saved terminal among the installed ones", () => {
  assert.deepEqual(savedChoice("iterm2", installed), installed[1]);
});

test("savedChoice is undefined when unsaved or no longer installed", () => {
  assert.equal(savedChoice(undefined, installed), undefined);
  assert.equal(savedChoice("", installed), undefined);
  assert.equal(savedChoice("wezterm", installed), undefined);
});

test("resumeFailure trims one trailing period and adds the hint", () => {
  assert.equal(resumeFailure("no tab opened."), "no tab opened. Use Resume in Another Terminal to pick another.");
  assert.equal(resumeFailure("no tab opened"), "no tab opened. Use Resume in Another Terminal to pick another.");
  assert.equal(resumeFailure("no tab opened.\n"), "no tab opened. Use Resume in Another Terminal to pick another.");
});
