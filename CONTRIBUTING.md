# Contributing

Keep changes focused and use small pull requests. Link each pull request to the
issue it advances. For behavior changes, add a targeted test first, confirm it
fails for the intended reason, then implement the smallest passing change.

## Local checks

Use stable Rust and run the same offline checks as CI:

```sh
cargo fmt --all -- --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```

Do not add dependencies without explaining why the standard library or an
existing dependency is insufficient. Security-sensitive changes should state
their threat boundary and limitations in the pull request.

## Changes to the container boundary

Do not add host mounts, task environment passthrough, network access, mutable
images, or broader privileges as convenience features. Such changes require a
separate design review and tests that demonstrate the resulting boundary.
