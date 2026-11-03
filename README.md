# Vitals

Live metrics of the hosts you reach over SSH. Vitals signs in to a host, puts a small
read-only agent on it, and reads CPU, memory, network, disks, disk I/O, pressure, thermals,
GPUs, and processes from the agent's stream.

- `agent/`: the vitals-agent (Go, module `github.com/polterware/vitals/agent`). It prints
  NDJSON frames on stdout (protocol v1: `hello`, `metrics`, `kill_result`, `pong`, `warn`)
  and reads `kill` and `ping` commands on stdin. `agent/build.sh` cross-compiles it for
  Linux, Windows, and macOS (amd64 and arm64) into `agent/dist/` (not committed); set
  `VITALS_AGENT_OUT` to write elsewhere.
- `app/`: the desktop app in GPUI (`vitals-app`, binary `vitals`), built on
  [polterware/ui](https://github.com/polterware/ui).

## Run

```bash
bash agent/build.sh
cargo run -p vitals-app
```

The app looks for the agent binaries in `VITALS_AGENT_DIR`, then the app bundle's
`Resources/agent`, then `agent/dist`.

Saved hosts and approved host keys live in `~/Library/Application Support/Vitals`
(`servers.json`, `known_hosts.json`), the files the Electron app wrote, so both read the
same hosts. `VITALS_DATA_DIR` points the app at another folder. The Electron app sealed
saved passwords and passphrases with its own safe storage; this app cannot read them, so it
asks for them again and does not save them yet.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
(cd agent && go vet ./... && go test ./...)
```

`app/tests/live_session.rs` runs a full session (host key, sign-in, agent install, frames)
against a real SSH server when `VITALS_TEST_HOST`, `VITALS_TEST_PORT`, `VITALS_TEST_USER`,
and `VITALS_TEST_KEY` are set. `VITALS_SHOTS=<dir>` runs a tour that captures every page to
PNG and quits.

## Where it comes from

This app replaces the Electron app `apps/vitals-desktop` of the former Polterware monorepo. The Electron app's main process (`src/main`) and renderer (`src/renderer`) are the reference for what is left to port; the file names below are theirs.

## Ported

- The frame: the sidebar with vibrancy and the inset main surface, with Hosts, Connect a
  host, the open sessions, and Settings.
- The SSH session: host key check against `known_hosts.json` with the approval prompt,
  sign-in by SSH agent, private key, or password, the agent install over SFTP with its
  SHA-256 cache, the frame stream with backoff restarts, and reconnecting after a lost
  connection (3 seconds).
- Hosts: saved hosts and open sessions in one table with live CPU, memory, load, network,
  and uptime; connect, edit, close, and remove.
- A host's Metrics tab: CPU (total, history, cores), memory and swap, network, disks, disk
  I/O, pressure, thermals, GPUs, and the top processes.
- The connect dialog, with saving a host once it connects.

## Left to port

In the order worth doing:

1. Terminal tab: the interactive shell (`shell-session.ts`, `TerminalView.tsx`, xterm).
2. Settings: network unit, history window, kill confirmation and default signal, close
   confirmation, auto-reconnect and its delay, reduce transparency, known hosts, recents
   (`SettingsDialog.tsx`, `settings.ts`).
3. Stopping a process from the process table, with its confirmation (the session already
   sends `kill` and reports `kill_result`).
4. The script fallback for hosts the agent cannot run on (`ssh-session.ts`,
   `remote-script.ts`, `shared/parsers`).
5. Saved secrets in the macOS Keychain, so a password host connects without asking.
6. The dashboard's card layout: reorder, hide, and resize cards, per-card history charts,
   smoothing, and the process table's sorting (`MetricsDashboard.tsx`, `card-layout*.ts`,
   `card-reorder.ts`).
7. Recents on the Hosts page, and the close-session confirmation.
8. Packaging: the `.app` bundle with the agent binaries in `Resources/agent`, signing, and
   Sparkle updates from `updates/vitals.xml`.
9. Motion: the dialogs open without their spring.

## License

Apache-2.0; see [NOTICE](NOTICE).
