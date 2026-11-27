# Secret shapes

This is fewer's published list of the secret shapes it hides. `fewer memory secrets` prints the same list (`secrets::SHAPES` in the code).

## What happens

- Before anything is saved, every value that matches a shape below is replaced by a marker: `[hidden: <name>]`, for example `password=[hidden: password]`.
- This happens for the output of `fewer run`, for agents' sessions (Claude Code, Codex and the others) and for OS log exports. Files you save on purpose with `fewer add` keep every byte, unless you ask with `--hide-secrets` (or set `hide_secrets_in_add = on`).
- Your terminal always shows the original output. Only the saved copy changes.
- `fewer read`, `table` and `changes` hide the same shapes in what they show, and `run` and Claude Code's hook hide them in a command's own line too. In a table (`table`, and `read` of a .csv or .tsv file), a column named as a secret (`password`, `api_token`, `API Key`, `Authorization`, ...) shows none of its values (true, false and null stay), and in a table with such a column a row with another number of fields shows none of its cells.
- Each saved run records how many values were hidden. `fewer memory --runs` shows it on the run's line ("2 secrets hidden").
- Hiding works line by line, on the bytes as they come. A secret split across two reads of a command's output is still hidden, because a part line waits for its line end before it is saved.
- Hiding the same text again changes nothing more: markers are never hidden twice.

Hiding is a safety net, not a promise. A secret in a shape that is not on this list is saved as it is. If one slips through, `fewer memory forget` removes the whole run.

## The shapes

| Name in the marker | What is matched | What is replaced |
|---|---|---|
| `AWS key` | `AKIA` or `ASIA`, then exactly 16 of A-Z and 0-9 | the whole key |
| `AWS secret` | a key named `aws_secret_access_key` (any case), then `=` or `:`, then exactly 40 of A-Z, a-z, 0-9, `/`, `+` and `=` | the value |
| `GitHub token` | `ghp_`, `gho_`, `ghu_`, `ghs_` or `ghr_`, then 36 or more letters and digits; `github_pat_`, then 22 or more letters, digits and `_` | the whole token |
| `GitLab token` | `glpat-`, then 20 or more letters, digits, `_` and `-` | the whole token |
| `Slack token` | `xoxa-`, `xoxb-`, `xoxp-`, `xoxo-`, `xoxs-` or `xoxr-`, then 10 or more letters, digits and `-` | the whole token |
| `Stripe key` | `sk_live_`, `sk_test_`, `rk_live_` or `rk_test_`, then 16 or more letters and digits | the whole key |
| `Anthropic key` | `sk-ant-`, then 20 or more letters, digits, `_` and `-` | the whole key |
| `OpenAI key` | `sk-proj-` or `sk-`, then 20 or more letters, digits, `_` and `-` | the whole key |
| `Google API key` | `AIza`, then exactly 35 letters, digits, `_` and `-` | the whole key |
| `JWT` | three base64url parts joined by `.`, the first two starting with `eyJ` (the third may be empty) | the whole token |
| `private key` | from `-----BEGIN ... PRIVATE KEY-----` (also `PRIVATE KEY BLOCK`, PGP) to the matching `-----END ...-----`, across lines | the key (see below) |
| `authorization` | `Authorization: Bearer <value>` and `Authorization: Basic <value>`, any case, also `Proxy-Authorization` and the JSON form `"Authorization": "Bearer <value>"` | the value |
| the key's name, such as `password` | a key whose name ends with `password`, `passwd`, `pwd`, `passphrase`, `secret`, `client_secret`, `secret_key`, `token`, `api_key`, `apikey`, `access_key` or `private_key`, then `=`, `:`, `=>` or `:=` | the value |
| `URL password` | the password in `scheme://user:password@host` | the password |
| the prefix, such as `acme_` | an extra shape from your config (below) | the whole token |

A token with a prefix counts only at the start of a word: the byte before it is not a letter or a digit. So `task-...` never matches `sk-`, and a key inside a longer word is left alone. A shape with an exact length (`AWS key`, `Google API key`) does not match when more letters or digits follow.

When a named value or a URL password is itself one of the tokens above, the marker names the token: `GITHUB_TOKEN=[hidden: GitHub token]`.

### Named values, in detail

