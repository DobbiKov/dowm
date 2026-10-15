# dowm

macOS tiling WM in Rust. See `PLAN.md`. This repo currently contains the **M0 spike**:
corner parking + CADisplayLink animation + AX write-throughput benchmark.

## Accessibility permission (required)

Both spike binaries drive other apps through the Accessibility API. Grant permission to the
terminal app you launch them from: System Settings > Privacy & Security > Accessibility,
enable Terminal / iTerm / WezTerm, then restart that terminal. Without it `dowm-spike` prints
a message and exits with status 2.

## Required macOS setting: one Space across displays

dowm does not use native macOS Spaces; it emulates workspaces by parking windows. For that
to survive unplugging a display, turn **off** "Displays have separate Spaces":

System Settings > Desktop & Dock > Mission Control > *Displays have separate Spaces*, then
**log out and back in**. (Same as `defaults write com.apple.spaces spans-displays -bool true`
followed by `killall SystemUIServer` and a logout.)

With it on, unplugging a display makes macOS move that display's Space onto the remaining
one and leave your other windows behind in a different Space. dowm cannot move windows
between Spaces (macOS blocks the private calls for ordinary processes), so it only detects
the split: windows in a Space that is not showing are marked `~` in `dowm list-windows`,
left alone, and raised only when you ask for them (`workspace N`, `focus`), at which point
macOS animates the switch to their Space. The daemon logs a warning at startup if the
setting is on. To merge a stray Space by hand, delete the extra desktop in Mission Control.

## Control socket and CLI

The daemon listens on a Unix socket (`$TMPDIR/dowm-<uid>/dowm.sock`, directory 0700, socket
0600, peers checked by uid; override with `--socket` or `DOWM_SOCKET`). The same `dowm`
binary is the client:

    dowm list-workspaces | list-windows | list-monitors [--json]
    dowm mode
    dowm subscribe [workspace|focus|mode ...]     # JSON lines until interrupted
    dowm workspace 3                              # any config command works the same
    dowm layout tiles horizontal

`exec-and-forget` is only accepted from the config file, never over the socket. Requests
are length-framed JSON (4-byte big-endian length, 64 KiB limit); the wire types are in
`crates/dowm-protocol` and the client library in `crates/dowm-client`.

## Build and test

    cargo build --release
    cargo test
    cargo clippy --all-targets -- -D warnings

## M0: park demo (moves a real window of the app you name)

    ./target/release/dowm-spike park --app "TextEdit"          # name (case-insensitive) or pid
    ./target/release/dowm-spike park --app 1234 --duration-ms 300 --pause-ms 1000

Takes the app's first standard window, parks it just past the bottom-left corner of its
monitor's visible rect (AeroSpace-style: `visibleRect.bottomLeftCorner + (1,-1) - (w,0)`;
bottom-right if the left corner would land on a neighbouring display), pauses, then animates
it back with ease-in-out-circ driven by CADisplayLink, writing `AXPosition` each tick. Every
tick interval is printed and saved to `bench/park_frames.csv`.

## M0: benchmark (reproducible, uses a helper app with plain windows)

    ./target/release/dowm-spike bench --spawn-target --counts 10,20,40 --duration 10

Spawns `dowm-spike-target` (40 NSWindows, separate process so AX writes are cross-process),
animates N windows at once for each count, and writes `bench/frames_N10.csv`,
`bench/frames_N20.csv`, `bench/frames_N40.csv` and `M0_REPORT.md`, which states the max
window count holding 60fps and the criterion used. Options: `--motion park|wiggle`,
`--half-period <secs>`, `--out-dir`, `--report`. To benchmark an existing app instead:
`--app <name|pid>` (it must have at least N standard windows; it will be moved around).

Run the helper by itself with `./target/release/dowm-spike-target --windows 40` (it exits
when its stdin closes).

## 60fps criterion

Frame budget = max(display refresh period, 1/60 s). A count holds 60fps when all of:

- the worse of the tick-to-tick interval p95 and the completion-to-completion interval p95
  is <= 1.25 x budget (20.83 ms at 60 Hz);
- dropped display ticks <= 5% (per interval `round(interval / budget) - 1`, over processed
  intervals plus dropped);
- failed AX writes <= 1%.

"Late frames" (interval > 1.5 x budget) and tick-to-completion lateness are reported but
do not gate the verdict. The max count is the largest N where N and all smaller tested
counts hold. AX writes/s is the rate sustained at display rate, not a peak.

Run `bench --spawn-target` first: it uses throwaway windows. `park` and `bench --app` move
real windows; `park` takes the app's first standard window (AXWindows order).

## Crash safety

Every touched window's original position is registered before the first move. SIGINT,
SIGTERM, SIGHUP, panics and normal exit move windows back and re-enable
`AXEnhancedUserInterface`. If something goes wrong anyway (e.g. `kill -9`), windows sit
~1px inside a monitor corner; quit and reopen the app or use its Window menu to recover.

## M4: config, hotkeys, binding modes

`dowm` reads an AeroSpace-compatible TOML config (an existing `aerospace.toml` loads
unchanged) and hot-reloads it: edit the file, or run `reload-config`, and bindings, modes,
gaps, `accordion-padding`, the normalization flags, default root layout/orientation,
callbacks, `on-window-detected` rules, force-assignments and `focus-follows-mouse` apply
without a restart. A config that fails to parse or validate is rejected with every error
and its key path logged, and the running config stays as it was.

**Config path.** `dowm --config <path>`, else `~/.config/dowm/dowm.toml`, else
`~/.dowm.toml`, else a small built-in default (`src/config/default.toml`). It does not read
AeroSpace's own file locations.

**Where dowm differs from AeroSpace**

- `focus-follows-mouse = true|false` (new, default `false`): focus the window under the
  pointer once it rests for ~90 ms. Windows hidden with their workspace, minimized or
  already focused are ignored; toggling it needs no restart.
- `workspace-auto-back-and-forth = true|false` (new): `workspace N` on the focused
  workspace goes back to the previous one.
- **Workspace pool.** The pool is fixed at startup and derived from the config: `1`-`9`,
  plus every other workspace name the config mentions (in `workspace` /
  `move-node-to-workspace`, an `on-window-detected` rule's `if.workspace`,
  `workspace-to-monitor-force-assignment` keys, `persistent-workspaces`), listed after
  `1`-`9`: numbers ascending, then names alphabetically. So rules that send windows to
  workspace `T` or `B` work with no extra setup. Changing the pool in a reload needs a
  restart: dowm logs "requires restart", keeps the running pool and applies everything else.
- `after-startup-command` runs once, ~1.5 s after launch. `after-login-command` and
  `start-at-login` are validated but have no effect (dowm installs no login item).
- `key-mapping.preset`: `qwerty` is the identity; `dvorak` and `colemak` re-map key names
  to the physical key that types that character on that layout.
- `workspace next|prev` is not supported; `resize smart` takes a signed change only.
  `layout` accepts `tiling` (as in `layout floating tiling`).
- `if.app-name-regex-substring` and `if.window-title-regex-substring` are case-insensitive.
- Unknown keys are errors, except AeroSpace keys dowm ignores with a warning:
  `config-version`, `automatically-unhide-macos-hidden-apps`, `exec-on-workspace-change`
  and the `[exec]` table.
- `close-all-windows-but-current` asks each other window of the focused workspace to close
  with Command-W; `exec-and-forget` runs through `/bin/bash -c`, detached.
