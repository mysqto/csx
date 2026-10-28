import { homedir } from "node:os";
import {
  Action,
  ActionPanel,
  closeMainWindow,
  Color,
  Icon,
  Keyboard,
  List,
  showHUD,
  useNavigation,
} from "@raycast/api";
import { TerminalPicker, resumeIn, TERMINAL_KEY } from "./terminal-picker";
import { showFailureToast, useLocalStorage, usePromise } from "@raycast/utils";
import { savedChoice, resumeTitle, TerminalChoice } from "./lib/terminals";
import { CsxMissingError, focusTab, listTabs, listTerminals } from "./lib/csx";
import {
  ClosedSession,
  closedKeywords,
  closedPrompt,
  closedTitle,
  Row,
  rowCwd,
  rowKey,
  rowKeywords,
  rowPrompt,
  rowTitle,
  shortenHome,
  terminalName,
  toolName,
} from "./lib/rows";

const INSTALL_COMMAND =
  "brew tap mysqto/csx https://github.com/mysqto/csx && brew trust mysqto/csx && brew install --cask csx";
const HOME = homedir();

async function focus(ref: string) {
  await closeMainWindow({ clearRootSearch: true });
  try {
    await focusTab(ref);
  } catch (e) {
    await showHUD(`Could not focus the tab: ${(e as Error).message}`);
  }
}

function statusAccessory(r: Row): List.Item.Accessory[] {
  switch (r.agent?.status) {
    case "busy":
      return [{ tag: { value: "busy", color: Color.Orange }, tooltip: "The agent is working" }];
    case "idle":
      return [{ tag: { value: "idle", color: Color.Green }, tooltip: "The agent waits for input" }];
    default:
      return [];
  }
}

function accessories(r: Row): List.Item.Accessory[] {
  const out = statusAccessory(r);
  if (r.agent?.updated_at) out.push({ date: new Date(r.agent.updated_at), tooltip: "Last activity" });
  if (r.terminal) out.push({ text: terminalName(r.terminal) });
  return out;
}

function subtitle(r: Row): string {
  const parts = [r.agent ? toolName(r.agent.tool) : "", shortenHome(rowCwd(r) ?? "", HOME)];
  const prompt = rowPrompt(r);
  if (prompt) parts.push(`“${prompt}”`);
  return parts.filter(Boolean).join(" · ");
}

function RowActions({ row, reload }: { row: Row; reload: () => void }) {
  const cwd = rowCwd(row);
  const agent = row.agent;
  return (
    <ActionPanel>
      {row.ref && <Action title="Focus Tab" icon={Icon.Window} onAction={() => focus(row.ref as string)} />}
      {agent?.resume_command && (
        <Action.CopyToClipboard
          title="Copy Resume Command"
          content={agent.resume_command}
          shortcut={Keyboard.Shortcut.Common.Copy}
        />
      )}
      {agent?.session_id && (
        <Action.CopyToClipboard
          title="Copy Session ID"
          content={agent.session_id}
          shortcut={{ modifiers: ["cmd", "shift"], key: "i" }}
        />
      )}
      {cwd && (
        <Action.CopyToClipboard
          title="Copy Working Directory"
          content={cwd}
          shortcut={Keyboard.Shortcut.Common.CopyPath}
        />
      )}
      {cwd && <Action.ShowInFinder path={cwd} shortcut={{ modifiers: ["cmd", "shift"], key: "f" }} />}
      <Action
        title="Refresh"
        icon={Icon.ArrowClockwise}
        shortcut={Keyboard.Shortcut.Common.Refresh}
        onAction={reload}
      />
    </ActionPanel>
  );
}

function ClosedActions({
  session,
  saved,
  savedId,
  onSaved,
}: {
  session: ClosedSession;
  saved: TerminalChoice | undefined;
  savedId: string | undefined;
  onSaved: (id: string) => void;
}) {
  const { push } = useNavigation();
  return (
    <ActionPanel>
      <Action
        title={resumeTitle(saved)}
        icon={Icon.Play}
        onAction={() =>
          saved
            ? resumeIn(session, saved, false)
            : push(<TerminalPicker session={session} save savedId={savedId} onSaved={onSaved} />)
        }
      />
      <Action
        title="Resume in Another Terminal…"
        icon={Icon.Terminal}
        shortcut={{ modifiers: ["cmd"], key: "t" }}
        onAction={() => push(<TerminalPicker session={session} save={false} savedId={savedId} />)}
      />
      <Action
        title="Change Default Terminal…"
        icon={Icon.Gear}
        shortcut={{ modifiers: ["cmd", "shift"], key: "t" }}
        onAction={() => push(<TerminalPicker save={false} savedId={savedId} onSaved={onSaved} />)}
      />
      {session.resume_command && (
        <Action.CopyToClipboard
          title="Copy Resume Command"
          content={session.resume_command}
          shortcut={Keyboard.Shortcut.Common.Copy}
        />
      )}
      <Action.CopyToClipboard
        title="Copy Session ID"
        content={session.session_id}
        shortcut={{ modifiers: ["cmd", "shift"], key: "i" }}
      />
      {session.cwd && (
        <Action.CopyToClipboard
          title="Copy Working Directory"
          content={session.cwd}
          shortcut={Keyboard.Shortcut.Common.CopyPath}
        />
      )}
    </ActionPanel>
  );
}

