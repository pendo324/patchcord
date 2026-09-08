# Design: preventing Discord's `discord_capture` autoconnect via LD_PRELOAD shim

## Problem (live-verified, corrected from an earlier wrong theory)

Confirmed via a real, live A/B test during screensharing: Discord's native
"Stream With Audio" feature routes audio to viewers through per-app
PipeWire capture nodes named `discord_capture` (`media.name = "game
capture"`, `application.process.binary = "Discord"`). Disconnecting
Spotify's PipeWire link to its `discord_capture` node caused Spotify's
audio to audibly cut out for a live viewer immediately; reconnecting it
restored it. This is the real, live audio path "Stream With Audio" uses --
not a side effect, not an artifact of an unrelated feature.

(An earlier version of this doc incorrectly concluded these nodes were
Discord's separate "Clips" background-recording feature, based on
matching C++ symbol names like `SetClipRecordUser`/`saveClipEx` found via
`strings`, plus observing these nodes already existed before any
screenshare was started. That inference was wrong: it doesn't rule out
`discord_capture` doing double duty for both Clips *and* live "Stream
With Audio" -- and the live disconnect/reconnect test above proves the
"Stream With Audio" case directly. Corrected here rather than left
standing.)

**The actual bug**: Discord creates a separate, individually-targeted
`discord_capture` node **per audio-producing app it detects on the
system**, and links each one to that specific app's PipeWire output --
confirmed live: with no user selection involved at all, one node was
linked to Spotify's `object.serial` and a second, simultaneously-existing
node was linked to Firefox's `object.serial`, each via an explicit
`target.object` property (not blanket PipeWire/WirePlumber autoconnect
policy -- `target.object` is set to the *exact* `object.serial` of one
specific app node each time, individually, confirmed by cross-referencing
values). Since **both** links exist and are both live at the same time,
anyone watching a "Stream With Audio" share hears **every** currently
audio-producing app simultaneously (Spotify **and** Firefox **and**
anything else running), not just whichever single app the user actually
wants to share -- there is no existing Discord UI/mechanism to pick just
one.

This is the actual problem PatchcordAppAudio needs to solve: let the user
choose which app(s)' `discord_capture` links exist, instead of Discord
unconditionally creating and linking one for every detected app.

## Root cause detail (how `discord_capture` nodes get their links)

- `discord_voice.node` does not link `libpipewire-0.3.so.0` at build time
  (confirmed via `ldd`: not in the needed-library list). It `dlopen()`s it
  at runtime (`dlopen@plt` call sites present, `libpipewire-0.3.so.0`
  string present) and resolves every individual `pw_*` function via
  `dlsym()` -- confirmed via 88 distinct `dlsym@plt` call sites and a full
  set of `pw_stream_new`/`pw_stream_connect`/`pw_properties_set`/etc.
  symbol-name strings present in the binary. This is the standard
  `pipewire-rs`/`libpipewire-sys` runtime-loader pattern.
