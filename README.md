# Vitals

Live metrics of the hosts you reach over SSH. Vitals signs in to a host, puts a small
read-only agent on it, and reads CPU, memory, network, disks, disk I/O, pressure, thermals,
GPUs, and processes from the agent's stream. Where the agent cannot be installed or run, a
shell script over the same connection reads a Linux host's `/proc` instead.

Everything is Rust:

- `agent/`: the vitals-agent (`vitals-agent`, a workspace member). It prints NDJSON frames on
  stdout (protocol v1: `hello`, `metrics`, `kill_result`, `pong`, `warn`) and reads `kill`
  and `ping` commands on stdin, as the Go agent it replaces did, frame for frame.
  `agent/build.sh` builds it for Linux (static musl), Windows, and macOS, amd64 and arm64,
  into `agent/dist/` (not committed) with the size-tuned `agent` profile; Linux and Windows
  go through `cargo-zigbuild` (`brew install zig` and `cargo install cargo-zigbuild`). Set
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

`bash app/bundle.sh` builds `target/bundle/Vitals.app`: the release binary, the agents in
`Resources/agent`, the Info.plist, and the icon compiled from `app/resources/vitals.icon`
(Icon Composer, into `Assets.car` and `vitals.icns`), signed ad hoc for this Mac. At
runtime, the app applies the chosen icon variant (Default, Ice, or Fire, rendered on the
macOS icon grid as `app/resources/vitals-icon.png`, `vitals-ice-icon.png`, and
`vitals-fire-icon.png` from their `.icon` bundles); Default uses the compiled `vitals.icon`
in a bundle and falls back to `vitals-icon.png` when run unbundled via `cargo run`.

Saved hosts and approved host keys live in `~/Library/Application Support/Vitals`
(`servers.json`, `known_hosts.json`), the files the Electron app wrote, so both read the
same hosts. `VITALS_DATA_DIR` points the app at another folder. A saved host's password or
key passphrase goes to the macOS Keychain, never to `servers.json`: a generic password per
server (its account the server's id) under the service `com.polterware.vitals`, or
`com.polterware.vitals (<folder>)` for a `VITALS_DATA_DIR`. The Electron app sealed its
secrets with its own safe storage instead; this app cannot read those, so a host saved there
asks once more.

## Checks

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
bash agent/build.sh
```

`cargo test` covers the agent too. `app/tests/live_session.rs` runs a full session (host
key, sign-in, agent install, frames, and the Terminal tab's shell) against a real SSH server
when `VITALS_TEST_HOST`, `VITALS_TEST_PORT`, `VITALS_TEST_USER`, and `VITALS_TEST_KEY` are
set. With `VITALS_TEST_FALLBACK=1` too, and only against a local docker sshd (a loopback
host), it also forces the script fallback: it turns the host's `~/.cache/vitals` into a file
so the agent upload fails, checks the script's samples and kills, and puts the folder back.
Unit tests keep saved secrets in memory and their files in a scratch folder, never the
Keychain or the real data folder; `cargo test -p vitals-app keychain_round_trip -- --ignored`
writes, reads, and deletes a throwaway item in the login Keychain.
`VITALS_SHOTS=<dir>` runs a tour that captures every page to PNG and quits;
`VITALS_HOLD=<seconds>` connects the saved hosts that need no password, holds them that
long, and quits, for measuring a connected app.

## Where it comes from

This app replaces the Electron app `apps/vitals-desktop` of the former Polterware monorepo. The Electron app's main process (`src/main`) and renderer (`src/renderer`) are the reference for what is left to port; the file names below are theirs.

## Ported

- The frame as the latest Electron app draws it: the title bar's strip of tabs (one per open
  session, with its close), the "+" that goes back to the start page, and Settings at its
  end, over the inset main surface.
- The start page (`HostHome`): Connect a host, Recents, and Saved, each row with its menu
  (Edit, Remove).
- The SSH session: host key check against `known_hosts.json` with the approval prompt,
  sign-in by SSH agent, private key, or password, the agent install over SFTP with its
  SHA-256 cache, the frame stream with backoff restarts, and reconnecting after a lost
  connection, after the delay Settings sets.
- The script fallback (`ssh-session.ts`, `remote-script.ts`, `shared/parsers`): where the
  agent cannot be installed (an unsupported platform, no writable folder, a failed upload)
  or never says hello (a `noexec` mount), the session says "Agent unavailable; using script
  fallback (…)" and runs the sampling script on the same connection for the rest of the
  session, reconnects included. Its samples become the agent's snapshots (no disk I/O or
  pressure), the Metrics page shows the "Script fallback" chip, kills run as `kill`, a
  stopped script starts again after 2s ("Metrics script stopped"), and a host without
  `/proc` says "Script fallback needs a Linux host with /proc".
- A host's Metrics tab: CPU (smoothed total, cores, history), memory and swap, network
  (interfaces, receive and send history), disk I/O, pressure, disks, thermals and GPUs, and
  the top processes, sortable, with their menu (copy the PID or command, kill); the numbers
  in the Electron app's formats. Cards move by dragging their header or grip, or with the
  arrow keys on a focused grip (`card-reorder.ts`): the held card lifts and follows the
  pointer, the others glide out of its way after a short dwell, the page scrolls near its
  edges, a drop on a hole keeps that cell, and Escape puts everything back. The card layout
  menu hides and shows cards, packs them with no holes (Auto-layout), and resets them; the
  layout (hidden cards, order, and dropped cells) is saved to `card-layout.json`. Before the
  first metrics, the page shows the `EchoRing` while it connects.
- A host's Terminal tab: an interactive shell (`xterm-256color` PTY) on the session's own
  SSH connection, opened the first time the tab shows and kept with its scrollback while the
  session is open; it follows the tab's size, opens again after a reconnect (`[reconnected]`),
  and says when the shell ends. The terminal is polterware-ui's `TerminalView`.
- Stopping a process, with its confirmation and default signal, and the close-session
  confirmation.
- Settings, section for section, with its search: General (reduce transparency, app icon,
  network unit), Monitoring (history window, kill confirmation and signal), Connections (close
  confirmation, auto-reconnect and its delay, known hosts), and Advanced (reset the card
  layout, clear the recent hosts). They save to `settings.json`.
- The connect dialog (`ConnectionDialog`), with Save server. It stays open while the host
  connects: the fields lock, Connect reads "Connecting…", the session's progress shows under
  the fields, Cancel ends the attempt and keeps the dialog, and a failure shows in it until
  the next edit. The attempt has no tab until it connects; then the dialog closes and the
  tab takes the page. An unknown host key asks over it (`HostKeyModal`).
- The dialogs' motion: each opens on the slow spring (its frame from 97%, fading in with
  its scrim) and leaves on the quicker tween.
- Saved secrets in the macOS Keychain (`storage.ts`, there sealed with `safeStorage`): a
  saved host's password or passphrase is kept once it signs in, a saved host connects with
  it and no prompt, and a missing or denied Keychain item opens the connect dialog instead.
  Removing a host forgets its secret, and the secrets of hosts the data folder no longer
  lists (it was cleared) are forgotten at the next start.
- Packaging: `app/bundle.sh` (ad hoc signing).

## Left to port

In the order worth doing:

1. Packaging for release: Developer ID signing, notarization, and Sparkle updates from
   `updates/vitals.xml`.

## License

Apache-2.0; see [NOTICE](NOTICE).