- **The name.** The key's name is compared without case, without `_`, `-` and `.`, and without digits at its end. So `DB_PASSWORD`, `db.password`, `X-Api-Key`, `clientSecret`, `API_TOKEN` and `token2` all match, and the marker shows the listed name (`password`, `api_key`, `client_secret`, `token`). The name must end there: `passwordless`, `password_hash`, `max_tokens` and `token_count` do not match.
- **The separator.** `=`, `:`, `=>` or `:=`, with spaces and quotes allowed around it: `password=x`, `password = x`, `"password": "x"`, `'password' => 'x'`, `password := x`, `config["password"] = "x"`. Comparisons are not separators: `password == x`, `token != x`, and `a::b` are left alone.
- **Where the value ends.** A value in quotes runs to its closing quote: `"..."`, `'...'`, and `\"...\"` inside a JSON string (an agent's tool output). A value without quotes ends at a space, a quote, `&`, `;`, `,`, a backslash or a control byte: `?token=abc&page=2` hides only `abc`.
- **JSON.** When the key is in double quotes, a value that is not a string stays when it is `null`, `true`, `false`, an object or an array (Kubernetes' `"secret": {...}` is a structure, not a secret), and a number is replaced by the marker in quotes, `{"pwd":"[hidden: pwd]"}`, so a JSON line stays valid JSON.

### Private keys, in detail

- **A whole key on one line** (a key inside a JSON string, its line ends written as `\n`): from `-----BEGIN` to the end of the matching `-----END ...-----` becomes one marker. A key cut short, with no END on its line, is hidden to the end of its JSON string, or of the line.
- **A key over several lines** (the BEGIN marker is alone on its line, or alone in its JSON string as Docker writes one JSON object per line): the BEGIN marker becomes the marker, and the following lines are key lines up to the matching END line, which stays:
  - a key line made only of base64 (`A-Z a-z 0-9 + / =`) is removed with its line end;
  - any other key line (a log prefix in front, Docker's JSON around it, a `DEK-Info:` header) loses every run of 16 or more base64 bytes, and keeps the rest, so wrapped lines stay valid.
- A BEGIN line with no END line hides key lines for at most 1,000 lines.
- `-----BEGIN PUBLIC KEY-----`, `-----BEGIN CERTIFICATE-----` and other blocks that are not private keys are left alone.

### Extra shapes

Add your own shapes in the config file, one per line:

```
secret = PREFIX CLASS MIN
```

- `PREFIX`: 1 to 64 printable ASCII bytes, without spaces or brackets, matched at the start of a word, as written.
- `CLASS`: `alnum` (A-Z, a-z, 0-9), `base64` (A-Z, a-z, 0-9, `+`, `/`, `=`, `-`, `_`) or `hex` (0-9, a-f, A-F).
- `MIN`: how many CLASS bytes must follow the prefix, from 1 to 4096. All the CLASS bytes that follow are hidden with the prefix.

For example, `secret = acme_ alnum 20` turns `acme_` followed by 20 or more letters and digits into `[hidden: acme_]`.

## Left alone on purpose

- Values too short for their shape, prefixes not on the list (`pk_live_` publishable keys, `ghx_`), and tokens inside longer words.
- Names that only start like a listed name: `passwordless=true`, `password_hash=...`, `tokens=5`, `max_tokens: 100`.
- Empty values (`Enter password: `), and markers from an earlier pass.
- Public keys and certificates.

## Known limits

- **A value without quotes ends at a space.** `passphrase: correct horse battery` hides only `correct`. Values with spaces are hidden whole only when they are quoted.
- **A name must come right before the separator.** `--password hunter2` (a space, no `=`) and a password alone in a sentence are not caught.
- **Some names catch values that are not secrets.** `token` matches fields such as Android's `token=Token{a64f992 ...}` window handles, and `pwd` matches `PWD=/home/you` in environment dumps. They are hidden anyway: hiding a harmless value costs little, missing a secret costs a lot. On the 16 public Loghub samples and the JSON samples, 13 such values were hidden, all of them `token` fields; nothing else changed.
- **Very long lines.** A part line is held until its line end, up to 1 MiB; a longer line is scanned in 1 MiB pieces, and a secret across a cut may be missed.
- **Keys wrapped line by line.** In a key wrapped by a log prefix or by Docker's JSON, a short last key line (under 16 base64 bytes) stays. A lone BEGIN line with no key after it makes the next lines (up to 1,000) lose their long base64 runs.
- **Inside a key block,** the lines are not scanned for other shapes.
- **Personal data** (names, emails, addresses) is not a secret shape and is saved as it is. OS log exports in particular hold personal details; `fewer add --help` says so.
