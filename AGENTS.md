# Agent guide

Instructions for any AI coding agent working in this repository.

## Skills

Reusable, on-demand procedures live in [`agents/skills/`](agents/skills/). Read
and follow the relevant one when its trigger matches the request.

- [`clean-code`](agents/skills/clean-code.md) — behavior-preserving cleanup pass:
  remove duplication, improve structure and naming, simplify, and strip
  unnecessary comments. Trigger: "clean up", "tidy", "refactor for quality",
  "deduplicate", "remove comments".
- [`release`](agents/skills/release.md) — cut a new release: commit pending
  changes, bump version in Cargo.toml, tag as vX.Y.Z, push everything. Trigger:
  "release", "cut a release", "bump version", "publish a new version".

## House style

- Comment sparingly — only where a comment explains something the code cannot
  (a _why_, a non-obvious constraint). Delete comments that restate the code.
- Match the existing style of the file you're editing.
- Verify changes with `cargo build` and `cargo clippy`, and run `cargo fmt`before
  declaring done.
