# VC Frame host and guest surface

VC Frame has two runtime roles:

- `vibecrafted-host` is the disposable visual shell. It owns one compact bar,
  one session rail, one status bar, and one replaceable `VC Guest` pane.
- `vibecrafted-guest` is a long-lived, content-only session. It owns terminal
  processes and tabs but loads no product chrome.

The interactive bridge is `vc-frame visit SESSION [--tab N]`. It is a normal
read/write client, unlike `watch`, and can run inside the host's guest pane.
Closing or replacing that visitor disconnects only the client; the guest
server and its PTYs continue running.

## Manual vertical slice

Create a detached guest:

```sh
vc-frame --layout vibecrafted-guest attach --create-background my-work
```

Start the shared frame:

```sh
vc-frame --new-session-with-layout vibecrafted-host --session vc-frame-host
```

In the host, selecting `my-work` in the left rail replaces only the center
pane. The rail, top bar, and bottom bar stay owned by `vc-frame-host`.

## Empty-host overview

While no guest is projected, the `VC Guest` pane renders a live overview
instead of a placeholder: workspaces with organ chips and liveness, the
`vc.live-runs.v1` census (unknown `?`, degraded `~`), and quick actions.
Enter or a click on a workspace projects it through the unchanged frame-host
routing; `n` creates an auto-named guest workspace and projects it on
success. Selecting in the rail keeps working exactly as before.

## Replace the visual host while keeping old work alive

The supported product entry is `vc-start --new-host --repo /absolute/repo`.
It creates a disposable host beside the old sessions. After the Operator has
published a verified native binary and matching plugin bundle, open that host,
select Workspace, and select an existing session in its Sessions rail. A tab
click or Cmd Left/Right projects the selected tab through the same owner.
For a named host, the corresponding primitive is:

```sh
vc-frame --session NEW_HOST project-workspace OLD_SESSION --tab 1
```

Tab numbers are one-based. This retains the OLD server, terminal PIDs, PTYs,
provider processes and conversations. It replaces a visual client, not the
server. Do not kill, resurrect, re-exec or recreate the old session for this
operation. Native server upgrades remain a separate operation.

Detach the old visual client through its own UI, or close its terminal window,
before using the bridge. The old guest's metadata reports activity across all
its clients, so this bridge acknowledges selection only when one guest client
and one active tab are observed. Multiple guest viewers cannot supply
unambiguous visitor selection on that old protocol. `Handled` requires both
rendered visitor bytes, matching observed guest metadata, and the corresponding
compact-bar publication applied for that exact host client. Screen flushes those
body/chrome bytes before the original request's receipt. A request number alone
is never active-tab truth. Metadata or plugin processing can delay the receipt.
A nonempty snapshot without exactly one active tab cannot replace the bar's last
observed selection or disable its navigation while leaving the old chip visible.
An empty gone-guest tombstone still clears the guest projection.

To return, detach the new visual client through its own UI and attach the old
session with its original native binary/config/layout. Select the desired tab
by clicking that client's tab strip or using its local tab shortcuts, and
verify its body before typing. A transient `vc-frame action go-to-tab` command
targets the last active client and is insufficient when a retained visitor is
also connected. Keep the old session until the returned client is verified.

Older layouts retain their inner compact bar, rail and status bar inside the
new host's guest pane. The new client cannot remove chrome owned by the old
server. Retaining it is preferable to losing live work. Background title/link
helpers should remain suppressed; projection does not need floating helper panes.
If the OLD session already has visible technical helpers, the new visitor retains
that OLD floating layer too. It cannot individually suppress those panes through
the OLD CLI. The existing OLD `action hide-floating-panes --tab-id TAB_ID` hides a
whole tab's floating layer without closing its processes. Use it only after a
complete pane inventory establishes that the layer contains exclusively the
owned link/title helpers. A layer containing floating work requires separate
handling; do not blindly hide it. This is a client-owned OLD layout operation,
not a new-host repair or a server upgrade.
The entry command also requires the sibling Vibecrafted runtime to carry the
verified candidate; a source commit alone does not update installed hosts.

The private regression uses the actual OLD native and an OLD ten-tab layout:

```sh
uv run --with pyte python zellij-server/tests/workspace_chrome_pty.py \
  --binary /absolute/new/vc-frame --legacy-binary /absolute/old/vc-frame \
  --rail-config /absolute/config.kdl --bridge-rounds 2 \
  --output /absolute/new-evidence-directory
```

It checks exact-task ordinary input, outer selection, PID/PPID/start identity,
new-host detach/reattach, client-local return across all ten tabs, and suppressed
technical surfaces. All generated sockets, configs and processes are private.

For an older product host containing work directly, also pass
`--legacy-product-layout /absolute/OLD/layouts/host.kdl` and
`--legacy-config /absolute/OLD/config.kdl`. This preserves its session-layer bars,
Workspace owner and overview, adds thirteen task identities across thirteen
cards, starts two visible technical helpers and a separate ordinary guest, and
checks rail discovery, leaving and returning, 26 physical tab transitions, host
reattachment and client-local return. Helper identities are retained after the
verified technical-only floating layer is hidden; no pane is closed for migration.
The product trial alternates rail mouse bytes and Super Right. It records body
and outer chip at the original Screen commit and at exact-task ordinary input,
before any later request. It never issues a corrective `project-workspace`
command between product transitions; later convergence cannot pass a failed
original boundary. The stock fixture separately exercises explicit CLI receipts.

On host reattachment, queued publications from a detached client must be refused
without revoking the new client's publisher lease. Only the leased client can
invalidate that lease when configured owner/client uniqueness is lost. Keeping
old plugin instances for other lifecycle work does not give them authority over
the attached client's chrome.

The host marker alone must not be inferred from a session name or pane title.
The OLD cross-session metadata format omits `SessionInfo.plugins`, even though
its local product host owns a frame-host plugin. A filter evaluated on a fabricated
external plugin map is not proof that an actual OLD host disappears from the rail.
Keep runtime evidence separate from classification hypotheses.
