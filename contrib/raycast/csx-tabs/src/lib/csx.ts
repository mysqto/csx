import { execFile } from "node:child_process";
import { accessSync, constants } from "node:fs";
import { homedir } from "node:os";
import { getPreferenceValues } from "@raycast/api";
import { csxCandidates, describeFailure, extendedPath } from "./locate";
import { parseReport, Report } from "./rows";

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

/** List tabs; only those that run an agent when `agentsOnly`. */
export async function listTabs(agentsOnly: boolean): Promise<Report> {
  const args = ["tabs", "--json", ...(agentsOnly ? ["--agents"] : [])];
  return parseReport(await run(args, 60_000));
}

/** Bring the tab named by `ref` to the front. */
export async function focusTab(ref: string): Promise<void> {
  await run(["tabs", "focus", "--", ref], 10_000);
}
