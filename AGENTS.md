# Repository Guidelines

## Project Structure & Module Organization

KunCode is a Rust workspace defined by the root `Cargo.toml`; all crates live under `crates/*`. Implemented areas include shared types in `crates/kuncode-core`, path safety in `crates/kuncode-workspace`, event logging in `crates/kuncode-events`, tool implementations in `crates/kuncode-tools`, and the CLI binary in `crates/kuncode-cli`. `kuncode-context`, `kuncode-policy`, `kuncode-provider`, and `kuncode-runtime` currently mark planned boundaries and may contain only minimal scaffolding.

Keep implementation code in each crate's `src/` directory. Put integration tests in `crates/<crate>/tests/`, following existing examples such as `crates/kuncode-tools/tests/read_file.rs` and `crates/kuncode-events/tests/event_log.rs`. Architecture notes and plans belong in `docs/specs/` and `docs/plans/`.

## Build, Test, and Development Commands

- `cargo build --workspace`: compile every crate in the workspace.
- `cargo test --workspace`: run all unit and integration tests.
- `cargo test -p kuncode-tools`: run tests for one crate while iterating.
- `cargo run -p kuncode-cli -- --version`: run the CLI binary locally.
- `cargo fmt --all -- --check`: verify Rust formatting.
- `cargo clippy --workspace --all-targets -- -D warnings`: treat lint warnings as failures.
- `cargo deny check`: audit licenses, advisories, bans, and dependency policy from `deny.toml`.

## Coding Style & Naming Conventions

Use Rust 2024 on the stable toolchain specified by `rust-toolchain.toml`. Formatting is controlled by `rustfmt.toml`: max width is 120 and small heuristics are set to `Max`. Prefer idiomatic Rust naming: `snake_case` for modules, functions, and files; `PascalCase` for types and traits; `SCREAMING_SNAKE_CASE` for constants.

Workspace Clippy enables `pedantic` warnings, with selected exceptions in `Cargo.toml` and `clippy.toml`. Add shared dependencies through `[workspace.dependencies]`.

## Testing Guidelines

Use Rust's built-in test framework with async tests where needed. Name integration test files after the behavior or tool under test, for example `search.rs`, `apply_patch.rs`, or `path_safety.rs`. Prefer focused tests for public crate behavior and filesystem edge cases with `tempfile`. Run `cargo test --workspace` before opening a PR.

## Commit & Pull Request Guidelines

Recent history uses short, phase-scoped commit subjects such as `phase1: fix shutdown event envelope loss` or `Phase2: Tool Runtime implementation`. Keep commits concise and focused. For PRs, include behavior changes, tests run, linked issues or plan items, and security or compatibility implications. CLI changes should include example output when useful.

## Security & Configuration Tips

Respect path-safety boundaries in `kuncode-workspace` and tool permission logic in `kuncode-tools`. Do not bypass `deny.toml` policy for licenses, yanked crates, or wildcard dependencies without documenting the rationale.
