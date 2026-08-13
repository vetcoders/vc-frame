# Semgrep adjudication evidence

Receiver baseline: Semgrep 1.172.0, explicit registry pack `p/rust`, 60 resolved
rules, 57 rules executed over 363 targets, 332 blocking findings and zero scan
errors on the `b75e7bd2` source base. The exact raw JSON hash is pinned in `baseline.json`;
`findings.jsonl` is the checked-in machine-verifiable verdict surface.
The gate also hashes Semgrep's normalized resolved rule representation, so a
registry rule-body change fails even when rule IDs stay the same. Scanner
version, all 60 resolved IDs and result fingerprints are independently pinned.
`make semgrep` is the canonical executable gate for this contract.

The 2026-08-04 re-adjudication found one real product defect: the clinic's
atomic config writer used a predictable process-ID temporary path and
`File::create`, which could follow a pre-positioned symlink. The writer now
uses an exclusively created random `NamedTempFile` in the destination
directory before fsync and atomic persist. The current 332 reviewed
fingerprints include five newly adjudicated session-socket lifecycle sites;
the remaining broad audit hits are explicit review boundaries, test helpers,
or false source-to-sink paths.
Validator negative tests prove missing rows, empty owners, new fingerprints
and broad ignores fail.

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

## Session socket lifecycle

Session ownership uses one `O_CLOEXEC`/`O_NOFOLLOW` lock file and a lifetime
`flock`; the owned socket is removed only while its device and inode still
match. Rename acquires the destination lease first and uses the platform's
atomic no-replace syscall on Apple and Linux, with all pointer inputs held by
validated `CString` values and every return code checked. Process-level tests
cover two competing owners, exec descriptor closure, legacy listeners,
replacement-inode teardown, and late rename destinations.

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
unions are checked or wrapped before safe Rust code observes them. Process and
pseudoconsole handles stay under RAII guards until fallible monitor-thread
handoff succeeds; cleanup calls consume only the exact guarded resources.

## Unix platform FFI

Unix findings are narrow terminal/libc adapters. Descriptors, pointers and
return values are checked at the boundary and converted to owned safe types.
The new test-only boundary injects a deliberate `EMFILE` spawn failure to prove
the child and PTY descriptors are reaped without publishing a terminal owner.

## Temporary paths

The three plugin findings and the xtask atomic-install finding are under
`cfg(test)`. The xtask helper atomically reserves a process-unique directory;
the four production scrollback findings append a new UUID v4 to the system
temp directory and contain only the current user's terminal dump.

## Current executable

`current_exe` starts another internal mode of the already running vc-frame
binary. It establishes no identity, trust, privilege or update provenance.
The plugin host resolves the reserved `vc-frame:self` command token to that
same executable so a frame-host visitor cannot drift to an older binary on
`PATH`; the command remains behind the existing plugin `RunCommands`
permission boundary.
The clinic also resolves the current executable for a read-only mtime and
local process-name drift diagnosis; it neither executes nor authorizes it.
The triage transfer-lock tests additionally re-enter the same test executable
under fixed test names with only the selected isolated scenario or the lock
path and expected lock state. The macOS-only xtask installer test copies its
own real Mach-O test executable into a process-private fixture directory so it
can prove signing and strict verification; it does not execute that copy.

## CLI arguments

`args_os` preserves platform arguments for direct typed clap parsing and
command-specific validation. It is input parsing, not authorization.

## Path traversal

The Actix taint findings are outside an Actix HTTP source-to-sink flow:
WASI preopens and watchers receive host-authorized paths; legacy migration uses
fixed current-user ProjectDirs; plugin loading is the explicit operator plugin
capability; protobuf hits only construct data; installer symlinks use a
validated framework root; webserver IPC and session lifecycle operations use
locally discovered, validated current-user sockets.

## Vendored browser assets

`.semgrepignore` contains exactly `zellij-client/assets/`. The validator binds
that literal path to `ignore-policy.json`, rejects globs and every
unadjudicated ignore. This is third-party browser code, not a blanket production
exclusion.
