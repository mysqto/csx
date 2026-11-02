import { execFile } from "node:child_process";
import { accessSync, constants } from "node:fs";
import { homedir } from "node:os";
import { getPreferenceValues } from "@raycast/api";
import { csxCandidates, describeFailure, extendedPath, isAutomatedUnsupported, isClosedUnsupported } from "./locate";
import { parseReport, Report } from "./rows";
import { parseTerminals, TerminalChoice } from "./terminals";

/** Thrown when no csx binary is found. */
export class CsxMissingError extends Error {
  constructor() {
    super("csx is not installed");
  }
}

/** A failed csx run, with the raw stderr kept for detection. */
class RunError extends Error {
  constructor(
    message: string,
    readonly stderr: string,
  ) {
    super(message);
  }
}

const CLOSED_UNSUPPORTED_WARNING = "Update csx to 0.3.0 or newer to resume closed sessions.";

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
      if (err) reject(new RunError(describeFailure(String(stderr), err.message, err.killed), String(stderr)));
      else resolve(String(stdout));
    });
  });
}

/** True when `e` says that csx is older than 0.3.0. */
function isBefore030(e: unknown): boolean {
  return e instanceof RunError && (isClosedUnsupported(e.stderr) || isAutomatedUnsupported(e.stderr));
}

/** List tabs; only those that run an agent, plus closed sessions, when
 * `agentsOnly`. `automated` also lists agents and sessions that a program or
 * another agent started. */
export async function listTabs(agentsOnly: boolean, automated = false): Promise<Report> {
  const extra = automated ? ["--automated"] : [];
  const args = agentsOnly ? ["--agents", "--closed", "--closed-limit", "200", ...extra] : extra;
  try {
    return parseReport(await run(["tabs", "--json", ...args], 60_000));
  } catch (e) {
    if (!isBefore030(e)) throw e;
    // csx before 0.3.0 lists open tabs only, and every agent.
    const report = parseReport(await run(agentsOnly ? ["tabs", "--json", "--agents"] : ["tabs", "--json"], 60_000));
    if (!agentsOnly) return report;
    return { ...report, warnings: [...report.warnings, CLOSED_UNSUPPORTED_WARNING] };
  }
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
