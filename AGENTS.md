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

- `agent/` is the vitals-agent (Go, module `github.com/polterware/vitals/agent`): read-only, NDJSON protocol v1 on stdout (`hello`, `metrics`, `kill_result`, `pong`, `warn`), `kill` and `ping` on stdin. A protocol change keeps old agents readable or bumps the version.
- `app/` is the GPUI app (`vitals-app`, binary `vitals`) on `polterware-ui` (git dependency on [polterware/ui](https://github.com/polterware/ui); a sibling checkout replaces it through a `.cargo/config.toml` `[patch]`). Use no third-party GPUI kit, and a control never defers its own panel.
- SSH is russh on its own tokio runtime (`app/src/session.rs`), with events reaching GPUI through a channel. Host keys are checked against `known_hosts.json` and a new or changed key asks the person first; never skip that check.
- Saved hosts live in `servers.json` in the data folder (`VITALS_DATA_DIR`, else `~/Library/Application Support/Vitals`). Never write a secret there in plain text.
- What is left to port, in order, is in README.md; delete an item when it ships.

## Verification

Run `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`, and `(cd agent && go vet ./... && go test ./...)` after relevant changes. Maintain risk-proportional tests.
