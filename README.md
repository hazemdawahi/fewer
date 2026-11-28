<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="Fewer" width="340">
  </picture>
</p>

<h3 align="center">Fewer tokens. Nothing forgotten.</h3>

<p align="center">
  <b>Get your Claude usage back: 86% fewer tokens, the same answers.</b>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-6366F1?style=flat-square" alt="License: Apache 2.0"></a>
  <img src="https://img.shields.io/badge/version-1.2.1-818CF8?style=flat-square" alt="Version 1.2.1">
</p>

<p align="center">
  <a href="#what-is-fewer">What is it</a> ·
  <a href="#benchmark">Benchmark</a> ·
  <a href="#see-it">See it</a> ·
  <a href="#install">Install</a> ·
  <a href="#use">Use</a>
</p>

---

## What is Fewer?

Fewer makes Claude Code use **86% fewer tokens**, with the same answers. Your Claude usage lasts
longer. It's free.

Claude Code re-reads the whole chat at every step. Fewer gives it short versions and reads ahead
what your message names, so Claude often answers in one step.

## Benchmark

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/benchmark-dark.svg">
    <img src="assets/benchmark-light.svg" alt="Tokens used on the same tasks. Code tasks: Fewer 166,118 tokens (85% fewer than Claude Code alone, 1,122,500); web tasks: Fewer 42,473 (90% fewer than 407,892)." width="880">
  </picture>
</p>

9 tasks, run 12 times each with and without Fewer, on GitHub's clean Linux and Windows machines
(Claude Code 2.1.332, Opus 5.5). Every answer was right both ways. Each round: 87–93% fewer
tokens, and 84% less in dollars.

**Check it:** [round 1](https://github.com/hazemdawahi/fewer/actions/runs/45949722161) · [round 2](https://github.com/hazemdawahi/fewer/actions/runs/45953716593) · [raw results](bench/results) · [rerun it yourself](bench/README.md)

### Each task, before and after

| Task | Claude Code alone | With Fewer | Fewer tokens |
|---|--:|--:|--:|
| Find why the tests fail | 47,945 | 19,618 | **59%** |
| Count the errors in a log | 81,415 | 20,163 | **75%** |
| Add up the totals in a CSV file | 91,490 | 19,615 | **79%** |
| Explain the last commit | 65,666 | 19,418 | **70%** |
| Find two status codes in a web page | 327,105 | 19,810 | **94%** |
| A web search: when Rust 1.0 came out | 80,786 | 22,663 | **72%** |
| Fix a bug, then run its test | 102,028 | 40,423 | **60%** |
| Read 13 files and answer 5 questions | 91,691 | 25,398 | **72%** |
| Review 20 files one by one (a long chat) | 642,263 | 21,481 | **97%** |
| **All 9 tasks** | **1,530,392** | **208,591** | **86%** |

## See it

`/fewer-tokens` in my own Claude Code chat:

<p align="center">
  <img src="assets/fewer-tokens.png" alt="/fewer-tokens in my own Claude Code chat" width="880">
</p>

## Install

Works on macOS, Linux and Windows. All you need is [Claude Code](https://claude.com/claude-code).

```sh
curl -fsSL https://github.com/hazemdawahi/fewer/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://github.com/hazemdawahi/fewer/releases/latest/download/install.ps1 | iex
```

It shows every change it will make, then asks you once. Then open a new Claude Code chat.

Or paste this into Claude Code:

```text
Install Fewer for me: curl -fsSL https://github.com/hazemdawahi/fewer/releases/latest/download/install.sh | sh -s -- --yes
```

**Update:** `fewer update` &nbsp;·&nbsp; **Remove:** `fewer uninstall`

## Use

Use Claude Code as usual, in any project: it uses Fewer by itself. These are for you:

| Command | What it does |
|---|---|
| `fewer tokens` | The tokens Fewer saved |
| `fewer memory` | What Fewer saved, its size, and delete what you want |
| `fewer off` / `fewer on` | Turn Fewer off or on |

In a Claude Code chat: `/fewer-tokens`, `/fewer-memory`, `/fewer-off`, `/fewer-on`.

## License

Apache 2.0. See [LICENSE](LICENSE).

---

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="Fewer" width="200">
  </picture>
</p>

<p align="center"><b>Saved you tokens?</b> A star helps other Claude Code users find Fewer.</p>
