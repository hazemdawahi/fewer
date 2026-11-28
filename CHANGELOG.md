# Changelog

## 1.2.1 (2026-11-27)

- Fewer no longer sets `autoCompactWindow`, so long chats compact where normal Claude Code does.
  1.0.0 to 1.2.0 set it to 100000, and Claude Code compacted long chats far too early (near
  67,000 tokens, even on the 1M-token model).
- An update takes Fewer's old line back out, and so do `fewer on`, `fewer off` and
  `fewer uninstall`; each says so in one line when it does. A value you set yourself is kept.
- Already on Fewer: run `fewer update`, then restart Claude Code once: quit VSCode or the
  terminal fully and open it again (`/clear` is not enough).

## 1.2.0 (2026-11-27)

- Claude Code runs as normal: Fewer no longer turns any of its features off. Workflows,
  subagents, skills, plan mode, Claude's questions, the to-do list, Artifacts and the AI-written
  chat titles are as you have them, on or off. Fewer still uses its own cheaper ways first (the
  web, pictures, skills) and makes the agents it starts in workflows and subagents lean.
- An update from an earlier version gives back what that version turned off (only where it is
  still at Fewer's own value).
- Read-ahead: before Claude's first reply, Fewer reads what your message names and hands Claude
  its short, exact view, so Claude can often answer in that first reply. It reads files and
  folders, a CSV or a log, the last commit, a web page or a web search, and a failing Python
  test with the code it imports. It also runs a test, build or lint command your message asks
  for, but only when your own permission rules already let it run without asking, and never in
  plan mode. Your Read rules and the project's bounds hold. To turn it off, put `ahead = off`
  in Fewer's config.
- `fewer read` of many files shows each file's own lines: its imports, constants and small
  functions. A block that repeats with only its numbers changed is shown once and named where it
  is. Before, Claude saw only the function names and had to search again.
- `fewer web` works in `claude -p` runs too; before, it turned them away as Agent SDK programs.
- `fewer update` on Windows works when run from PowerShell 7; before, the PowerShell it started
  could not check the download, and the update did not happen.
- Fewer's rules are shorter, and still say the same.
- `fewer on` and `fewer off` say one line (`--dry-run` lists what they would change).

## 1.1.0 (2026-11-26)

- Works on Windows too. In PowerShell:
  `irm https://github.com/hazemdawahi/fewer/releases/latest/download/install.ps1 | iex`. In Git
  Bash, the `install.sh` line hands over to it. `fewer update` and `fewer uninstall` work there
  the Windows way (your user PATH, a `fewer.exe` that is running).
- On Windows, Claude Code's hooks, Fewer's MCP server, `fewer run` (npm's `.cmd` tools too) and
  saved chats work as on macOS and Linux. Not on Windows yet: `fewer-pack --rm` on a folder (it
  keeps the folder).
- Fewer's helpers keep a `claude setup-token` login (CLAUDE_CODE_OAUTH_TOKEN), as on a CI machine.
- The benchmark (`bench/`) is the same for everyone: Claude Opus 5.5, the Claude Code version
  shown, and with a login token none of your own Claude Code loaded.

## 1.0.0 (2026-11-26)

The first release.

- Claude Code uses 87% fewer tokens, with the same answers as without fewer. Measured with a
  fresh Claude Code 2.1.332 and Claude Opus 5.5 on 9 tasks, 3 runs each: every answer was right,
  with fewer and without it.
- Claude reads short versions of long output (a test run's failures, a build's errors, a table's
  totals); the full text stays on your computer, where Claude can open it.
- Everything else works as usual: rewind, your permission rules and your add-ons. fewer's reading
  commands run without asking, as Claude Code's Read tool does: they keep your Read rules and
  stay inside the project.
- `fewer install`, `fewer update` and `fewer uninstall`; `fewer on` and `fewer off`; `fewer
  tokens` (the tokens saved) and `fewer memory` (what is saved, its size, and a menu to delete
  what you want). In a chat: `/fewer-tokens`, `/fewer-memory`, `/fewer-on`, `/fewer-off`.
- Works on macOS and Linux.
