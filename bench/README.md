# Rerun Fewer's benchmark

The numbers on the main page come from this benchmark. You can run it yourself.

It makes a small made-up project and gives Claude Code the same 9 tasks twice: once alone, once
with Fewer. It counts every token each run uses and checks each answer.

## Run it

You need Claude Code, Python 3, git and Fewer.

```sh
python3 bench/bench.py run ~/fewer-bench
```

For a clean run without your own Claude Code settings, first make a login token with
`claude setup-token` and set it as `CLAUDE_CODE_OAUTH_TOKEN`.

It uses your Claude usage: about 6 million tokens alone, under 1 million with Fewer. To try it
small first:

```sh
python3 bench/bench.py run ~/fewer-bench --tasks T1,T7 --runs 1
```

If it hits your usage limit, it stops. Run the same command later and it picks up where it left off.

Claude answers a little differently every time, so expect numbers close to ours, not the same.

## The tasks

| | Task |
|---|---|
| T1 | Find why the tests fail |
| T3 | Count the errors in a log |
| T5 | Add up the totals in a CSV file |
| T7 | Explain the last commit |
| T21 | Find two status codes in a web page |
| T22 | A web search: when Rust 1.0 came out |
| T6 | Fix a bug, then run its test |
| T8 | Read 13 files and answer 5 questions |
| T9 | Review 20 files one by one |

The exact wording of each task is at the top of `bench.py`.

## Our runs

[results/](results) holds every run behind the main page's numbers. To print the table from them:

```sh
python3 bench/bench.py report bench/results/*
```
