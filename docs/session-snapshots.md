# Session snapshots

Status: planning. Nothing is implemented yet.

Save a Lynn session to a file and restore it later, including commands, comments, pages, and agent requests and replies.

## Commands

Borrowed from the SQLite shell's dot commands:

| Command | Does |
| --- | --- |
| `.save FILE` | Write the current session to a tar snapshot. |
| `.restore FILE` | Replace the current session with the snapshot, like `reset` followed by a replay. Lynn's working directory stays the same. |
| `.dump` | Print the session as a plain-text transcript. |
| `.open FILE` | Later: save as you go by appending each new event to the tar. |

## Why tar

- Pure Rust (the `tar` crate), with no C dependency like SQLite would add.
- Anyone can inspect it with `tar -tf` and `tar -xf`, no Lynn needed.
- Entries can be appended (`tar -rf`), and when a name repeats the last copy wins. That makes `.open` cheap later.

## Layout

```text
session.tar
├── session.json
└── events/
    ├── 000001-command.json
    ├── 000001-command.sh
    ├── 000002-comment.json
    ├── 000002-comment.txt
    ├── 000003-request-3-claude+opus.json
    ├── 000003-request-3-claude+opus.txt
    ├── 000004-response-3-claude+claude-opus-5-5.json
    ├── 000004-response-3-claude+claude-opus-5-5.md
    ├── 000005-page-4.json
    ├── 000005-page-4.html
    ├── 000009-request-8-pi+kapitsa+qwen3.5-9b.json
    └── 000009-request-8-pi+kapitsa+qwen3.5-9b.txt
```

### `session.json`

Session-wide state:

- the format version
- `cwd`, for information only (restore does not `cd`)
- the current Pi model
- todos
- watches
- the next `#n`

### One event, two files

Each event is a pair of files with the same stem:

- **Body:** exactly the raw content, with no preamble. The command line, the comment text, the request text, the reply as Markdown, or the page in its own format (`.html` and so on).
- **Metadata (`.json`):** whatever the filename doesn't already say, such as exit status, access, effort, output mode, elapsed time and model info.

The body is written first and the `.json` second, so the `.json` serves as a commit marker. If a body has no `.json`, the event was never finished and restore ignores it.

## Filename grammar

The filename is canonical. It is the only record of an event's sequence number, kind, id, harness and model, and the `.json` does not repeat them.

```text
<seq>-<kind>[-<id>][-<harness>+<model>].<ext>
```

| Field | Values | Notes |
| --- | --- | --- |
| `seq` | six digits, `000001` up | Keeps `ls` sorted and preserves order when a snapshot is extracted and repacked. |
| `kind` | `command`, `comment`, `request`, `response`, `page`, `head` | The kind sets which fields follow. |
| `id` | the session `#n` | Present for `request`, `response`, `page` and `head`. |
| `harness` | `claude`, `codex`, `pi`, `agy`, `fx`, `fm`, `jsc` | Present for `request` and `response`. |
| `model` | the model path, with `/` written as `+` | Present for `request` and `response`. |
| `ext` | `sh`, `txt`, `md`, `html`, … for the body, or `json` | |

Lynn writes the seq, kind, id and harness itself, and guarantees they never contain `-` or `+`.

### Which model goes in the name

- **Request:** the model Lynn passed to the harness (`kapitsa/qwen3.5-9b`). For agents with no explicit backend model, it's Lynn's agent name (`opus`).
- **Response:** the model the harness reported (`claude-opus-5-5`), or the requested model when nothing was reported.

### Parsing

1. Split off the extension at the last `.`.
2. Split the rest on `-` for as many fields as the kind needs. Whatever remains is the agent part.
3. In the agent part, the text before the first `+` is the harness. The rest, with each `+` turned back into `/`, is the model.

Hyphens and dots in model names are fine because the model comes last. Pi IDs with extra slashes work too: `pi+openrouter+anthropic+claude-sonnet` is harness `pi` and model `openrouter/anthropic/claude-sonnet`.

Restore rejects names that don't parse, and pairs whose stems don't match, instead of guessing.

### Cross-platform characters

There is no percent-encoding. Filenames use only `[A-Za-z0-9._+-]`, which is safe on Windows, macOS, Linux and in shells without quoting.

A model ID can only be stored exactly if it consists of `[A-Za-z0-9._-]` and `/`. Otherwise:

- Each character outside that set becomes `_` in the name. The usual case is `:` (Ollama's `qwen3:8b`, OpenRouter's `:free`), which Windows forbids. A literal `+` is replaced the same way.
- The `.json` then records the exact value, e.g. `"model": "qwen3:8b"`. Restore uses that when present and the name otherwise.

Two models that differ only in case would collide on case-insensitive filesystems. This is rare enough to ignore for now.

## Restore behaviour

- The session is replaced, not merged.
- Lynn's working directory does not change.
- A request with no response is restored as interrupted.
- Todos come from `session.json`.

## Open questions

- **Failed responses** are printed but aren't events today. Should snapshots record them, e.g. as `000011-error-9-codex+gpt-6-luna.txt`, so a restored request shows as failed rather than interrupted?
- **"Avoid cd'ing"** was read as "`.restore` doesn't change directory". Did it also mean dropping the fish-style auto-cd for words starting with `.`, so that dot commands can't clash with it?
- **Todos and watches** stay in `session.json` for now. Should they become events later?
