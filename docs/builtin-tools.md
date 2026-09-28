# Built-in tools

tau ships eight native tools — `read`, `write`, `edit`, `ls`, `grep`,
`find`, `bash`, `powershell` — so the agent can work on a repository with
nothing installed first. They are registered into the same
`ToolRegistry` that `-e` components fill, and they are **host code**: they
read, write, and spawn processes with the permissions of the tau process.

```
tau (host)  ──registers──►  read write edit ls grep find bash powershell
            ──registers──►  tools from -e wasm components (shadow built-ins by name)
```

A tool that is on by default cannot require an install, which is why
`grep` and `find` search natively instead of shelling out to ripgrep and
fd — pi downloads those on first use, and tau core has no HTTP. The
crates are ripgrep's own (`regex` for the engine, `ignore` for the
walker, `globset` for `--glob`), so the semantics match where the model
can see them.

## The eight tools

| tool | what it does | limits |
|------|--------------|--------|
| `read` | file contents verbatim, no line numbers; images come back as an image block plus `Read image file [mime]` | 2000 lines / 50KB per call, `offset`/`limit` to page |
| `write` | creates or overwrites a file, parent directories first; `Successfully wrote to <path>` | — |
| `edit` | one or more `{oldText,newText}` replacements, matched against the original file; exact match first, then a fuzzy pass (NFKC, smart quotes and dashes folded) | — |
| `ls` | directory listing, sorted case-insensitively, `/` suffix for directories | 500 entries |
| `grep` | regex or literal search, `path:line: text`, `-` separators for context lines; gitignore respected, hidden files searched | 100 matches, 500 chars per line, 50KB |
| `find` | glob search, paths relative to the search root, `src/**/*.spec.ts` matches as written | 1000 results, 50KB |
| `bash` | runs a command through bash, stdout and stderr merged in arrival order | last 2000 lines / 50KB, full output to a temp file |
| `powershell` | the same, through `pwsh.exe` or `powershell.exe` (Windows only) | same |

`read`, `write`, `edit`, `ls`, `grep`, `find` are the same tools pi has,
with pi's schemas, descriptions, output shapes, error texts, and
truncation limits — the point is that a model with pi experience is
already fluent in them. `bash` and `powershell` are pi's shell tools:
same flags, same timeout semantics, same output notices.

## The two flags

```
tau --tools read,grep -p "…"      # exactly these tools, built-in or component
tau --no-builtin-tools -p "…"     # no native tools at all
```

- Every run that starts an agent prints which built-ins it registered —
  including `none`. The administrative subcommands (`tau tree`,
  `tau consent`, …) never reach the agent and say nothing.

  ```
  [tau] built-in tools: bash, edit, find, grep, ls, powershell, read, write
  ```

