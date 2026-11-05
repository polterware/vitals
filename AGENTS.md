# Vitals Agent Instructions

## Communication and editing

- Speak to Erick in Brazilian Portuguese.
- Write code, UI strings, technical documentation, commits, logs, and identifiers in English. No Portuguese text anywhere in the repository: code, comments, UI strings, error and log messages, tests, fixtures, docs, and skills.
- Preserve unrelated work and make the smallest safe change.
- Do not create branches or commits unless explicitly requested.
- Do not run browsers, previews, or development servers unless the current task asks for them. Automated tests and builds may run by default.
- Never connect to a host the task does not name. The live session test runs only against the server its `VITALS_TEST_*` variables name.

## What this is

Live metrics of the hosts you reach over SSH (see README.md).

- `agent/` is the vitals-agent (Rust, crate `vitals-agent`, a workspace member): read-only, NDJSON protocol v1 on stdout (`hello`, `metrics`, `kill_result`, `pong`, `warn`), `kill` and `ping` on stdin, byte-compatible with the Go agent it replaced (whole numbers print without a fraction). It runs on every host Vitals reaches, so it stays small (the `agent` profile) and needs nothing on the host. A protocol change keeps old agents readable or bumps the version.
- `app/` is the GPUI app (`vitals-app`, binary `vitals`) on `polterware-ui` (git dependency on [polterware/ui](https://github.com/polterware/ui); a sibling checkout replaces it through a `.cargo/config.toml` `[patch]`). Interface work follows polterware-ui's `AGENTS.md` (Building interface): build the control in polterware-ui as a general component, with the old Solid app's look on our tokens and materials and gpui-kit's behavior, and keep here only Vitals' data, words, and effects. A control never defers its own panel.
- SSH is russh on its own tokio runtime (`app/src/session.rs`), with events reaching GPUI through a channel. Host keys are checked against `known_hosts.json` and a new or changed key asks the person first; never skip that check.
- Saved hosts live in `servers.json` in the data folder (`VITALS_DATA_DIR`, else `~/Library/Application Support/Vitals`). Never write a secret there in plain text. Saved passwords and passphrases go to the macOS Keychain (`app/src/secrets.rs`, one item per server id); unit tests use its memory store, never the real Keychain.
- What is left to port, in order, is in README.md; delete an item when it ships.

## Verification

Run `cargo fmt --check`, `cargo clippy --all-targets`, and `cargo test` (the agent's tests included) after relevant changes, and `bash agent/build.sh` after agent changes. Maintain risk-proportional tests.
