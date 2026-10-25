# AGENTS.md

## Tests

Smoke tests first. Every user-visible change needs a UI smoke test in `tests/ui.rs`.
A smoke test drives the production `Root` view in a headless window through
`gpui_kit::test::TestWindowExt`: `press` the real shortcut, `click` the real
control, `input` real text, then assert the outcome the user would see (view
state through a public accessor, `window.rem_size()`, the config file, player
state). Setting state by hand, calling `dispatch_action`, or invoking a handler
directly bypasses what broke last time and does not count. A test that cannot
fail without the fix is not a test; check it fails before the fix lands.

Unit tests (`#[test]` in a `#[cfg(test)] mod tests` block in the same file) are
only for pure logic a smoke test cannot reach cheaply: parsing, math,
serialization, SQL.

Rules for smoke tests:

- Set up through `omamusic::init_actions`, which installs the real key map and
  the app-wide action handlers. Never bind a hand-made subset.
- Cover both focus states for a shortcut: nothing focused, and the search field
  focused (`press("ctrl-p")` then `input`).
- Call `AppConfig::use_dir` with a fresh temp dir first (see `isolated_config_dir`).
  Tests must never read or write `~/.config/omamusic`.
- Use `Player::new_test()` and `Library::empty()`. No audio device, desktop
  session or network. Mark anything that needs real hardware
  `#[ignore = "reason"]`.
- `find`/`click` need an element id. Kit components (`button`, `icon_button`,
  `input`) register theirs automatically. A plain `div().id(..)` must add
  `.test_support()` to be reachable.
- Snapshots are from the last frame. After an external change, call
  `window.render_frame(cx)` before querying. Use `cx.run_until_parked()` after
  each interaction.

Pixel tests live in `tests/rendering.rs`. They draw the production view with
GPUI's headless WGPU renderer (needs a Vulkan adapter; Mesa lavapipe works
without a GPU) and check the captured image: a control's bounds contain ink,
a frame changes after an action, the same view renders identically twice.
Add one when a change affects what is painted rather than what is laid out
(colors, icons, text size, hover and selection highlights). Assert relative
facts (before vs after, inside vs outside bounds), never absolute colors, since
the theme follows the machine. The file has no test harness; add a test to the
list in `run()`.

Run:

```sh
cargo test                       # everything
cargo test --test ui             # smoke suite only
cargo test --test ui NAME -- --nocapture
cargo test --test rendering      # pixel tests; prints "skipped" without a renderer
RENDERING_REQUIRED=1 cargo test --test rendering   # fail instead of skip (CI does this)
RENDERING_DUMP_DIR=/tmp/shots cargo test --test rendering   # save every capture as PNG
```

Linux is the only supported target. CI (`.github/workflows/ci.yml`) runs on Ubuntu with Mesa lavapipe for the pixel tests, on every push, pull request and release.

Manual QA against a throwaway config: `OMAMUSIC_CONFIG_DIR=/tmp/omamusic-qa cargo run`.

## Before committing

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass locally; CI enforces the same checks.
