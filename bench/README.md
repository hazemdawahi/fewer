# Rerun Fewer's benchmark

The numbers on the main page come from this benchmark. Run it on your own computer to see your
own numbers.

**What it does:** it makes a small made-up project (code with two failing tests, a 20,000-line
log, a CSV file, a git history), then gives your own Claude Code the same 9 tasks twice: once as
it is, once with Fewer. For each run it counts the tokens Claude Code used (input and cache, every
model, Fewer's helpers too) and checks the answer against the facts it planted.

Both setups load the same things of yours: your login, your CLAUDE.md, your add-ons and skills.
The only difference is Fewer. So your numbers depend on your setup, and they will not match ours
exactly.

## Run it

You need Claude Code (logged in), Python 3 and git, and Fewer installed (`fewer` on your PATH).

```sh
python3 bench/bench.py run ~/fewer-bench
```

It prints each run as it goes, then a table like the one on the main page. To see the table again:
`python3 bench/bench.py report ~/fewer-bench`.

**It uses your Claude usage:** the whole benchmark is 54 runs (9 tasks, 3 runs each, 2 setups),
about 6 million tokens with Claude Code alone and under 1 million with Fewer. To try it small
first:

```sh
python3 bench/bench.py run ~/fewer-bench --tasks T1,T7 --runs 1
```

If your plan's usage limit is reached, it stops; run the same command later and it goes on where
it stopped.

## The tasks

| | Task | What Claude is asked |
|---|---|---|
| T1 | Find why the tests fail | Run the tests; which fail and what each failure says |
| T3 | Count the errors in a log | 5xx responses per endpoint in a 20,000-line log |
| T5 | Add up the totals in a CSV file | Each region's total to the cent, and the top product |
| T7 | Explain the last commit | Each function the last commit changed, and how |
| T21 | Find two status codes in a web page | Read RFC 9110 on the web for two HTTP status codes |
| T22 | A web search | The year Rust 1.0 came out |
| T6 | Fix a bug, then run its test | Fix the code (not the test) and show the test passes |
| T8 | Read 13 files and answer 5 questions | Facts spread over 13 modules |
| T9 | Review 20 files one by one | A long chat: 20 long modules read whole, five bugs to find |

The exact wording of each task is at the top of `bench.py`.