- `--tools` **replaces** the selection, and it names component tools as
  well as built-ins (pi's flag works the same way). A component tool it
  does not name is loaded but never offered to the model, and the load
  log says so. Naming only a component tool is a legitimate selection —
  the startup line then reads `none`, because it counts *built-ins*, and
  the lines below it name what the run actually has:

  ```
  [tau] built-in tools: none
  [tau]   tool: upper
  ```
- A `--tools` name that nothing answers to stops the run
  (`unknown tool: nope (available: …)`, exit 1). A typo must not quietly
  hand the model a smaller toolset than you think it has.
- Registration is last-wins: a component tool with the same name shadows
  the built-in, exactly as pi lets an extension replace a built-in.
- `--tools` and `--no-builtin-tools` conflict (clap exits 2).

## Paths

The working directory is captured once at startup (`std::env::current_dir`,
falling back to `.`) and every relative path in every tool resolves
against it — the same value the session payload reports as `cwd`.

Arguments are normalized before they are resolved: `~` and `~/…` expand,
one leading `@` is dropped, unicode spaces become ASCII spaces, and on
Windows a shell-style path (`/c/Users/…`, `/mnt/c/Users/…`) becomes
`C:\Users\…`.

## Shells

`bash` finds its shell the way pi does: `TAU_BASH_PATH` if set, then Git
Bash in its install locations, then `bash` on `PATH`, then `/bin/bash`
then `bash` then `sh` elsewhere. Windows' own `System32\bash.exe` — the
WSL launcher — is recognised by path and handed the script on stdin,
which is the only way it takes one. `powershell` prefers `pwsh.exe`
(PowerShell 7) over `powershell.exe`, runs it with
`-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command`, and
prefixes every command with the line that switches PowerShell's console
encoding to UTF-8. `TAU_POWERSHELL_PATH` overrides discovery the way
`TAU_BASH_PATH` does.

```
tau --tools bash -p "use bash to count the files here"
```

Two behaviours worth knowing:

- **A timeout kills the process tree.** `timeout:` is in seconds; when it
  fires, the shell *and everything it started* is killed —
  `taskkill /F /T` (from `%SystemRoot%\System32`, never from `PATH`) on
  Windows, the process group on unix. A command that leaves a server
  running does not outlive its own timeout. Output is not lost either:
  what the command printed before the kill comes back with
  `Command timed out after N seconds`.
- **Output that outgrows the limits is moved, not dropped.** The model
  sees the tail (last 2000 lines or 50KB) with a notice naming the range,
  and the whole stream — head included — is in a temp file
  (`tau-bash-<id>.log`) whose path the notice prints.

A command that exits non-zero is an **error result** carrying its output
and `Command exited with code N`; a command killed by a signal reports
`128 + signal`, never a silent success. A tool call is not interrupted by
Ctrl-C: the interactive abort is checked between model events and tool
batches, not inside a running tool, so a long command is bounded by its
`timeout:` and nothing else.

## Honesty about the sandbox

These tools are host code, and nothing about the wasm sandbox applies to
them:

- They are **not** affected by `--deny-wasi`. That flag governs what
  wasm components may do; the built-ins run in the tau process itself
  (`docs/extensions.md` §7).
- They do **not** ask for approval. `--allow-inject`, `--remember`, the
  consent store — none of it gates them. A tool call runs.
- The only thing that constrains them is `--tools` / `--no-builtin-tools`
  at startup. `--no-builtin-tools` means the toolset is exactly what the
  loaded components provide.

If you need a run that cannot touch the filesystem or spawn a process,
that is `--no-builtin-tools` plus a component that does the narrow thing
you want — or, later, ACP mode, where mutating built-ins go through the
editor's permission prompt.

## `--demo`

`--demo` scripts one tool call so a run has something to show. Which tool
it may script is a policy, not a coin flip: the built-ins carry a demo
tier, and only the read-only four (`read`, `ls`, `grep`, `find`) that the
user named with `--tools` are scriptable. The mutating four (`write`,
`edit`, `bash`, `powershell`) never are, even by name — `tau --tools bash
--demo -p "rm -rf x"` registers bash and runs nothing.

The point is that the recorded transcripts in `README.md`,
`docs/tutorial.md` and `scripts/validate.sh` are stable, and that
"`--demo` cannot run a shell" is an assertion (`scripts/validate.sh` 1e),
not a hope.

## Differences from pi

Deliberate, and each one is in the tool's own module docs:

- **grep/find are native.** ripgrep's ignore-file precedence corner cases
  (`.ignore`, `.rgignore`, parent-directory lookup) are not reproduced:
  the walker honours `.gitignore` — even outside a git repository, as pi
  passes `--no-require-git` there — plus hidden files and a skipped
  `.git`.
- **find patterns match relative paths.** fd matches a path-shaped
  pattern against the *absolute* candidate, which is why pi prepends
  `**/` to every pattern; here `src/**/*.spec.ts` matches as written.
- **No image resizing, no `details`/diff channel, no `promptSnippet`.**
  tau has nowhere to put a diff payload, and the per-tool system-prompt
  guidelines pi injects are not ported.
- **No tool progress events.** A long command reports when it is done.
- **`powershell` is not registered off Windows** (pi registers it and
  fails at call time).
- **`read` reports its own error text** (`Cannot read file: <path>: …`)
  rather than a raw runtime error.

## Testing

`cargo test -p tau-tools` covers the crate — the pi edge cases
(truncation notices, `offset` past the end, grep limits and context, the
edit error wordings, CRLF/BOM round-trips, gitignore without a repo,
timeouts that kill a grandchild) — and needs no API key. The CLI legs are
`scripts/validate.sh` 1d and 1e.
