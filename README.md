# cris_tools_proxy

A small Rust program that lets voice commands (for example from Dragon) drive
CRIS Tools over a local socket. If CRIS Tools is not running it will launch it.

```
cris_tools_proxy.exe <command>
```

`<command>` is the name of a registered automation in CRIS Tools (for example
`load_event_history` or `launch_maxims`). The proxy sends `run/<token>/<command>`
to the app's NNG listener on `tcp://localhost:<port>` and prints the response,
where `<token>` is read from the app's per-run `ipc.token` (see below). If no
token is found it falls back to the legacy `run/<command>` form.

## Authentication

CRIS Tools writes a random token for the current run to
`%LOCALAPPDATA%\cris-tools\ipc.token` (mode 0600; `$XDG_RUNTIME_DIR` on Linux)
and rejects any command that does not present it. This stops other users or
sessions on a shared workstation from driving clinical automations. The proxy
reads the same file and includes the token on every request.

Rollout: both sides accept the legacy form until
`[commands] require_token = true` is set in the app's `config/global.toml`
(and `require_token = true` in this proxy's config). Set the app's flag once
every deployed proxy sends tokens, and the proxy's flag once every deployed app
accepts them.

## Configuration (`cris_tools_proxy.toml`)

The config is looked up **next to the executable** first, then in the working
directory, so it works no matter what CWD the proxy is launched from.

| Key | Meaning |
| --- | --- |
| `port` | Port the app's command listener binds. Must match `[commands].port` in the app's `config/global.toml`. |
| `cris_tools_path` | Fallback executable used when no published build is found. |
| `app_root` | Optional directory containing `app/current.txt` and the build zips. Defaults to the config file's directory; relative paths resolve against it. |
| `require_token` | When true, refuse to send if the app's IPC token file is missing instead of using the legacy protocol. Default `false`. |
| `ipc_token_file` | Optional override for the app's `ipc.token` path. Defaults to `%LOCALAPPDATA%\cris-tools\ipc.token`. |

## Launching a published build

When the app is not running and the user chooses to launch it, the proxy looks
for `app/current.txt` (written last by `build_cris_tools.py` after publishing a
zip), extracts the matching `app/cris_tools_<version>.zip` into a per-user run
cache (`%LOCALAPPDATA%\cris-tools\run\<version>\`), and launches it from there
with `CRIS_TOOLS_DATA_DIR` pointed back at the share so `config/`, `plugins/`
and logs stay shared. Running from a local cache keeps startup fast and avoids
per-launch AV extraction. Cancelling the progress dialog aborts the launch.
