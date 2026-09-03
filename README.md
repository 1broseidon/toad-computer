# toad.computer

A small Linux desktop for a coding agent. One container is one machine: a
display, a browser, a shell, a home directory, and one MCP server that lets
an agent see the screen, act on it, and check what happened. Toad starts one
per teammate; anything else that speaks MCP can point at it too.

The image is a contract, not a binary. Anything that serves these eight tools
over streamable HTTP at `/mcp`, with `/health` open, is a valid computer.

## What is in the box

```
toad-computer  PID 1, supervisor, window manager, dock, MCP server
├── Xvfb       the X server; pixels in RAM, no GPU
├── dbus       the session bus the accessibility tree rides on
└── chromium   the visible browser, driven over its DevTools protocol
```

The image is a vanilla Alpine, a few distro packages, an X server built in
the Dockerfile, and one binary. Nothing in it runs as root, so the container
runs with every capability dropped. It weighs 555 MB, and Chromium is 264 MB
of that.

| what | why |
| --- | --- |
| Xvfb, built from the xorg-server release with GLX off | Alpine's Xvfb links libGL, and libGL drags in 300 MB of Mesa a framebuffer in RAM never calls |
| `dbus`, `at-spi2-core` | the accessibility tree `capture` and `wait` read |
| `chromium` | the browser |
| `ttf-dejavu`, `font-noto-emoji` | text and emoji render |
| `libxfont2`, `libxcvt`, `libxau`, `libmd`, `pixman`, `xkbcomp`, `xkeyboard-config` | what Xvfb links and the keymap it compiles |

Nothing else: the agent sends input through XTEST and owns the CLIPBOARD
selection itself, so there is no `xdotool`, `xclip`, or `wmctrl` to shell
out to.

## The viewer

The machine serves its own screen. `/` is a page with one canvas; `/ws` is
the socket behind it. Frames go down as binary messages, six little-endian
`u16` (x, y, width, height, screen width, screen height) followed by a PNG of
that rectangle; X DAMAGE decides which rectangles, at most twenty a second,
and an idle desktop sends nothing. A viewer that joins or falls behind gets
the whole screen next. Input comes up as JSON and reaches the X server through
XTEST:

```json
{"t":"move","x":640,"y":400}
{"t":"button","b":1,"down":true}
{"t":"wheel","dy":1}
{"t":"key","key":"Enter","down":true}
```

The socket takes the bearer as a `token` query, because a browser cannot send
a header on a WebSocket; Toad opens `http://127.0.0.1:<port>/#<token>` and the
page reads the fragment, which never leaves the browser. A person's input
holds the machine as `person` for ten seconds at a time, so a teammate's
mutating tools are refused while someone is driving and the desktop hands
itself back when they stop.

## The tools

- `capture` returns a scaled PNG and the AT-SPI tree, or writes an original PNG.
- `input` clicks, moves, drags, scrolls, types, presses keys, and uses the clipboard.
- `browser` drives the visible Chromium over CDP; element refs last for one text snapshot. No action runs longer than a minute, and a browser the person closed is replaced by the next call rather than waited on.
- `shell` runs bounded commands or launches a detached desktop application.
- `files` gets, puts, and lists paths confined below the computer home.
- `windows` lists, focuses, closes, maximizes, and tiles windows.
- `wait` polls the accessibility tree and browser page text for a phrase.
- `state` owns control leases, browser logins, and home-directory snapshots.

`/health` and the viewer page never require authentication. When
`TOAD_COMPUTER_TOKEN` is set, every method on `/mcp` requires
`Authorization: Bearer <token>`, the viewer's socket requires the same token
as its `token` query, and otherwise both return a JSON 401. `X-Computer-Holder` names the teammate using a lease or
run slot; an absent header means `anonymous`.

## The desktop

The agent is the window manager. A normal window opens maximized into the
work area above the dock; a dialog opens centred at its own size; a click
focuses the window under it. The wallpaper is black with the Toad mark in
grey, and the dock at the bottom holds the browser. `_NET_CLIENT_LIST`,
`_NET_ACTIVE_WINDOW`, and `_NET_WM_STATE` are kept current because the
`windows` tool reads them, and its `focus`, `close`, and `tile` actions are
`_NET_ACTIVE_WINDOW`, `_NET_CLOSE_WINDOW`, and configure requests the same
thread handles.

## Boot

`toad-computer boot` is the entrypoint. As PID 1 it forks: the parent reaps
every child the kernel hands it and forwards SIGTERM; the child starts Xvfb
and dbus-daemon, becomes the window manager, and serves. A machine whose
display or bus has died exits, and the container with it.

`toad-computer serve` serves on a display that already exists, for running
the agent outside the container.

| variable | default | |
| --- | --- | --- |
| `TOAD_COMPUTER_ADDR` | `0.0.0.0:8787` | where `/mcp`, `/health`, and the viewer listen |
| `TOAD_COMPUTER_TOKEN` | unset | bearer for `/mcp`; unset means open |
| `TOAD_COMPUTER_HOME` | `/home/agent` | the directory `files` is confined to |
| `TOAD_COMPUTER_SCREEN` | `1920x1080` | the Xvfb screen `boot` creates |
| `DISPLAY` | `:0` | the display `boot` creates and `serve` uses |

## Build and run

```sh
make image                # docker build -t toad-computer:next .
make run                  # a hardened container on 127.0.0.1:8787, token in .token
make contract             # the contract test against it, through a real MCP client
make check                # fmt, clippy -D warnings, unit tests
```

`make run` is the create command Toad uses, spelled out:

```sh
docker run -d --name toad-computer-next \
  --cap-drop=ALL --security-opt no-new-privileges \
  --pids-limit 512 --memory 2g --shm-size 1g \
  -p 127.0.0.1:8787:8787 \
  -e TOAD_COMPUTER_TOKEN="$(cat .token)" \
  toad-computer:next
```

Mount a workspace with `-v "$PWD:/home/agent/workspace"`. Chromium needs the
sized `/dev/shm`.

## Layout

```
src/boot.rs      PID 1, Xvfb, dbus, then the agent
src/desktop.rs   wallpaper, dock, window manager
src/serve.rs     the HTTP door: /health, bearer auth, /mcp, the viewer routes
src/viewer.rs    the viewer page and its socket
src/viewer.html  the page: one canvas, pointer and keys
src/screen.rs    DAMAGE-driven PNG rectangles for the viewer
src/xtest.rs     the person's pointer and keys, injected with XTEST
src/tools/       the eight tools
src/browser.rs   the managed Chromium over CDP
src/x11.rs       screenshots and EWMH window queries
src/a11y.rs      the AT-SPI tree as text
src/lease.rs     who holds the machine
tests/contract.rs  the opt-in proof against a running container
```
