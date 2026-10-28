import { Action, ActionPanel, closeMainWindow, Icon, List, LocalStorage, showHUD, useNavigation } from "@raycast/api";
import { usePromise } from "@raycast/utils";
import { listTerminals, openSession } from "./lib/csx";
import { ClosedSession } from "./lib/rows";
import { TerminalChoice } from "./lib/terminals";

/** LocalStorage key that holds the id of the default terminal. */
export const TERMINAL_KEY = "terminal";

/** Resume `session` in `terminal`. The window closes first, so a failure shows
 * as a HUD. The terminal is saved only after the open worked. */
export async function resumeIn(
  session: ClosedSession,
  terminal: TerminalChoice,
  save: boolean,
  onSaved?: (id: string) => void,
) {
  await closeMainWindow({ clearRootSearch: true });
  try {
    await openSession(terminal.id, session.tool, session.session_id);
  } catch (e) {
    await showHUD(`${(e as Error).message}. Use Resume in Another Terminal to pick another.`);
    return;
  }
  if (save) {
    await LocalStorage.setItem(TERMINAL_KEY, terminal.id);
    onSaved?.(terminal.id);
  }
}

/** Pick a terminal. With a `session`, resume it there. Without one, only save
 * the choice as the default. `save` says whether a resume also saves it. */
export function TerminalPicker({
  session,
  save,
  savedId,
  onSaved,
}: {
  session?: ClosedSession;
  save: boolean;
  savedId?: string;
  onSaved?: (id: string) => void;
}) {
  const { pop } = useNavigation();
  const { data, error, isLoading, revalidate } = usePromise(listTerminals);

  async function choose(t: TerminalChoice) {
    if (session) {
      await resumeIn(session, t, save, onSaved);
      return;
    }
    await LocalStorage.setItem(TERMINAL_KEY, t.id);
    onSaved?.(t.id);
    await showHUD(`Default terminal: ${t.name}`);
    pop();
  }

  return (
    <List isLoading={isLoading} searchBarPlaceholder="Choose a terminal">
      {error ? (
        <List.EmptyView
          icon={Icon.ExclamationMark}
          title="csx failed"
          description={error.message}
          actions={
            <ActionPanel>
              <Action title="Try Again" icon={Icon.ArrowClockwise} onAction={revalidate} />
            </ActionPanel>
          }
        />
      ) : (
        <List.EmptyView icon={Icon.Terminal} title="No supported terminal found" />
      )}
      {(data ?? []).map((t) => (
        <List.Item
          key={t.id}
          icon={Icon.Terminal}
          title={t.name}
          accessories={t.id === savedId ? [{ text: "Default" }] : []}
          actions={
            <ActionPanel>
              <Action title={session ? `Resume in ${t.name}` : `Use ${t.name}`} onAction={() => choose(t)} />
            </ActionPanel>
          }
        />
      ))}
    </List>
  );
}
