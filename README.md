# Lynn

Lynn is an experimental shell for humans and coding agents. You type shell commands and agent requests at the same prompt.

Lynn brings a shell and coding agents into one session. The shell runs your commands and agents handle coding requests using shared session context.

Lynn is named in honour of biologist Lynn Margulis, whose work on endosymbiosis helped explain how the mitochondria in our own cells, and the chloroplasts in plants and algae, originated from bacteria that came to live within other cells.

## Install

With Rust and Cargo installed, install from this checkout and start Lynn:

```sh
cargo install --path .
lynn
```

Both the package and executable are named `lynn`.

## Why use Lynn

A coding agent can usually change all the files in your project. Lynn gives you more control:

- Each agent request has a suffix. The suffix sets the access level of the request.
  - `?` gives read-only access. The agent can read files, but it cannot change them.
  - `!` gives read-write access. The agent can change files and run commands.
- On macOS, a system sandbox prevents file writes during `?` requests, except to temporary directories and the agent's own state. This protection stays on if the agent's own restrictions fail.
- You can run agents and shell commands as a separate Unix user, `staffer`. This user has less access than your own account.
- All agents share one session. An agent can see your notes, recent command input (and exit statuses but not output), watched command output, and answers from other agents.

Watch a command with `+ bash? git status`, or watch an Atom or RSS feed with `+ FEED URL`. Lynn shares the results with agents; enter `+` to refresh the watches. See [Watch a command or URL](#watch-a-command-or-url) for examples.

Lynn does not give full protection. Read [Limits of the protection](#limits-of-the-protection) before you use it.

## Requirements

- macOS or Linux.
- Rust and Cargo, to install or build Lynn.
- One or more agent CLIs, installed and authenticated: Claude Code, Codex, Pi, or fx.
- Optional, on macOS: Apple's `fm` CLI, with its license accepted.

## Build and run

Run `make` to rebuild both debug and release binaries. Use `make debug` or `make release` for just one, and `make test` to run the tests. The binaries are at `target/debug/lynn` and `target/release/lynn`.

To build and start Lynn in one step, run `cargo run`. Run `cargo run -- --help` to show help and exit. At the Lynn prompt, enter `?` to show a cheatsheet of prefixes and built-in commands. On macOS, the cheatsheet also shows the command that creates the `staffer` account.

## Agent requests

### Send a request

Type the agent name, the suffix, and the task:

```text
claude? explain how src/session.rs classifies input
codex! add a test for the input parser
```

Lynn supports these agent names:

| Name | Agent |
| --- | --- |
| `claude`, `fable`, `opus`, `sonnet`, `haiku` | Claude Code |
| `codex`, `sol`, `luna`, `terra` | Codex |
| `pi` | Pi |
| `fx` | fx (`fx?` requires macOS) |
| `fm` | Apple Foundation Models (macOS only) |
| `jsc` | JavaScriptCore shell (macOS only) |

Each request runs in the background. You can continue to type commands while the agent works.

Mutating requests (`!`, `!!`, `!!!`, and `?!`) share one serial queue across all models in this Lynn session. For example, `sol!` waits for an earlier `opus!` to finish or fail. Lynn shows `queued behind mutating request #n` while waiting and builds the request's context when it starts. An earlier mutating request waiting for referenced results keeps its place in the queue.

Read-only requests (`?`, `??`, and `???`) run in parallel without waiting for the write queue, unless they explicitly refer to an unfinished result. A bare interactive write selector such as `codex!` waits for queued mutations before opening its CLI. The write reservation survives `reset` until an already running mutation finishes. This queue coordinates agents in one Lynn process; ordinary shell commands and other Lynn processes are independent.

For a short read-only request, use `🎵 task` or `🎶 task` for `opus? task`, `☀️ task` for `sol? task`, `🌙 task` for `luna? task`, `🥧 task` for `pi? task`, or `🍎 task` for `fm? task`.

`Opus? task` also works as `opus? task`.

Repeat the final `?` or `!` to request more reasoning from Claude Code or Codex agents. `opus?? task` and `sol!! task` request `xhigh`; `opus??? task` and `sol!!! task` request `max`. `?` remains read-only and `!` retains write access. A single suffix uses the agent's normal effort setting. This also works with a read-only group, such as `opus/luna?? task`, or with a bare interactive selector such as `opus??`. Lynn shows the requested suffix in each result label, for example `opus??#3`. The selected model or your provider settings may limit the actual effort; if the provider rejects the requested level, Lynn reports its error.

Use `opus?! task` or `luna?! task` to give an agent write access and show live progress while it works. Lynn updates the pending line from Claude Code's or Codex's streamed events, then records the complete final answer as `opus?!#n` or `luna?!#n`. This works with all Claude Code and Codex model names. The stream is shown only in the terminal; the shared session stores the final answer.

You can paste a multiline request, including its `opus?` prefix, into the interactive prompt. Lynn keeps the pasted line breaks in one request; press Enter after pasting to send it.

Enter an agent name with no request, for example `opus?` or `codex!`, to open that agent's own interactive CLI in the terminal. With `?` the session is read-only, using the same read-only tools and macOS sandbox as a `?` request. With `!` it can change files, using the same tools and sandbox settings as a `!` request. Bare `fx?` or `fx!` opens fx interactively. Bare `jsc?` or `jsc!` opens the jsc REPL, and bare `fm?` or `fm!` enters fm mode. When you exit the agent, you return to the Lynn prompt.

`jsc` is not a model: Lynn runs the request as JavaScript in the macOS JavaScriptCore shell and replies with what the script prints. Use `print()` for output, for example `jsc? print(6 * 7)`.

### How Lynn applies read-only access

For a `?` request, Lynn uses these restrictions:

- **Codex:** Lynn uses the Codex `read-only` sandbox. Lynn ignores your Codex user configuration, so an automatic approval setting cannot cancel read-only mode.
- **Claude Code:** Lynn uses restricted mode and an explicit list of tools.
- **fx:** `fx?` uses the macOS system sandbox to enforce read-only access; fx has no native read-only mode, so `fx?` is unavailable on Linux. `fx!` uses `fx ask --auto`, allowing edits subject to fx's automatic permission review. Background requests use `--no-save` and share Lynn's session context.
- **Pi:** Lynn uses an explicit list of tools.
- **fm:** Lynn controls the tools. The agent can list directories and read files in the current directory. It cannot write files or run commands.
- **macOS system sandbox:** For Claude Code, Codex, Pi, fx, and jsc, a macOS sandbox denies all file writes except to `/dev`, the temporary directories (`/tmp` and `/var/folders`), and the agent user's `~/.claude`, `~/.claude.json*`, `~/.codex`, `~/.pi`, `~/.fx`, `~/.cache`, and `~/Library/Caches`. This protects your project, its Git index, other repositories, and the rest of your home directory.

For a `!` request, Codex uses its `workspace-write` sandbox. Other agents can write files and run commands.

### Run an agent as the `staffer` user

Put `@staffer` before a request to run the agent as the `staffer` Unix user:

```text
@staffer opus? Is there a README.md yet?
```

Before you use `@staffer`, do these steps:

1. Create the `staffer` account. On macOS, enter `?` to see the command.
2. Configure passwordless `sudo` for `staffer`. Lynn uses noninteractive `sudo`.
3. Give `staffer` access to the agent CLI.
4. Authenticate the agent CLI as `staffer`.
5. Give `staffer` permission to read the project.

You cannot use `@staffer` with `fm`, because the `fm` tools run inside Lynn.

### Ask more than one agent

Use a slash between agent names to send the same read-only question to each agent:

```text
opus/luna? what is a modern alternative to tree?
```

Each agent runs in the background and gets its own reply label.

### Read and refer to answers

Each request gets a label, for example `codex?#3` for read-only access or `opus!#4` for read-write access. Numbers are unique across all agents.

- When the request starts, Lynn shows `codex?#3: …`.
- When the answer is ready, Lynn shows `codex?#3: answer`.
- Press Down at an empty prompt to show each running or queued request above its status line, and expand existing replies with their original requests and model details. Claude Code reports the model or models used in `modelUsage`, for example `opus?#3 [used claude-opus-5-5]: answer`. Codex does not report the actual model in its result events, so Lynn shows its explicitly requested model as unverified, for example `[requested gpt-6-luna; actual model unreported]`. Compact replies and `#3` recall omit this detail.
- To show the latest completed agent reply, enter `#`. To show a specific full answer, enter `#3` or `codex?#3`. Older labels such as `codex#3` also work.
- Claude, Codex, Pi, fx, and fm read-only agents are asked for a concise one-line answer, preferably under 40 words, unless you explicitly request more detail or a structured answer. Lynn displays their replies without clipping; long lines wrap on screen.
- To give an answer to a different agent, refer to it in the request. For example: `claude? do you agree with #3?`
- If a request refers to an unfinished answer, Lynn queues it in the background, shows `waiting for #3`, and starts it when the answer arrives. A request with several references waits for all of them. Lynn builds the queued request's context after those answers enter the session log and includes each referenced answer once. If a referenced request fails, the queued request reports the failure.
- Entering `#3` by itself while it is still running reports that it is still in progress.

Labels stay available until you exit Lynn or enter `reset`.

### Use numbered results as command inputs

Use an unquoted reference as a filename argument to an ordinary command. Lynn supplies the result through a pipe exposed as `/dev/fd/n`; the command does not need to know about Lynn, and no temporary result files are created:

```text
cat #3 #7 >> brief.md
cat requirements.md #3 #7 > brief.md
wc -w #3
diff #3.1 #3.2
cat < #3
```

Agent replies, HTTP results, and watched items are supported. `#3` selects a watch's latest successful result; `#3.2` selects revision 2. Each input uses the same source headings and newline separators as explicit references passed to agents. Watch excerpts have the same 128 KiB limit. The saved file is a snapshot of those results.

Lynn waits for unfinished replies or a watch's first result, and resolves all references before opening output files. Repeated references produce separate input streams in argument order. The descriptors are closed after use and can be reused by later commands. These inputs are streams; commands that require seeking or rereading should use a saved file instead.

Quote or escape a reference to use a literal filename, for example `cat '#3'`, `cat \#3`, or `cat ./#3`. References cannot be output destinations. This expansion applies to Lynn's ordinary command parser; scripts run through `bash!`, `zsh!`, or shell command substitution keep the host shell's behavior.

### Put an agent answer in a command

Use `name?(task)` as one argument of a shell command. Lynn waits for the read-only answer, then gives the answer text to the command as one argument:

```text
git commit -m luna?(Suggest a commit message)
```

Obey these rules:

- Do not put `(` in the task. The first `)` ends the task.
- Do not put text immediately after the `)`.
- If the `)` is missing, Lynn shows an error.

### Use fm mode

On macOS, `fm?` and `fm!` use Apple Foundation Models:

```text
fm? explain how src/session.rs classifies input
fm! create a file named note.txt containing hello
```

To send many read-only `fm` requests, enter `fm?` or `fm!` with no task. The prompt changes to `fm?>`. Each line that you type is then an `fm?` request. To go back to the shell prompt, enter `.` or `exit`, or press Ctrl-C. Agent requests continue to run after you go back.

Lynn examines tool access for each `fm` step. Lynn skips repeated tool calls. When the step limit is reached, Lynn tells the agent to give a final answer.

For a summary of earlier session results, such as `fm? summarize the ops/s`, Lynn sends the shared session context directly to `fm` instead of starting a file search. Refer to a specific reply with `#n` when you want that reply alone summarized.

## Shell commands

### What agents see from commands

For an ordinary command such as `git status`, Lynn shows you the output in the terminal, but gives agents only the command text and exit status. It does not add that command's stdout or stderr to the shared session.

Agent requests receive the full session history of commands, comments, agent requests, and replies, plus current todos. Older instructions are retained until `reset` or exit; Lynn does not drop conversation events by count or truncate the assembled conversation. Watched outputs and web pages use bounded excerpts with visible omission markers. Explicit `#n` references supply stored results without duplicating them in the automatic context. Long sessions therefore produce larger prompts; there is no automatic conversation summarization.

To share a command's output with agents, register a watch such as `+ bash? git status`. Lynn includes watch output in agent context. Shell watches run as `staffer`, and you refresh them by entering `+` or `+-`. Agents can also run commands themselves when their access allows it, so this controls what Lynn shares automatically rather than what an agent can ever learn.

### Commands that run as you

Lynn runs these commands directly, without an LLM:

- Commands from `PATH`, with arguments, quotes, and backslash escapes.
- `$NAME` and `$?` expansion.
- Pipelines and the redirections `<`, `>`, `>>`, `2>`, and `2>>`.
- `&&` chains: `cargo test && cargo build` runs the build only if the tests succeed. Each command can be a pipeline. Built-in changes, such as `cd project && pwd`, apply to later commands in the chain.
- Fish-style directory shorthand: enter a directory path such as `..`, `./project`, `/tmp`, or `project/` on its own to change into it. Bare names such as `project` remain commands.
- Built-in commands: `cd`, `export NAME=value`, `unset NAME`, `NAME=value`, and `exit`.

Lynn detects unquoted `&` and stops before running the line, with a suggestion to enter `zsh!` or `bash!`. Run background jobs inside that interactive shell, where `jobs`, `fg`, and `wait` share its job state. Directory changes, variables, and jobs belong to that shell; exiting returns to Lynn's own state. Quoted or escaped ampersands remain literal arguments. Explicit `bash! command` and `zsh! command` scripts are interpreted by their shell.

At the interactive prompt, an unknown first command turns red as you type. Commands found in `PATH`, executable paths, and Lynn built-ins keep the normal color. For example, `opus` turns red if you meant to type `opus?`.

If a command contains `$(...)`, Lynn runs the full command through Bash or Zsh. Lynn uses `$SHELL` if it is Bash or Zsh. If not, Lynn uses Zsh on macOS and Bash on Linux. Built-in commands such as `cd` in this shell do not change the directory of Lynn.

Enter `bash!` or `zsh!` alone to open a normal interactive shell. Within that shell, directory changes, variables, functions, and environment activation persist across commands, and you can enter multiline loops and conditionals. Type `exit` or press Ctrl-D to return to Lynn. The shell starts in Lynn's current directory; changes inside it do not change Lynn's directory or the environment used by agents. Its state ends when you leave it. Commands entered inside the shell are not added to Lynn's shared session; agents see only the shell entry and its exit status. This mode requires a terminal.

To run a single command through a specified shell, use `bash!` or `zsh!` followed by the command:

```text
bash! for f in *.rs; do wc -l "$f"; done
```

Lynn does not change the script and does not use an LLM. Changes to the directory or environment of the child shell do not go back to Lynn.

### Git worktrees

Create a local branch in a separate worktree, then change into it yourself:

```text
git worktree add -b qip-wav-to-mp3-lame ../qip-wav-to-mp3-lame scaleway-m1b/qip-wav-to-mp3-lame
cd ../qip-wav-to-mp3-lame
opus? Review this branch
```

Lynn starts `opus?` in its current directory, so the read-only agent can read that worktree. Finish and commit the changes there. Then return to your original checkout, merge the branch, and remove the extra worktree:

```text
git status
git add -A
git commit -m "Add WAV to MP3 conversion"
cd -
git merge qip-wav-to-mp3-lame
git worktree remove ../qip-wav-to-mp3-lame
```

### Commands that run as `staffer`

`bash? command` and `zsh? command` run the command as the `staffer` user through `sudo`.

- In interactive input, `sudo` can ask for your password.
- In noninteractive input, Lynn uses `sudo -n`. If `sudo` needs a password, the command fails immediately.
- If your Lynn user owns the Git repository, Lynn lets `staffer` read it with Git. Lynn sets `safe.directory` for that one command only.

**Warning:** `staffer` can write to all paths that `staffer` has write access to, for example group-writable paths. `bash?` and `zsh?` are not a read-only sandbox.

### Watch a command or URL

Put `+` before `bash?`, `zsh?`, `HEAD`, or `FEED` to watch a command or URL:

```text
+ bash? git status
+ HEAD https://github.com/patrickgwsmith/lynn
+ FEED https://github.com/patrickgwsmith/lynn/commits/main.atom
+ FEED https://static.crates.io/rss/updates.xml
+ https://github.com/patrickgwsmith/lynn/commits/main.atom
```

- Lynn runs each watch once when registered and again only when you enter `+` or `+-`. Shell watches run as `staffer`; HEAD and feed watches use Lynn's HTTP client.
- `+ URL` accepts Atom and RSS feeds when their responses have `Content-Type: application/atom+xml` or `application/rss+xml`. Use `+ FEED URL` to parse a feed served with a generic XML content type, such as the crates.io RSS feed. Feed entries show their title and link; `+` shows new entries and `+-` also shows entries that disappeared from the feed.
- Agents can see watch output in the shared session.
- Shell watches retain up to 4,000 lines or 1 MiB for display and `#n` recall. Longer results keep the beginning and end, with an omission marker in the middle. To keep agent requests manageable, Lynn includes up to 16 KiB per watch and 64 KiB total automatically, or up to 128 KiB when you refer to a watch with `#n` or `#n.r`; these excerpts also keep the beginning and end.
- Shell watches need passwordless `sudo` for `staffer`. Errors show at the prompt.
- You can watch more than one command or directory at the same time.

Enter `+` alone to run each watch again and show only added lines. Enter `+-` to show both added and removed lines. Unchanged lines are omitted. Each check compares with the previous `+` or `+-` check, or with the first result if you have not checked yet.

Every watch, HTTP request, and agent request uses the same number sequence. A watch's first successful result is revision 1; Lynn adds another revision whenever its output changes. For example, `#3` shows watch #3's latest successful result, and `#3.2` shows its second revision. Agents can use the same references: `opus? What changed between #3.1 and #3.2?` A request for a watch still loading waits for its first result. Repeating the same `+` command keeps the existing watch number; enter `+` to refresh it. Watch revisions last until `reset` or exit.

Numbered HTTP results, such as `[head #2]`, can also be referenced in an agent request with `#2`.

### Example: build a Base64 crate

In a fresh Lynn session, write down the goal, create the crate, and watch recent crates.io releases. The feed is shared context; the crate's feature plan does not depend on a particular release appearing in it. Ask Opus to research the feature set, Pi to implement it, and Luna to review the code. This transcript shows illustrative, shortened output; enter each request after the preceding reply arrives:

```text
# I want to make a base64 encoder in Rust
cargo new --lib mini-base64
cd mini-base64
+ FEED https://static.crates.io/rss/updates.xml
[watch #1 loading] + FEED https://static.crates.io/rss/updates.xml
[watch #1.1 + FEED https://static.crates.io/rss/updates.xml] New crate version published: ...
opus? Research the features a small Base64 Rust crate should support. Suggest a public API and test cases for standard and URL-safe encoding.
opus?#2: Support both alphabets, specify padding behavior, and test RFC 4648 vectors plus empty and non-ASCII bytes.
pi! Implement the API and tests proposed in #2, with documentation for standard and URL-safe encoding.
pi!#3: Added standard and URL-safe encoders, padding options, documentation, and test vectors.
luna? Review #3, src/lib.rs, and the tests for correctness and API clarity before publishing.
luna?#4: The encoders and tests cover the key cases; check the package metadata before publishing.
```

The first `#` line is a note shared with all agents. The feed is `#1`, Opus's research is `#2`, and Pi's implementation is `#3`. You can send Luna's review request while Pi is still working; referring to `#3` queues it until Pi's result arrives. After you review the code, address Luna's feedback, choose an available crate name, and fill in the crate's metadata, you run the publication commands yourself:

```text
cargo test
cargo publish --dry-run
cargo publish
```

### Notes, todos, and URLs

Lynn keeps these items in the session, and agents can see them:

- Comments that start with `#`.
- Todos that start with `- [ ]`.
- Web pages. Enter an HTTP or HTTPS URL to load the page in the background. Lynn keeps up to 4 MiB of each page in memory. Agents get a part of the page.

Enter `-` alone to show the current TODO list.

Lynn does not follow redirects. It shows the new URL. To load that page, enter the new URL.

Enter `HEAD https://...` to show the response headers and the time to receive them. Enter `+ HEAD https://...` to register a HEAD watch, then enter `+` when you want to refresh it and compare results. Lynn does not (yet) show separate DNS, TCP, and TLS times.

### Reset the session

Enter `reset` to clear:

- The session.
- The input history.
- The watched commands.
- The request numbers. The next request is `#1`.

`reset` does not change the current directory or the environment. Requests that are in progress can continue to run, but Lynn discards their results. To reset the terminal, enter `bash! reset`.

## Prompt and editing

The prompt shows a short form of the current directory. It shows the first character of each parent directory and the full name of the current directory. It shows your home directory as `~`. For example, Lynn shows `/Users/pgwsmith/Collected/lynn` as `~/C/lynn>`.

In an interactive terminal, Lynn sets the terminal title to the same shortened directory path shown in the prompt and updates it after `cd` or a child command changes the title.

Press Down at an empty prompt to show its full path until you enter a command or press Ctrl-C. The next prompt uses the short path again. Lynn also reprints completed agent results with the request that produced each one. New results stay compact; press Down again to expand those results.

Colors help you see the access level before you press Enter:

| Color | Input |
| --- | --- |
| Cyan | The prompt |
| Magenta | Agent requests, for example `opus/luna?` and `codex!`, and the `fm?` mode label |
| Blue | Commands as `staffer`: `bash?`, `zsh?`, and `@staffer` |
| Yellow | Commands as you through a shell: `bash!` and `zsh!` |

Entered commands and agent requests appear in bold, while selector colors still show their access level. Expanded agent results show the original request in bold. Watched commands, for example `+ bash?`, use the same colors. Agent answers color the reply label, Markdown bold text, bullet marks, and inline code. To remove the colors, set `NO_COLOR=1`.

Editing keys:

| Keys | Action |
| --- | --- |
| Tab | Complete a command, file, or directory |
| Ctrl-A, Ctrl-E | Move to the start or end of the line |
| Ctrl-B, Ctrl-F | Move back or forward one character |
| Option-B, Option-F | Move back or forward one word |
| Ctrl-W, Option-D | Delete a word |
| Ctrl-U, Ctrl-K | Delete to the start or end of the line |
| Ctrl-Y | Put back the text that you deleted last |
| Ctrl-P, Ctrl-N | Show the previous or next history item |
| Ctrl-C | Cancel the current line |
| Ctrl-D | Exit Lynn (at an empty prompt) |

Tab completion accepts relative paths, absolute paths, and paths that start with `~/`. It adds escapes to spaces in names. Lynn also shows suggestions from your history as you type.

Type `!` at an empty prompt and press Tab to recall the last command if it was a read-only agent request. For example, `opus? explain this` becomes `opus! explain this`, preserving the instruction. Repeated question marks become matching exclamation marks (`sol??` becomes `sol!!`). The recalled request stays in the editor until you press Enter. This shortcut uses only the most recent command and applies to requests for a single agent.

Type `?` at an empty prompt and press Tab to recall the most recently used agent selector without its instruction, for example `opus?`. This searches back past ordinary commands and always uses read-only access with the default reasoning effort.

## Limits of the protection

- The macOS sandbox still lets a `?` agent write to temporary directories and to the agent CLIs' own state and cache directories in the agent user's home directory.
- On Linux, `fx?` is unavailable because fx has no native read-only mode. Other `?` requests use only the restrictions of each agent CLI. Lynn does not add a system sandbox.
- `!` requests can change files and run commands.
- `bash?`, `zsh?`, and `@staffer` have all the access of the `staffer` user.

## Other limits

- Lynn's top-level command parser supports `&&`, but does not support other shell control syntax or globbing. Use `bash! command` or `zsh! command`, or enter `bash!` or `zsh!` alone for an interactive shell with state that persists until you exit it.
- History and the session are in memory only. Lynn discards them when you exit.
- Lynn reserves `?` and `!` without an agent name for a future default model.
- Lynn recognizes all `name?` and `name!` forms as agent requests. If the agent is not available, Lynn shows an error and does not run a shell command. Names can contain letters, digits, underscores, hyphens, and periods. The `?` or `!` must be at the end of the first word.
- Lynn reserves other syntax for future use.

## TODO

- [ ] Support `opus?sol! task` as an automatic two-stage request: Opus makes a read-only plan, then Sol receives the original task and completed plan and implements it with write access. Give each stage its own result number; do not start Sol if Opus fails.
