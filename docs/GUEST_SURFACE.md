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
rendered visitor bytes and matching observed guest metadata; a request number
alone is never active-tab truth. Metadata convergence can delay the receipt.

To return, detach the new visual client through its own UI and attach the old
session with its original native binary/config/layout. Select the desired tab
by clicking that client's tab strip or using its local tab shortcuts, and
verify its body before typing. A transient `vc-frame action go-to-tab` command
targets the last active client and is insufficient when a retained visitor is
also connected. Keep the old session until the returned client is verified.

Older layouts retain their inner compact bar, rail and status bar inside the
new host's guest pane. The new client cannot remove chrome owned by the old
server. Retaining it is preferable to losing live work. Background title/link
helpers must remain suppressed; projection does not need floating helper panes.
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
