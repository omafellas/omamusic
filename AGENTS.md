# AGENTS.md

## Iterating

- Do not run the full suite (`cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`) while a task is in progress. Run them once, at the end, before finishing.
- During iteration, keep feedback fast: `cargo check` or a single targeted test (`cargo test <name>`), and prefer the edit tool's exact-match feedback over recompiling for every small change.

## Tests

- Every change to behavior must come with tests. New modules and new logic need unit tests in a `#[cfg(test)] mod tests` block in the same file; bug fixes need a regression test that fails without the fix.
- Tests must not depend on a desktop session, audio device or network. Use the simulated output (`Player::new_test`) for playback; mark anything that needs real hardware with `#[ignore = "reason"]`.
- Tests must pass on Linux; CI (`.github/workflows/ci.yml`) runs them on every push, pull request and release.

## Before committing

Run the full suite once, at the end of the task (see [Iterating](#iterating)):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass locally; CI enforces the same checks.
