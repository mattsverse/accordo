# Accordo

Accordo runs development services together on **macOS and Linux**, with a Ratatui tabbed interface or a merged stream of numbered logs.

## Install

Once the first release is published, install a prebuilt binary with Homebrew:

```sh
brew install mattsverse/tap/accordo
```

Or install through mise:

```sh
mise use -g github:mattsverse/accordo@latest
```

With a current stable Rust toolchain, install from crates.io:

```sh
cargo install accordo --locked
```

With Node.js 22 or newer, install through npm or run directly with npx:

```sh
npm install -g @matfire/accordo
npx @matfire/accordo --help
```

The npm package automatically selects the matching binary from `@getaccordo`.
Keep optional dependencies enabled; no Rust compiler or postinstall download is needed.

All methods install the `accordo` command. Prebuilt binaries support Apple Silicon
and Intel macOS, and ARM64 and x86_64 Linux (glibc 2.39 or newer, such as Ubuntu 24.04).
Archives and checksums are also available on the [GitHub releases page](https://github.com/mattsverse/accordo/releases).

To install directly from this checkout:

```sh
cargo install --path .
```

For development, use `cargo run -- -f path/to/accordo.yaml`.

Maintainers: see [the release procedure](docs/releasing.md) for publishing and recovery.

## Configure and run

Create `accordo.yaml`:

```yaml
services:
  - name: app1
    cwd: ./apps/app1
    cmd: pnpm dev
    hide_stderr: true
  - name: app2
    cmd: cargo run app2
    autostart: false
layout: tabbed
```

Run `accordo`, or `accordo -f your_config.yaml` (`--file` also works). Use `accordo --help` for CLI help.

- `layout` defaults to `tabbed`; the other choice is `linear`.
- `autostart` defaults to `true`. Enabled services start concurrently.
- `hide_stderr` defaults to `false`. Set it to `true` on a service to hide its stderr in both views. Tabbed mode redirects stderr to `/dev/null` before launching the command; linear mode drains and filters it. Stdout and failure exit codes are unaffected.
- `cwd` defaults to the configuration file's directory. Relative directories resolve against that directory, even when Accordo is launched elsewhere. For a symlinked configuration, paths resolve beside the target file.
- Each `cmd` runs through `/bin/sh -c` with the inherited environment. Shell quoting, pipes, and redirects work; interactive shell aliases and profiles are not loaded.
- Configuration is validated before any service starts. Names must be unique and nonempty, commands must be nonempty, and working directories must exist. Unknown configuration fields are rejected.

## Tabbed view

Services appear in a numbered sidebar on the left, with their current status. Each service runs in its own pseudo-terminal (PTY), and the selected service's terminal screen appears on the right. The `vt100` engine and `tui-term` widget handle colors, cursor movement, progress-bar redraws, line wrapping, and alternate screens. Exited services remain available to inspect and restart. A restart retains available history and adds a run separator.

| Key | Action |
| --- | --- |
| `0`–`9` | Select service 0–9 (number row or numpad with Num Lock) |
| `↑` / `↓` | Cycle through all services, including those beyond 9 |
| Click a service | Select it in the sidebar |
| Mouse wheel / trackpad over logs | Scroll the selected service's log history |
| `PageUp` / `PageDown` | Scroll logs one page |
| `Home` / `End` | Jump to oldest retained logs / resume live output |
| `s` | Start or stop the selected service |
| `r` | Restart the selected service, or start it if inactive |
| `q` | Quit and stop every service |
| `Ctrl+C` | Quit and stop every service |

Tabbed layout requires interactive stdin and stdout. Terminal output follows the latest screen until you scroll up. Your place is preserved as output arrives and when switching services; scrolling back to the bottom or pressing `End` resumes live output. Wheel scrolling never changes the selected service. Each emulator retains up to 10,000 scrollback rows; alternate-screen applications control their own screen and do not contribute normal scrollback.

Accordo resizes every service's PTY to match the output pane and reports `TERM=xterm-256color`. Programs can redraw for the new size; existing wrapped output is not reflowed by the current engine. Stdout and stderr share the terminal, so stderr has no added prefix or forced color in this view. Lifecycle status stays in the sidebar and pane title rather than being injected into program output.

This mode displays terminal output while reserving the keyboard for Accordo's controls. Child stdin is a PTY and basic terminal queries receive replies, but typing and mouse input are not forwarded to applications. Interactive prompts therefore still require a separate terminal. The renderer supports text and terminal styles, not Ghostty-specific graphics or every modern terminal extension.

## Linear view

Set `layout: linear` to print ordinary terminal output:

```text
[0 app1] --- run 1 ---
[0 app1] --- running ---
[1 app2] --- stopped ---
[0 app1] ready on port 3000
[0 app1] [stderr] an example warning
```

The merged feed supports normal terminal scrollback, redirection, and piping. Service logs and lifecycle messages go to stdout; configuration and Accordo errors go to stderr. Stderr from a service is marked `[stderr]` in the merged feed.

There are **no keyboard bindings in linear mode**. `Ctrl+C` stops every service. Services with `autostart: false` remain stopped; linear mode exits once all enabled services finish. If none are enabled, Accordo prints an explanation and exits successfully.

## Process and output behavior

- A failed service does not stop the others. Services never restart automatically.
- Stop, restart, quit, and SIGTERM signal the service's entire Unix process group with SIGTERM. After three seconds, remaining members receive SIGKILL. Restart waits for cleanup before starting again.
- Descendants remaining after their main command exits are also cleaned up. Services must stay in their assigned process group; daemonized processes that create a new session/group are unsupported.
- In linear mode, stdout and stderr are read concurrently, preserving each stream's ordering. The merged feed follows arrival order; ordering across different streams is not guaranteed. Child stdin is closed.
- Linear mode strips terminal escape/control sequences, replaces invalid UTF-8, and turns carriage returns into separate records. Records are split at 16 KiB; an unterminated final record is still displayed. Tabbed mode sends raw PTY bytes directly to the terminal emulator, including partial lines and escape sequences.
- Output queues are bounded. A slow consumer applies backpressure to services while lifecycle controls remain responsive. A closed output pipe triggers process cleanup.
- Tabbed terminal state is restored before shutdown waits for processes. On user-requested shutdown, pending output is drained for cleanup but is not displayed.

Exit codes: `0` for successful completion (or a closed output pipe), `1` for configuration/service/application failures, `130` for Ctrl+C, and `143` for SIGTERM. In tabbed mode, `q` returns `1` if any service failure was observed during the session. CLI argument errors use Clap's exit code `2`.

## Development checks

```sh
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings -D clippy::pedantic -D clippy::perf -D clippy::suspicious
cargo check --all-targets --all-features
python3 tests/terminal_smoke.py
```

The terminal smoke test uses a real pseudo-terminal to exercise keyboard controls, resizing, and terminal restoration. Run it after `cargo test` has built the binary. CI runs these checks on macOS and Linux.
