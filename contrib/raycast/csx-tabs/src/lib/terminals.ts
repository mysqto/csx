// Pure helpers for the terminal choice. No Raycast or Node APIs here.

export interface TerminalChoice {
  id: string;
  name: string;
}

/** Parse `csx tabs terminals --json`. Throws when it is not a list. */
export function parseTerminals(text: string): TerminalChoice[] {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new Error("csx printed something that is not JSON");
  }
  if (!Array.isArray(v)) throw new Error("csx printed JSON that is not a list");
  const out: TerminalChoice[] = [];
  for (const e of v) {
    if (typeof e === "object" && e !== null) {
      const { id, name } = e as Record<string, unknown>;
      if (typeof id === "string" && typeof name === "string") out.push({ id, name });
    }
  }
  return out;
}

/** The title of the primary action for a closed session. */
export function resumeTitle(saved: TerminalChoice | undefined): string {
  return saved ? `Resume in ${saved.name}` : "Resume Session…";
}

/** The saved terminal, when it is still installed. */
export function savedChoice(savedId: string | undefined, installed: TerminalChoice[]): TerminalChoice | undefined {
  return savedId ? installed.find((t) => t.id === savedId) : undefined;
}

/** The HUD text for a failed resume: csx's message, then what to try. */
export function resumeFailure(message: string): string {
  return `${message.trim().replace(/\.$/, "")}. Use Resume in Another Terminal to pick another.`;
}