- Because of this, a conventional `LD_PRELOAD` symbol override (relying on
  the dynamic linker's normal PLT/GOT resolution order) does **not**
  intercept these calls -- Discord never asks the dynamic linker to
  resolve `pw_stream_new` by name at load time; it asks `dlsym()` for it
  explicitly, at a time and via a path our `LD_PRELOAD` library can itself
  intercept instead.
- The actual per-app targeting is set via the `target.object` stream
  property at `pw_stream_new`/`pw_stream_connect` time (live-verified:
  each `discord_capture` node's `target.object` exactly matches one
  specific app's `object.serial`), plus `node.autoconnect = true` as an
  apparent fallback/reinforcement. Both need to be addressed by any fix
  that wants to control which app(s) actually get linked.

## Solution: two independent pieces

### 1. `discord-capture-shim` (new, small C shared library)

An `LD_PRELOAD`-loaded shim whose only job is to prevent Discord's own
code from choosing `discord_capture`'s target app, by stripping/rewriting
the properties that set it before the stream is ever created. It has
**no knowledge of patchcord, no IPC, and makes no routing decisions of
its own** -- it purely disables Discord's per-app auto-targeting so
something else (patchcord) can decide instead.

Mechanism:
- Overrides `dlsym(handle, name)` (the actual interception point, per the
  root cause above). For every call, first check if `name` is one of a
  small set of symbols we care about (`pw_stream_new`; add
  `pw_stream_connect` too if testing shows `target.object`/autoconnect
  bits are also set via that call rather than only in `props`). If so,
  resolve the *real* symbol via the real `dlsym` once (cached), wrap it,
  and return our wrapper's address instead. For every other symbol name,
  call straight through to the real `dlsym` unmodified -- this must be
  correct and low-risk for every other symbol in the entire process
  (libEGL, libGL, PipeWire's own other symbols, etc.), since a shim bug
  here can break far more than just audio capture.
- `pw_stream_new` wrapper: inspect the incoming `struct pw_properties *`
  (or the `name` argument) for `discord_capture`/`game capture` identity
  (matching the same property values confirmed live: `node.name`,
  `media.name`, `application.process.binary`). If matched, call
  `pw_properties_set(props, PW_KEY_NODE_AUTOCONNECT, NULL)` and
  `pw_properties_set(props, PW_KEY_TARGET_OBJECT, NULL)` (clears both
  keys) before forwarding to the real `pw_stream_new`. For every other
  stream name, forward completely unmodified.
- Result: `discord_capture` still gets created exactly as before for
  every detected app (so Discord's own detection/creation logic, port
  counts, format negotiation, etc. are all completely untouched -- we are
  not intercepting the actual audio path or capture logic at all), but
  each instance appears in the PipeWire graph with no target/no
  autoconnect, instead of being auto-linked to its detected app
  immediately.

### 2. Patchcord: new graph-watch rule (extends existing, working code)

Patchcord already has: (a) continuous graph-change monitoring (the
existing `graphChanged` event used to react to the virtual sink's own
state), and (b) working node-to-node link-management primitives (already
used for `routeNodes`/`ensureVirtualSink` today).

Add: recognize `discord_capture` nodes the same way the shim identifies
them (same property match), and -- instead of linking to a virtual
sink/mic as an intermediate step -- link the user's currently-selected
node(s)' outputs *directly* into each `discord_capture` instance's input
ports, using the same link-creation primitive `routeNodes` already uses.
Only link the app(s) the user actually selected in the modal; leave every
other detected app's `discord_capture` instance unlinked (or link it to
nothing, silence by omission -- no need to actively mute or destroy the
node, since an unlinked `Stream/Input/Audio` node has no output to
capture).

Re-apply whenever the user's selection changes (mirrors the existing
`routeNodes` call already triggered by the modal), and re-apply on any
relevant graph change (mirrors the existing `graphChanged` reaction) to
handle a new `discord_capture` instance appearing for a newly-launched
app, or an existing one needing relinking after its target app restarts.

No virtual sink or virtual mic is needed for this path at all -- this is
actually simpler and more direct than today's mic-swap approach (which,
per this session's earlier finding, also never actually applied its
resolved `deviceId` to anything, and even if it had, would have been the
wrong mechanism regardless, since `discord_capture` doesn't go through
mic-input-device selection at all).

## LD_PRELOAD injection: confirmed feasible, see companion findings

Full findings on how to reliably get `LD_PRELOAD` applied to Discord's
real process (surviving Discord's own auto-updater and app relaunches)
are documented separately in this session's chat history: set it in
`/usr/bin/discord` (the OS-packaged shell wrapper Discord's own
auto-updater never touches, confirmed via updater log inspection),
before its final `exec "$discord_host" "$@"` -- environment variables
survive plain `exec()`. `app.relaunch()` (used elsewhere in Equicord's
own code, and by Discord's own self-update flow) inherits the current
process's environment by default per Electron's actual
`relauncher_linux.cc` source (`base::LaunchProcess` with no explicit
`environ` override), so the shim stays active across relaunches with no
additional plugin-side code needed.
