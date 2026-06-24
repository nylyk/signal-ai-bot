---
name: clean-code
description: >
  Behavior-preserving cleanup pass: remove duplication, improve structure and
  naming, simplify, and strip unnecessary comments. Invoke on "clean up", "tidy",
  "refactor for quality", "deduplicate", or "remove comments". Tool-agnostic.
---

# Clean-code pass

Make existing code cleaner **without changing what it does**. A quality pass, not
a bug hunt or feature change.

**Scope:** default to the current diff; otherwise the file/dir named. Only sweep
the entire codebase if specifically prompted — keep changes small and reviewable.

## Rules

- **Preserve behavior** — no change to output, public API, or control-flow
  semantics. If a cleanup might change behavior, flag it instead of applying it.
- **Improve weak style, don't enshrine it** — poor idioms, naming, or error
  handling are fair game to refactor toward a clearly better pattern; don't keep
  something just because it's already there. Move toward the codebase's prevailing
  conventions, not a personal one-off, and don't add dependencies or churn broadly
  for a small win.
- **Verify** with `cargo build`, `cargo clippy`, `cargo fmt` (+ `cargo test` if
  tests exist) before declaring done. Report the real result; fix or revert fails.
- Every change must be explainable in one line as "same behavior, clearly better".

## What to look for

- **Duplication** — extract repeated blocks into a helper, literals into named
  constants, parallel branches into a parameter. Search first; the helper may exist.
- **Structure** — split long/multi-purpose functions; flatten nesting with guard
  clauses; colocate data with the logic that owns it; delete dead code and unused
  params/fields/imports.
- **Clarity** — rename vague identifiers; prefer stdlib/existing abstractions over
  hand-rolled; simplify redundant conditionals and awkward control flow.
- **Efficiency** — drop obviously wasteful work (needless clones, repeated lookups)
  only when it doesn't cost readability. Don't micro-optimize.

## Comments

Default to fewer. **Remove:** restatements of the code, commented-out code, stale
comments, doc boilerplate, banners/noise. **Keep only** the _why_ code can't show
— a workaround, ordering constraint, subtle edge case, or external bug/spec ref.
Prefer a better name or smaller function over a comment.

## Procedure

1. Read the target plus its immediate callers/callees.
2. Apply cleanups in small, coherent edits, skipping anything risky to behavior.
3. Verify (build/lint/fmt/test); fix or revert until green.
4. Summarize what changed and why, the verification result, and anything left alone.
