# CSX Tabs

Jump to the terminal tab where an AI coding agent runs, or to any terminal tab.
The extension supports Ghostty, iTerm2, Terminal, kitty, WezTerm and tmux.

## Commands

- Search AI Sessions lists the tabs that run Claude Code, Codex, Gemini CLI, Cursor Agent or another AI coding agent. Each row shows the session title, folder, last prompt and whether the agent is busy or idle. Sessions whose tab is closed show below, in the Closed Sessions section.
- Search Terminal Tabs lists every terminal tab.

Press Return to bring the tab to the front. The Action Panel can also copy the command that resumes the session, the session ID or the folder.

## Resume closed sessions

Closed sessions appear only in Search AI Sessions. Press Return on one to resume it in a new tab.

The first time, the extension asks which terminal to use. It lists only the terminals that are installed. After the session opens, the extension remembers that terminal. Later, Return opens the session there. The action is named Resume in, then the terminal name.

- Resume in Another Terminal opens the session in a terminal you pick. It does not change the saved one.
- Change Default Terminal changes the saved terminal.
- If the saved terminal is no longer installed, the extension asks again.
- If the session already runs in a tab, csx brings that tab to the front.
- If the open fails, a message tells you why. Use Resume in Another Terminal to try a different terminal.

## Automated sessions

Both commands hide sessions that a program or another AI agent started, such as print mode, SDK runs, review jobs and subagents. To show them, turn on Show Automated Sessions in the extension preferences. Such a session then has an `automated` or `subagent` tag. This needs csx 0.3.0 or newer.

## Setup

The extension uses the [csx](https://github.com/mysqto/csx) command line tool. Install csx 0.2.0 or newer with Homebrew. Resuming closed sessions needs csx 0.3.0 or newer. With csx 0.2.0 the extension still lists and focuses tabs, and shows a warning that closed sessions need an update. Recent Homebrew versions load a cask from a third-party tap only after `brew trust`:

```sh
brew tap mysqto/csx https://github.com/mysqto/csx
brew trust mysqto/csx
brew install --cask csx
```

The extension looks for csx in `/opt/homebrew/bin`, `/usr/local/bin`, `/opt/local/bin`, `~/.cargo/bin` and `~/.local/bin`. If you installed csx somewhere else, set its full path in the extension preferences.

## Permissions

csx reads the tab lists through Apple Events. The first time you use a command, macOS asks whether Raycast can control each terminal. Allow it. If you clicked Don't Allow, turn the terminal on again in System Settings > Privacy & Security > Automation > Raycast.

kitty needs remote control. Add `allow_remote_control socket-only` and `listen_on unix:${TMPDIR}/kitty` to `kitty.conf`, then restart kitty.
