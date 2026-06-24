---
name: release
description: >
  Cut a new release: commit pending changes, bump the version in Cargo.toml,
  commit the bump, tag as vX.Y.Z, and push everything. Invoke on "release",
  "cut a release", "bump version", or "publish a new version".
---

# Release

Cut a new version in a predictable, repeatable way.

## Rules

- **Never force-push or rewrite history.** This is an additive flow.
- **Never commit secrets.** Check the diff before committing.
- **Verify the build is green** before tagging — a broken release is worse than
  a delayed one.
- **Version scheme:** semver `MAJOR.MINOR.PATCH`. If the user specifies a
  version, use it. Otherwise apply best judgment:
  - **PATCH** for bug fixes and small behavior-preserving changes.
  - **MINOR** for new features or non-breaking enhancements.
  - **MAJOR** for breaking API or behavior changes.
  - When unsure, default to PATCH.
- **Tag format:** `vX.Y.Z` (e.g. `v0.2.3`), matching existing tags.

## Procedure

1. **Check for uncommitted changes.**
   - `git status` + `git diff`.
   - If there are changes: `cargo fmt && cargo clippy && cargo build` to verify
     green, then stage and commit with a concise message matching the repo
     style (check `git log --oneline -5`).
   - If the working tree is clean, skip to step 2.

2. **Bump the version.**
   - Read the current version from `Cargo.toml` (`version = "X.Y.Z"`).
   - Determine the next version (see rules above).
   - Update `Cargo.toml` with the new version.
   - Run `cargo build` to regenerate `Cargo.lock` with the new version.

3. **Commit the version bump.**
   - Stage `Cargo.toml` **and** `Cargo.lock` and commit as `bump version to
     X.Y.Z` (matching the existing commit style — check `git log --oneline`).

4. **Tag.**
   - `git tag vX.Y.Z` (annotated tag is fine if the repo uses them; check
     `git tag` for existing style).

5. **Push everything.**
   - `git push && git push --tags` (or `git push origin vX.Y.Z` if the default
     push doesn't include tags).

6. **Report** the new version, tag, and what was committed.
