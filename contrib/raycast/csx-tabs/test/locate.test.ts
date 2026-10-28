import assert from "node:assert/strict";
import { test } from "node:test";
import { binDirs, csxCandidates, describeFailure, extendedPath } from "../src/lib/locate.ts";

test("binDirs lists Homebrew, MacPorts and Cargo folders", () => {
  assert.deepEqual(binDirs("/Users/me"), [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/Users/me/.cargo/bin",
    "/Users/me/.local/bin",
  ]);
});

test("csxCandidates puts the preference first", () => {
  const c = csxCandidates("/Users/me", "  /custom/csx ");
  assert.equal(c[0], "/custom/csx");
  assert.equal(c[1], "/opt/homebrew/bin/csx");
  assert.equal(c.length, 6);
});

test("csxCandidates ignores an empty preference", () => {
  assert.equal(csxCandidates("/Users/me", "  ")[0], "/opt/homebrew/bin/csx");
  assert.equal(csxCandidates("/Users/me", undefined).length, 5);
});

test("extendedPath adds install folders once, before the old PATH", () => {
  const p = extendedPath("/Users/me", "/usr/bin:/opt/homebrew/bin:/x");
  const parts = p.split(":");
  assert.deepEqual(parts.slice(0, 5), ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/opt/homebrew/bin"]);
  assert.ok(parts.includes("/x"));
  assert.equal(parts.filter((d) => d === "/opt/homebrew/bin").length, 1);
  assert.equal(parts.filter((d) => d === "/usr/bin").length, 1);
});

test("extendedPath works without a PATH", () => {
  assert.ok(extendedPath("/Users/me", undefined).endsWith("/Users/me/.local/bin"));
  assert.ok(!extendedPath("/Users/me", "").includes("::"));
});

test("describeFailure explains a timeout first", () => {
  assert.match(describeFailure("error: boom", "Command failed", true), /did not answer.*Automation|allow it/);
});

test("describeFailure explains an old csx from the real clap output", () => {
  const clap =
    "error: unrecognized subcommand 'tabs'\n\n  tip: a similar subcommand exists: 'sync'\n\nUsage: csx [OPTIONS] [COMMAND]\n\nFor more information, try '--help'.\n";
  assert.match(describeFailure(clap, "x"), /Update csx to 0\.2\.0/);
});

test("describeFailure explains an old csx", () => {
  assert.match(describeFailure("error: unrecognized subcommand 'tabs'\n", "x"), /0\.2\.0/);
  assert.match(describeFailure("error: unexpected argument '--agents' found", "x"), /too old/);
});

test("describeFailure returns the first stderr line without its prefix", () => {
  assert.equal(
    describeFailure("\ncsx: the session in /dev/ttys9 is no longer running\nmore", "x"),
    "the session in /dev/ttys9 is no longer running",
  );
  assert.equal(describeFailure("error: boom", "x"), "boom");
  assert.equal(describeFailure("plain", "x"), "plain");
  assert.equal(describeFailure("  \n", "Command failed"), "Command failed");
});
