# Agent hooks

Agent hooks make replies faster for Claude Code and Copilot CLI.

A hook is a normal feature of those tools: the agent runs a small script when
something important happens. Tebis installs a local script that forwards the
summary to Telegram.

If hook setup fails or the agent does not support hooks, Tebis still works by
reading the terminal output.

## When to use hooks

Use hooks if your default agent is Claude Code or Copilot CLI and you want
completion messages to arrive promptly in Telegram.

You do not need hooks for basic sending, reading, or session control.

## Enable hooks during setup

If your default agent command looks like a supported agent, `tebis setup` asks
whether to install hooks automatically when that agent starts.

This writes project-local hook config for the selected project.

## Manage hooks manually

```sh
tebis hooks install   [<project-dir>] [--agent claude|copilot]
tebis hooks status    [<project-dir>]
tebis hooks list
tebis hooks uninstall [<project-dir>]
tebis hooks prune
```

If `<project-dir>` is omitted, Tebis uses the default agent directory from your
config file.

## What Tebis installs

| Agent | Forwarded events |
| --- | --- |
| Claude Code | Final replies, subagent replies, permission prompts, idle notifications |
| Copilot CLI | Completion, permission, and idle notifications |

Copilot CLI does not expose the same final-reply events as Claude Code, so some
Copilot replies may still come from terminal output.

## Safety

- Tebis removes only hook entries it created.
- Your own hook entries are left alone.
- Hook replies are accepted only from your operating-system user.
- Unix hook scripts need `jq` and `nc` on `PATH`.

On Debian or Ubuntu:

```sh
sudo apt install jq netcat-openbsd
```

## Disable hooks

```sh
tebis hooks uninstall [<project-dir>]
```

To stop automatic hook installation, set this in your config file and restart
Tebis:

```sh
TELEGRAM_HOOKS_MODE=off
```
