import { execFile } from "node:child_process";
import { accessSync, constants } from "node:fs";
import { homedir } from "node:os";
import { getPreferenceValues } from "@raycast/api";
import { csxCandidates, describeFailure, extendedPath } from "./locate";
import { parseReport, Report } from "./rows";
import { parseTerminals, TerminalChoice } from "./terminals";

/** Thrown when no csx binary is found. */
export class CsxMissingError extends Error {
  constructor() {
    super("csx is not installed");
  }
}

function isExecutable(path: string): boolean {
  try {
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

export function findCsx(): string {
  const { csxPath } = getPreferenceValues<Preferences>();
  const bin = csxCandidates(homedir(), csxPath).find(isExecutable);
  if (!bin) throw new CsxMissingError();
  return bin;
}

function run(args: string[], timeout: number): Promise<string> {
  const bin = findCsx();
  const env = { ...process.env, PATH: extendedPath(homedir(), process.env.PATH) };
  return new Promise((resolve, reject) => {
    execFile(bin, args, { env, timeout, maxBuffer: 16 * 1024 * 1024 }, (err, stdout, stderr) => {
      if (err) reject(new Error(describeFailure(String(stderr), err.message, err.killed)));
      else resolve(String(stdout));
    });
  });
}

/** List tabs; only those that run an agent, plus closed sessions, when `agentsOnly`. */
export async function listTabs(agentsOnly: boolean): Promise<Report> {
  const args = ["tabs", "--json", ...(agentsOnly ? ["--agents", "--closed", "--closed-limit", "200"] : [])];
  return parseReport(await run(args, 60_000));
}

/** Bring the tab named by `ref` to the front. */
export async function focusTab(ref: string): Promise<void> {
  await run(["tabs", "focus", "--", ref], 10_000);
}

/** The terminals csx can open a session in. Only installed ones. */
export async function listTerminals(): Promise<TerminalChoice[]> {
  return parseTerminals(await run(["tabs", "terminals", "--json"], 10_000));
}

/** Resume a closed session in the terminal `terminal`. Only ids from csx go in.
 * Returns csx's output: empty, or `focused <ref>` when the session already runs. */
export function openSession(terminal: string, tool: string, sessionId: string): Promise<string> {
  return run(["tabs", "open", "--terminal", terminal, "--", tool, sessionId], 30_000);
}
