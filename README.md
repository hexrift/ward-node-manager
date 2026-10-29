# Ward Node Manager

Ward Node Manager runs one bounded, disposable Docker container for a task. It
is a small Rust CLI with no third-party crate dependencies.

## Requirements

Rust 1.88 or later, Cargo, and Docker with a running Linux container engine.
The configured Alpine image must already be present locally; task containers
never pull images or access the network.

## Test and build

```sh
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo build --release --offline
```

Run a task by passing its argument vector after `--`:

```sh
target/release/wardnm run --run-id demo --task-id task-1 -- /bin/busybox printf 'hello\n'
```

The CLI emits a bounded JSON result. Nonzero task exits and timeouts return a
nonzero CLI exit status.

## Container boundary

The manager pins the image by digest, never pulls during task execution, passes
no host mounts or environment variables into a task, disables networking,
runs as an unprivileged UID with all capabilities dropped and
`no-new-privileges`, uses a read-only root filesystem plus a small ephemeral
`/tmp`, and applies CPU, memory, process, wall-time, and output limits. Each
container is named and labelled from validated run/task identifiers and is
removed on completion. Task arguments are passed as an argv array, never through
a host shell.

This is a constrained **container** boundary, not a separate kernel per Task.
On macOS and Windows, Docker Desktop runs Linux containers inside its shared
Linux VM. On Linux, containers share the host kernel. The Docker daemon and its
host remain trusted; this prototype is not yet appropriate for hostile
multi-tenant workloads or as a production security claim. Higher-risk use needs
an additional VM/microVM or equivalent kernel boundary and a reviewed daemon
deployment model.

Task output is treated as untrusted, potentially sensitive data. The manager
bounds its size but does not redact arbitrary content; an integrating system
must decide what output may become evidence or operator-visible.

## Workspace snapshot contract

The Rust library defines a versioned `WNM1` snapshot codec for regular-file
contents. It accepts at most 32,768 files, 255 UTF-8 path bytes per file, 64 MiB
per file, and 512 MiB total, enough for a checkout of most open-source
repositories without their build output. Paths must be relative, slash-separated, normalized,
unique, and cannot represent both a file and one of its descendants. Symlinks,
permissions, and other filesystem metadata are not represented. The library
can pack a directory into WNM1, rejecting symlinks and special files while
bounding traversal to 262,144 directory entries. It can also convert snapshots to
deterministic USTAR archives containing regular files only.
`wardnm` holds the snapshot, its decoded files and the archive in memory while
it starts the task, so packing or running a snapshot near the limit needs
about three times its size in host memory. The archive must also fit the
`/workspace` tmpfs: a snapshot larger than the workspace fails when the task
unpacks it, and the run is reported as failed.

`wardnm snapshot pack DIRECTORY OUTPUT` creates a WNM1 snapshot and refuses to
overwrite an existing output file.

`wardnm run --snapshot FILE -- COMMAND ...` reads at most 512 MiB of WNM1 data,
validates and converts it before starting Docker, then streams the archive over
Docker stdin into a private 32 MiB `/workspace` tmpfs. The task runs with
`/workspace` as its working directory. No host path is mounted into the task;
the tmpfs is bounded by the container memory limit and removed with the
container. When a snapshot is supplied, stdin is reserved for workspace
transfer and the task receives EOF rather than independent input.

## Docker acceptance

With Docker running and the pinned task image already present, run the opt-in
pack/transfer/execute/cleanup acceptance test:

```sh
cargo test --offline --test docker_acceptance -- --ignored
```
