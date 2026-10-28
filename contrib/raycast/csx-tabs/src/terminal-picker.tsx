import {
  Action,
  ActionPanel,
  closeMainWindow,
  Icon,
  List,
  LocalStorage,
  PopToRootType,
  showHUD,
  useNavigation,
} from "@raycast/api";
import { usePromise } from "@raycast/utils";
import { listTerminals, openSession } from "./lib/csx";
import { ClosedSession } from "./lib/rows";
import { resumeFailure, TerminalChoice } from "./lib/terminals";

/** LocalStorage key that holds the id of the default terminal. */
export const TERMINAL_KEY = "terminal";

/** Save the default terminal. `onSaved` does the write when given, such as the
 * list's useLocalStorage setter, so there is one write. */
async function saveTerminal(id: string, onSaved?: (id: string) => void | Promise<void> | Promise<void>) {
  if (onSaved) await onSaved(id);
  else await LocalStorage.setItem(TERMINAL_KEY, id);
}

/** Resume `session` in `terminal`. The window closes first, so a failure shows
 * as a HUD. The terminal is saved only after the open worked. */
export async function resumeIn(
  session: ClosedSession,
  terminal: TerminalChoice,
  save: boolean,
  onSaved?: (id: string) => void | Promise<void>,
) {
  await closeMainWindow({ clearRootSearch: true, popToRootType: PopToRootType.Immediate });
  try {
    await openSession(terminal.id, session.tool, session.session_id);
  } catch (e) {
    await showHUD(resumeFailure((e as Error).message));
    return;
  }
  if (save) {
    await saveTerminal(terminal.id, onSaved);
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
  onSaved?: (id: string) => void | Promise<void>;
}) {
  const { pop } = useNavigation();
  const { data, error, isLoading, revalidate } = usePromise(listTerminals);

  async function choose(t: TerminalChoice) {
    if (session) {
      await resumeIn(session, t, save, onSaved);
      return;
    }
    await saveTerminal(t.id, onSaved);
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
