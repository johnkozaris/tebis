# Dashboard

Tebis can expose a small local browser dashboard for the same machine.

Enable it during setup, or set:

```sh
INSPECT_PORT=51624
```

Then restart Tebis and open:

```text
http://127.0.0.1:51624
```

![Tebis local dashboard](dashboard.png)

## What it shows

- current service state
- running terminal sessions
- recent activity
- handler counts and timings
- hook install status
- selected settings

If `BRIDGE_ENV_FILE` points at your Tebis config file, the dashboard can also
edit settings.

## Safety

The dashboard binds to `127.0.0.1` only and has no login page. Do not expose it
through a tunnel, reverse proxy, or public network interface.

To disable it, remove `INSPECT_PORT` from the config file and restart Tebis.
