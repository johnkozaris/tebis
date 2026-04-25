# Contributing

Thanks for helping improve Tebis. This project controls real local terminal
sessions through Telegram, so reliability and safety matter more than adding
features quickly.

## Before you open a pull request

1. Open an issue first for behavior changes, new dependencies, new network
   paths, or anything that changes who can send commands.
2. Keep changes focused. A feature PR should not also reformat unrelated
   files or rewrite docs outside its scope.
3. Add tests for behavior changes.
4. Do not log Telegram message text, terminal output, bot tokens, API keys, or
   notification payloads.
5. Escape text before sending Telegram HTML replies.

## Local checks

Run the checks that match your change:

```sh
cargo fmt --all --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo audit
cargo deny check
```

If you change shell hook scripts, also run:

```sh
shellcheck contrib/claude/claude-hook.sh contrib/copilot/copilot-hook.sh
```

The CI workflow runs formatting, clippy, tests, release build, dependency
policy checks, shellcheck, and Windows checks.

## Project map

| Path | Purpose |
| --- | --- |
| `src/main.rs` | CLI entry point and daemon startup |
| `src/config.rs` | Environment-file loading and validation |
| `src/bridge/` | Telegram message handling and session routing |
| `src/telegram/` | Telegram Bot API client |
| `src/platform/` | macOS, Linux, and Windows system integration |
| `src/setup/` | Interactive setup wizard |
| `src/agent_hooks/` | Claude Code and Copilot CLI hook management |
| `src/audio/` | Voice input and voice reply support |
| `src/inspect/` | Local dashboard |
| `src/notify/` | Local notification listener for agent hooks |

## Security-sensitive changes

Please discuss these before coding:

- changing Telegram authentication
- accepting usernames instead of numeric Telegram IDs
- adding any non-local listener
- changing how terminal session names are validated
- changing how keystrokes are sent
- changing config-file permissions
- adding a runtime dependency with a large network or TLS surface

For suspected vulnerabilities, do not open a public issue. Follow
[SECURITY.md](SECURITY.md).

## Pull request checklist

- [ ] The change is focused and documented where users need it.
- [ ] Tests pass locally or the PR explains why a test could not be run.
- [ ] New behavior has tests.
- [ ] No secrets, tokens, message text, or terminal output are logged.
- [ ] User-facing errors explain what to do next.
- [ ] New dependencies are justified.

## Commit messages

Use a short imperative subject, such as:

```text
Add Windows service status check
```

Add a body when the reason is not obvious.

Do not add `Co-authored-by: Claude`, `Co-authored-by: Copilot`, or any
AI-authored trailer.
