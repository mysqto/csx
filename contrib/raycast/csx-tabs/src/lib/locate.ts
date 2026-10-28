// Where to look for csx, and the PATH it runs with. Pure, like rows.ts.

/** Folders csx installs into, in search order. */
export function binDirs(home: string): string[] {
  return ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin", `${home}/.cargo/bin`, `${home}/.local/bin`];
}

/** Paths to try for csx: the preference first, when set. */
export function csxCandidates(home: string, preference: string | undefined): string[] {
  const pref = preference?.trim();
  const found = binDirs(home).map((d) => `${d}/csx`);
  return pref ? [pref, ...found] : found;
}

/** Raycast starts commands with a short PATH. csx runs osascript, ps, lsof,
 * kitten, wezterm and tmux, so put the install folders in front. */
export function extendedPath(home: string, path: string | undefined): string {
  const parts = [...binDirs(home), ...(path ? path.split(":") : []), "/usr/bin", "/bin", "/usr/sbin", "/sbin"];
  return [...new Set(parts.filter(Boolean))].join(":");
}

/** Error text from a failed csx run, mapped to what the user should do. */
export function describeFailure(stderr: string, message: string): string {
  if (/unrecognized subcommand 'tabs'|unexpected argument '--agents'/.test(stderr)) {
    return "This csx is too old for tabs. Update csx to 0.2.0 or newer.";
  }
  const line = stderr
    .split("\n")
    .map((l) => l.trim())
    .find(Boolean);
  return line ? line.replace(/^(csx|error): /, "") : message;
}
