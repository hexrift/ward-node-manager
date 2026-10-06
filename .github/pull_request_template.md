## Issue

<!-- Link the issue this PR advances. -->

## Purpose

<!-- Describe the single behavior or invariant changed. -->

## TDD evidence

- [ ] I added or changed a targeted test first and observed it fail.
- [ ] I implemented the minimum change needed to pass.
- [ ] Tests check behavior or invariants rather than implementation details.

### RED

<!-- State the failing test and observed failure. -->

### GREEN

<!-- State the passing test and implementation. -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --offline`
- [ ] `cargo clippy --offline --all-targets -- -D warnings`
- [ ] Platform-specific or Docker acceptance checks, if applicable.

## Security and trust

<!-- Describe effects on isolation, host access, credentials, network, and output. -->

## Scope

- [ ] This PR has one clear purpose.
- [ ] Follow-up work is split into separate issues or PRs.
- [ ] Production and test code avoid unnecessary explanatory comments.

## Reviewer focus

<!-- Identify the most important invariant to challenge. -->
