# Semgrep adjudication evidence

Receiver baseline: Semgrep 1.164.0, explicit registry pack `p/rust`, 60 resolved
rules, 57 rules executed over 373 targets, 318 blocking findings and zero scan
errors at `9f8a67b0`. Raw JSON is preserved beside the W0-B delivery report;
`findings.jsonl` is the checked-in machine-verifiable verdict surface.
The gate also hashes Semgrep's normalized resolved rule representation, so a
registry rule-body change fails even when rule IDs stay the same. Scanner
version, all 60 resolved IDs and result fingerprints are independently pinned.
`make semgrep` is the canonical executable gate for this contract.

No confirmed product defect was found. The broad audit rules found explicit
review boundaries, test helpers, or false source-to-sink paths. No fake product
fix was invented. Validator negative tests prove missing rows, empty owners,
new fingerprints and broad ignores fail.

## Plugin API FFI

`zellij-tile/src/shim.rs` is the wasm guest ABI shim. Its unsafe calls enter one
host-command trampoline after serialization. Ownership: Plugin API FFI.

## Vendored termwiz

`zellij-utils/src/vendored/termwiz/input.rs` contains upstream compatibility
code for Windows event unions and a validated UTF-8 fast path. It remains in
the inventory rather than being hidden by the browser-assets ignore.

## IPC libc

`zellij-utils/src/consts.rs` implements bounded Unix-socket probing. `OwnedFd`
owns the descriptor, sockaddr length is checked, `poll` bounds connect time,
`getsockopt` checks completion, and original flags are restored.

## Process probes

Unix daemonization and local PID liveness probes are explicit lifecycle
boundaries. They operate only at startup or on locally discovered session PIDs.

## Process environment

Environment mutation is limited to initialization, synchronous host-command
handling, or test cleanup. It is not a concurrent shared-state API.

## Transfer lock descriptor

The triage child inherits one already-held transfer-lock descriptor from its
parent. Before adopting it as `File`, the child proves the descriptor is open,
sets `FD_CLOEXEC`, canonicalizes the expected lock path, matches device and
inode, and verifies that the inherited open-file description still owns the
non-blocking flock. The `from_raw_fd` call then creates exactly one Rust owner
in the child process; the parent owns a separate descriptor-table entry.

## Test-only unsafe

Unsafe environment changes in Rust tests restore prior values and are not
compiled into production paths.

## Windows platform FFI

Windows findings are narrow Win32/ConPTY adapters. Raw handles and tagged
unions are checked or wrapped before safe Rust code observes them.

## Unix platform FFI

Unix findings are narrow terminal/libc adapters. Descriptors, pointers and
return values are checked at the boundary and converted to owned safe types.

## Temporary paths

The three plugin findings are under `cfg(test)`. The four production scrollback
findings append a new UUID v4 to the system temp directory and contain only the
current user's terminal dump; no fixed authority filename is used.

## Current executable

`current_exe` starts another internal mode of the already running vc-frame
binary. It establishes no identity, trust, privilege or update provenance.
The triage transfer-lock tests additionally re-enter the same test executable
under fixed test names with only the selected isolated scenario or the lock
path and expected lock state.

## CLI arguments

`args_os` preserves platform arguments for direct typed clap parsing and
command-specific validation. It is input parsing, not authorization.

## Path traversal

The nine Actix taint findings are outside an Actix HTTP source-to-sink flow:
WASI preopens and watchers receive host-authorized paths; legacy migration uses
fixed current-user ProjectDirs; plugin loading is the explicit operator plugin
capability; protobuf hits only construct data; installer symlinks use a
validated framework root; webserver IPC uses locally discovered sockets.

## Vendored browser assets

`.semgrepignore` contains exactly `zellij-client/assets/`. The validator binds
that literal path to `ignore-policy.json`, rejects globs and every
unadjudicated ignore. This is third-party browser code, not a blanket production
exclusion.