function closedSubtitle(c: ClosedSession): string {
  const parts = [toolName(c.tool), shortenHome(c.cwd, HOME)];
  const prompt = closedPrompt(c);
  if (prompt) parts.push(`“${prompt}”`);
  return parts.filter(Boolean).join(" · ");
}

function ErrorView({ error, reload }: { error: Error; reload: () => void }) {
  if (error instanceof CsxMissingError) {
    return (
      <List.EmptyView
        icon={Icon.Download}
        title="csx is not installed"
        description="This extension needs the csx command. Install it with Homebrew, or set its path in the extension settings."
        actions={
          <ActionPanel>
            <Action.CopyToClipboard title="Copy Install Command" content={INSTALL_COMMAND} />
            <Action.OpenInBrowser title="Open GitHub Page" url="https://github.com/mysqto/csx" />
            <Action title="Try Again" icon={Icon.ArrowClockwise} onAction={reload} />
          </ActionPanel>
        }
      />
    );
  }
  return (
    <List.EmptyView
      icon={Icon.ExclamationMark}
      title="csx failed"
      description={error.message}
      actions={
        <ActionPanel>
          <Action title="Try Again" icon={Icon.ArrowClockwise} onAction={reload} />
          <Action.CopyToClipboard title="Copy Error" content={error.message} />
        </ActionPanel>
      }
    />
  );
}

export function TabsList({ agentsOnly }: { agentsOnly: boolean }) {
  const { data, error, isLoading, revalidate } = usePromise(listTabs, [agentsOnly], {
    // The empty view explains a missing csx, so only other errors get a toast.
    onError: (e) => {
      if (!(e instanceof CsxMissingError)) showFailureToast(e, { title: "csx failed" });
    },
  });
  const { value: savedId, setValue: saveId } = useLocalStorage<string>(TERMINAL_KEY);
  const terminals = usePromise(listTerminals, [], { execute: agentsOnly, onError: () => {} });
  const saved = savedChoice(savedId, terminals.data ?? []);
  const rows = data?.rows ?? [];
  const closed = data?.closed ?? [];
  const warnings = data?.warnings ?? [];
  const noun = agentsOnly ? "AI sessions" : "terminal tabs";

  return (
    <List isLoading={isLoading} searchBarPlaceholder={`Search ${noun}`}>
      {error ? (
        <ErrorView error={error} reload={revalidate} />
      ) : (
        <List.EmptyView
          icon={Icon.Terminal}
          title={`No ${noun}`}
          description={agentsOnly ? "No AI coding agent runs in a terminal tab right now." : undefined}
        />
      )}
      {warnings.length > 0 && (
        <List.Section title="Warnings">
          {warnings.map((w, i) => (
            <List.Item
              key={`warning-${i}`}
              icon={{ source: Icon.Warning, tintColor: Color.Yellow }}
              title={w}
              actions={
                <ActionPanel>
                  <Action.CopyToClipboard title="Copy Warning" content={w} />
                </ActionPanel>
              }
            />
          ))}
        </List.Section>
      )}
      <List.Section title={warnings.length > 0 || closed.length > 0 ? "Tabs" : undefined}>
        {rows.map((r, i) => (
          <List.Item
            key={rowKey(r, i)}
            icon={r.agent ? Icon.Stars : Icon.Terminal}
            title={rowTitle(r)}
            subtitle={subtitle(r)}
            keywords={rowKeywords(r)}
            accessories={accessories(r)}
            actions={<RowActions row={r} reload={revalidate} />}
          />
        ))}
      </List.Section>
      {closed.length > 0 && (
        <List.Section title="Closed Sessions">
          {closed.map((c) => (
            <List.Item
              key={`${c.tool}:${c.session_id}`}
              icon={Icon.Clock}
              title={closedTitle(c)}
              subtitle={closedSubtitle(c)}
              keywords={closedKeywords(c)}
              accessories={c.updated_at ? [{ date: new Date(c.updated_at), tooltip: "Last activity" }] : []}
              actions={<ClosedActions session={c} saved={saved} savedId={savedId} onSaved={saveId} />}
            />
          ))}
        </List.Section>
      )}
    </List>
  );
}
