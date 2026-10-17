# Container-based Git hook checks

The normal git-guard and Lefthook chain runs Rust formatting, strict workspace
Clippy, isolated workspace tests and the security audit in disposable Linux
containers. It does not reuse a previous receipt as a pass, run native Cargo as
a fallback, or change global hooks. This satisfies deployment-host isolation.

Before committing, provision a reviewed local Linux Docker image and an explicit
machine-local JSON configuration. Set `AGENT_BUS_HOOK_QA_CONFIG` to its absolute
path. Missing configuration, Docker, tools or dependencies fails visibly.
Podman and remote Docker endpoints are not admitted by this initial adapter.

Configuration fields:

| Field | Required value |
| --- | --- |
| `image` | Exact existing local image ID, `sha256:` plus 64 hexadecimal digits |
| `endpoint` | Explicit local Unix socket or Windows named pipe for the Linux engine |
| `tools_dir` | Reviewed directory with `bin/rustfmt`, `bin/cargo-fmt`, `bin/cargo-clippy`, `bin/clippy-driver`, `bin/cargo-audit` as needed by the gates |
| `tool_files_sha256` | Exact relative-file-to-SHA256 inventory of that entire tool directory |
| `cargo_home` | Owned dependency cache without credentials or host Cargo configuration |
| `target_dir` | Owned compilation output directory |
| `evidence_root` | Owned directory for receipts, source snapshots and the active-run lease |
| `jobs` | One through four Cargo jobs |
| `timeout_seconds` | Thirty through 3,600 seconds per container |

Use distinct, non-overlapping absolute directories. Source is mounted read-only;
only the owned cache/output and private run directory are writable. The image
compiler/Cargo must match `rust-toolchain.toml`. Formatter and Clippy binaries
must belong to the same compiler cohort, with compatible driver libraries.
Check the actual tools inside the pinned image before using it. For example, an
audit binary requiring newer GLIBC cannot run in an older QA image; a verified
official portable release is one option. Do not silently install components or
substitute another compiler during a hook.

Pre-commit snapshots the exact index tree. Pre-push receives all Git ref records
through one `use_stdin` coordinator and tests each distinct nondeleted outgoing
commit. It refuses missing input and unsupported objects rather than assuming
`HEAD`. The executing adapter must match the snapshotted source. Git archive
omissions, substitutions, symlinks and unsafe paths fail admission.

Formatting/lint/tests have no container network. Security auditing runs in a
separate container with a fresh advisory checkout and private Cargo home; it
cannot modify compilation caches. Every run clears inherited bus configuration,
credentials and proxy settings. The adapter verifies the actual container
resources, mounts and privileges before execution and retains inspection,
command, source-hash, advisory-commit and lifecycle receipts.

Concurrent use of the same configured evidence/cache lane refuses via an
exclusive lease. On failure, inspect the first error and cleanup receipts.
Only the exact captured container may be stopped and removed, after process
settlement. A cleanup failure retains the lease for reconciliation; never remove
it merely because it is old or start another writer against the same cache.

The maintained `scripts/ci/check-lefthook-gates.sh` fixture checks command
execution, exact multi-ref stdin transfer and failure propagation. Its marker
and controller tests establish hook behavior, not a successful Rust test run;
actual gate receipts remain necessary. Normal review, exact-head CI and merge
protection still apply after local hooks pass.

The host precreates the finite command logs and actor receipt. The container
opens those existing files in place; it never replaces them or repairs their
permissions. This preserves host ownership across Docker Desktop's filesystem
boundary. Completion requires unchanged file identities and host readback of
every handoff file, as well as the actor's actual gate results.

Rustdoc executes merged documentation-test binaries in TMPDIR. The owned 256 MiB
UID-1000 temporary filesystem therefore explicitly permits execution while
retaining nosuid, nodev and mode 0700. Docker's implicit noexec default otherwise
makes rustdoc return failure after printing successful standalone test results.
Source remains read-only, and compilation caches never gain network access.
