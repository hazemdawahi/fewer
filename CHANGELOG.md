# Changelog

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
