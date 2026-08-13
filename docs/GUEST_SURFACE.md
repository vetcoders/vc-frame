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

Existing sessions remain attachable, but sessions created with an older
layout still render their own chrome inside the guest pane. Recreate them with
`vibecrafted-guest` when their work can be safely migrated; never kill a live
session merely to make the visual transition look clean.
