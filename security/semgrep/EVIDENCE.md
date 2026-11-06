# Semgrep adjudication evidence

Receiver baseline: Semgrep1.172.0, explicit registry pack `p/rust`, all60
resolved rules and57 executed rules,392targets including all334 tracked Rust
files,366 findings and zero scanner errors on combined Frame c0414b17. The raw
JSON digest, source SHA and capture metadata are pinned in `baseline.json`;
`findings.jsonl` is the current per-fingerprint verdict surface. This records
the frozen combined-source capture before inventory alignment. Release
acceptance additionally requires the canonical gate on the final derived commit.

The v2 fingerprint normalizes only one structurally recognized final generated
`Local_file` origin, including its temporary directory and basename. Every other
byte remains significant. Missing, multiple or malformed origins fail closed;
rule/pattern/severity/language/modifier changes must fail the hash check. Scanner
version, all60 resolved IDs, scanner errors and finding fingerprints remain
independent checks. The356 minimum and exact vendored-assets ignore are unchanged.

The current60-rule baseline is a deliberate reviewed migration. Its captured
YAML SHA256 is33de6cc9722bfda36c7c9ac9c55e8921b57664d70de7985eae76ade891eb49b0.
The old d576a630 hash was sensitive to temporary origin directories and its
original rule/dump preimage is unavailable; historical semantic equivalence is
not asserted. Registry rule bodies are not vendored or redistributed here.

The earlier archive scan under an ancestor Git/ignore root omitted12Rust test
files and additional Python/shell/JSON targets. Correct-root Git enumeration and
an explicitly rooted export select the same eligible universe. That archive
result does not justify lowering356. `make semgrep` is the canonical gate.

Inventory metadata is a bounded source adjudication, not exhaustive FFI or
product security certification. Exact newly reviewed operations have source
shape/cfg/lifetime falsifiers; new fingerprints remain fatal until separately
reviewed. Tests cover malformed origins, real rule-field drift, missing/duplicate
inventory, altered source predicates, out-of-scope unsafe calls and broad ignores.

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

The plugin and build-fixture findings are cfg(test). Test-only does not imply
private temporary storage: fixed-name synthetic fixtures remain hardening debt.
The xtask install helper atomically reserves its directory; E2E/layout helpers
retain exclusive TempDir ownership; build-target expectations perform no I/O.
The socket fixture uses exclusive mkdir and refuses existing paths, but does not
explicitly enforce0700.

The four production scrollback methods reach the named writer in
`os_input_output.rs`. Source checks require Unix creation mode0600 regardless of
umask, retaining explicit existing-export permissions and error behavior.
UUID naming alone is insufficient. A source-fixture rollback to0666 must fail.
The isolated named-dump test deliberately creates synthetic data in a child
with owned TempDir TMPDIR, fixed exact test name and controlled umask marker;
it is distinct from the read-only process_temp comparison below.
The Unix socket fixture test in `zellij-server/src/unit/os_input_output_tests.rs`
is loaded only through the exact `#[cfg(test)]` parent gate in
`zellij-server/src/os_input_output.rs`. Its `process_temp = std::env::temp_dir()`
binding compares the short `tempfile::tempdir_in("/tmp")` fixture against
`/tmp` or `/private/tmp` and a never-created inherited-TMPDIR legacy name;
it does not create, open, or bind any file through `process_temp`.

## Current executable

The reviewed client `current_exe` sites start another internal mode of the
already running binary without elevation. Separately, project preparation
compares the canonical current engine to a selected physical Runtime Pack
engine; this is generation membership validation, not cryptographic binary
authentication. Its two cfg(test) sites copy/link their test binary as fixtures.
The named-dump test re-enters one fixed test with an owned TempDir TMPDIR.
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

CLI `args_os` preserves platform arguments for typed clap validation; this is
input parsing, not authorization. Two non-clap uses are separately classified:
`process_has_server_flag` only suppresses a CLI-client log marker for --server
processes; `process_log_scope` only selects a single filename component for
the current-user temporary log directory; it falls back to the process ID and
does not authorize a server, select a socket, or execute an argument.

## Path traversal

The Actix taint findings are outside an Actix HTTP source-to-sink flow:
WASI preopens and watchers receive host-authorized paths; legacy migration uses
fixed current-user ProjectDirs; plugin loading is the explicit operator plugin
capability; protobuf hits only construct data; installer symlinks use a
validated framework root; webserver IPC and session lifecycle operations use
locally discovered, validated current-user sockets. The webserver query helper
currently ignores its timeout argument and uses blocking connect/read; bounded
availability is not claimed. The developer E2E staging helper copies expected
nonempty plugin artifacts from the selected build output into target/e2e-data;
this is not an arbitrary untrusted-name copy API.

## Vendored browser assets

`.semgrepignore` contains exactly `zellij-client/assets/`. The validator binds
that literal path to `ignore-policy.json`, rejects globs and every
unadjudicated ignore. This is third-party browser code, not a blanket production
exclusion.

## Reviewed additional boundaries

The readiness test calls listen(fd,1) only on a live owned UnixListener and
checks its return. The Unix rail allocator opens a regular0600 file with
O_NOFOLLOW/O_CLOEXEC, checks flock and retains ownership through the critical
section. The reaper uses0600 below a private log root, checks nonblocking flock
and moves its File into the thread for the loop lifetime; O_NOFOLLOW is not
claimed for the reaper.

The macOS foreground probe zeroes only the C POD proc_bsdinfo, passes its exact
size and live pointer to proc_pidinfo, and reads e_tpgid only after a matching
byte count. The Unix saturation test passes a bounded sockaddr, adopts one
checked OwnedFd, sets nonblocking flags and retains connected streams; it is
not the production poll/flag-restore path. The separate test-only getsockopt
helper supplies a live c_int buffer and sizeof length and checks syscall success.

Five new guest ABI wrappers serialize their exact typed command before the
host trampoline: RunAction, Show/HideFloatingPanes, SetPaneRegexHighlights and
ClearPaneHighlights. Host RunActionsAsUser or ChangeApplicationState permissions
remain separate authority. This does not waive the independently identified
WASI environment exposure; that repair belongs to a separate source cut.

The Windows marker probe checks a query-only process handle and exit status,
preserves errors as unknown and closes the handle before return. Merely opening
a handle is insufficient proof that a process remains alive. Moving the Windows probe out of sessions.rs changes ordinal fingerprint suffixes
for its remaining Unix unsafe matches. The old unmatched SG-0324 row described
the Linux no-replace syscall, which still exists under the preceding suffix;
its disappearance is not deletion of that operation. All three current Unix
operations and the separate Windows probe are re-bound to current source.

## WASI environment test reentry

The unsuppressed current_exe match in plugins/unit/wasi_environment_tests.rs is
loaded only through the unique cfg(test) parent in plugin_loader.rs. Its private
isolated helper re-enters the current test executable with fixed runner flags
and one test selection; current_exe establishes no production privilege or
executable trust. The owned TempDir supplies TMPDIR, HOME, cache/data and both
socket directories. Inherited environment is cleared and only the child marker
and synthetic sentinel values are added. The child must report one successful
test. A closed command-builder shape rejects later env inheritance, private-path
or argument overrides. Falsifiers also remove the cfg parent, private-directory
acquisition, marker and success assertion. No nosemgrep suppression is used.
