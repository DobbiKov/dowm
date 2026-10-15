# dowm — a macOS tiling WM: Rift's engine, AeroSpace's workspace model

## Context

You want a tiling WM that combines two things that today exist only separately:

- **AeroSpace's workspace model** — a fixed set of numbered workspaces that ignore native
  macOS Spaces entirely, each assignable to any monitor and movable between them.
- **Rift's engine** — smooth animation and the performance architecture behind it.

You already maintain a fork of Rift (`github.com/DobbiKov/rift`, upstream `acsandmann/rift`)
whose `feat/global-workspaces` branch attacks the same problem, but its `global` mode still
anchors every workspace to one native macOS Space per display — exactly the limitation you
want gone. You've chosen to build `dowm` fresh rather than keep patching the fork.

**Decided approach:** dowm is a new project, but not a legal clean-room one — we read both
codebases, and Rift is Apache-2.0 while AeroSpace is MIT, so porting with attribution is
permitted. The split you chose:

- **Port from Rift** (with an Apache-2.0 `NOTICE`): the FFI layer, the per-app actor and
  the presenter — the hard-won engine, verified in Finding D to carry no Spaces coupling.
- **Write fresh**: the workspace model, layout, config, commands and IPC — the actual new
  idea stays entirely yours.

**Why the merge is architecturally coherent** — the thesis the plan rests on:
AeroSpace's model is extremely AX-write-heavy. Every workspace switch rewrites the frame of
every window entering *and* leaving the screen, and AeroSpace executes that as a serial,
global, blocking reconciliation on the `@MainActor` — `layoutWorkspaces()` re-layouts all
visible workspaces and re-hides every invisible one on each refresh
(`layout/refresh.swift:155-202`). Rift's architecture — per-app threads for blocking AX IPC,
latest-frame-wins coalescing, a vsync-paced presenter — is precisely the machine that makes
that workload cheap. Together they produce what neither has: **animated workspace switching
with no native Spaces and no disabling SIP.**

Calibration: Rift is ~104k lines of Rust, AeroSpace's core ~11k lines of Swift. Most of
Rift's bulk exists *because* of native Spaces (churn/quarantine state machine, space-switch
gesture synthesis, volatile-space-id persistence reconciliation). Dropping native Spaces
deletes that entire category, so dowm should land far closer to AeroSpace's size.

---

## Finding A — how Rift achieves smooth animation and performance

Five composable mechanisms (paths relative to `utilities/rift`):

1. **Vsync-paced presenter thread.** A main-thread `CADisplayLink` per display
   (`src/sys/display_link.rs:108`) publishes only the *latest* tick into a mutex and wakes a
   dedicated `"rift-presenter"` thread over a `bounded(1)` crossbeam channel. Stale ticks are
   never replayed; the link is paused when a display has no motion
   (`src/actor/reactor/animation.rs:798`), so idle displays cost zero. Software ~60fps
   fallback after three missed refreshes.
2. **Closed-form motion math.** Discrete moves tween on an ease-in-out-circ curve
   (`animation.rs:1497`); the scrolling camera is a critically damped spring solved
   analytically — no per-frame integration (`src/layout_engine/systems/scrolling.rs:157`).
3. **Three concurrency domains.** reactor thread (layout decisions) → presenter thread
   (timing + interpolation) → **one OS thread per application** (`src/actor/app.rs:596`), the
   only place blocking AX IPC happens. A hung app stalls its own thread only.
4. **Coalescing at every hop.** Latest-frame-wins per window with a single re-armed wake per
   app (`app.rs:491`); writes skipped when target == current; `TransactionId`/`WindowTxStore`
   fence stale AX notifications; **size is written once at the 50% mark**, position every
   frame — apps re-layout expensively on resize, cheaply on move.
5. **AX used well, not bypassed.** Geometry goes through public `AXPosition`/`AXSize`
   (`src/sys/axuielement.rs:322`) with grow-before-shrink write ordering (`app.rs:650`) and
   `AXEnhancedUserInterface` disabled during bursts (`src/sys/enhanced_ui.rs:14`). Private
   SkyLight/CGS is used for *reads* and event subscriptions only. **There is no secret
   non-AX fast path to copy** — the speed is architecture, not API choice.

Event sources: per-app AX observers; NSWorkspace/notification center; raw CGS connection
notifications via `SLSRegisterConnectionNotifyProc` (`src/sys/window_notify.rs`); CGEventTap
for input.

## Finding B — Rift's architecture, and how deep native Spaces run

Modules: `sys/` (macOS FFI), `model/` (`WindowStore`, `VirtualWorkspace`, slotmap tree),
`layout_engine/` (`LayoutEngine` + 6 pluggable `LayoutSystem`s: bsp, traditional, stack,
master_stack, scrolling, floating), `actor/` (reactor hub, spaces, per-app, input, config),
`ui/`, `ipc/` (Mach), plus crates `rift-protocol` (OS-agnostic wire types), `rift-client`,
`rift-config-derive`.

**Native Spaces are the substrate, not a module.** `SpaceId` (a raw CGS space id,
`src/sys/screen.rs:28`) threads through nearly every signature in `model/` and
`layout_engine/`. `VirtualWorkspace` has a hard `pub space: SpaceId` field
(`src/model/virtual_workspace.rs:51`). Rift *requires* "Displays have separate Spaces" and
exits at startup without it (`src/bin/rift.rs:150`), and synthesizes native space-switch
swipe gestures from crafted IOHID events (`src/sys/space_switch.rs`, 555 lines).

**Transfers to dowm largely intact:** layout systems + `model/tree.rs`; `WindowStore`;
`app_rules`; AX discovery in `actor/app.rs`; config system + derive macros; protocol shapes
minus `space_id`; hotkey parsing/dispatch. **Does not transfer:** `actor/spaces.rs`,
`sys/space_switch.rs`, `model/space_activation.rs`, the `SpaceId`-keyed persistence schema
and its fuzzy reconciliation layer, space-keyed IPC queries.

**Test seam to copy:** tests construct a `Reactor` directly and feed synthetic events
(`src/actor/reactor/testing.rs`), asserting on `LayoutEngine`/`WindowStore` — no AX or CGS
needed. `global_workspaces.rs:538-1151` is an extensive scenario suite for display
connect/disconnect and workspace routing: the best behavioural reference available.

## Finding C — AeroSpace's workspace model

**The keystone: corner parking.** A workspace is hidden by moving each window just past a
corner of its own monitor, offset by the window's own size — *not* to huge negative
coordinates (`Sources/AppBundle/tree/MacWindow.swift:121-154`):

```swift
case .bottomLeftCorner:
    guard let s = try await getAxSize(.cancellable) else { fallthrough }
    let onePixelOffset = macApp.appId == .zoom ? .zero : CGPoint(x: 1, y: -1)
    p = nodeMonitor.visibleRect.bottomLeftCorner + onePixelOffset + CGPoint(x: -s.width, y: 0)
```

Before hiding, the position is saved *proportionally* to the monitor rect, so it restores
correctly even if the monitor resized. Invariant: exactly one workspace visible per monitor,
each workspace visible on at most one monitor — enforced in `setActiveWorkspace`
(`Workspace.swift:144-162`), which evicts the previous occupant on both sides of the
bidirectional map. There is no OS-level concept of a workspace anywhere.

**Corner choice is monitor-aware** (`refresh.swift:151-188`): for each monitor it probes
points just outside both bottom corners and picks whichever is least likely to land inside a
neighbouring display, weighting the diagonal probe 10×. Then — to reduce flicker — it
**unhides visible workspaces first, hides invisible ones second**.

**Documented caveat that shapes the design:** macOS will not let a window sit fully
off-screen, so a ~1px sliver of every hidden window remains visible in the corner
(`docs/guide.adoc:427-455`). Users are told to arrange monitors so each has a free bottom
corner. Stacking many windows in one corner also glitches Mission Control, with a documented
workaround (`defaults write com.apple.dock expose-group-apps -bool true`).

**Crash safety net — essential, easy to forget.** `AppServerTerminationHandler.beforeTermination()`
(`util/appBundleUtil.swift:26-52`, hooked to POSIX `signal()`) force-centers every window on
quit or crash, so no window is ever stranded off-screen. Any WM using corner parking *must*
have this or a bad exit leaves the user's windows unreachable.

**Workspace→monitor resolution is a priority chain** (`Workspace.swift:108-113`):
`forceAssignedMonitor` → the monitor it is currently visible on → `assignedMonitorPoint`
(last monitor it lived on) → main monitor. `move-workspace-to-monitor` plugs the hole it
leaves by assigning a stub workspace to the source monitor, so a monitor is never without
an active workspace.

**Near-zero private API:** AeroSpace declares exactly one private symbol,
`_AXUIElementGetWindow` (`Sources/PrivateApi/include/private.h`), used only to map an
`AXUIElement` to a `CGWindowID`. No CGS/SkyLight at all — the opposite of Rift's philosophy.

**Geometry writes** use `setAxFrame` → size, then position, **then size again**
(`MacApp.swift:411-419`), because some apps clamp size when position changes. Failures are
fire-and-forget; there is no verify-and-retry. Each app has its own AX thread, and a newer
layout cancels the stale in-flight frame-set for the same window — the same insight Rift
reaches via coalescing.

**Design traps worth naming:**
- AeroSpace also disables `AXEnhancedUserInterface` around writes. That is *not* why it
  can't animate — Rift disables the same attribute and animates fine. AeroSpace doesn't
  animate because it writes final frames once instead of interpolating. Disabling the
  attribute is compatible with dowm's animation plan.
- Monitors are keyed by `rect.topLeftCorner`, a `CGPoint`, so identity breaks when displays
  are rearranged. Rift's stable display UUIDs are strictly better — don't copy this.
- AeroSpace creates workspaces lazily by name and garbage-collects empty invisible ones
  (`garbageCollectUnusedWorkspaces`, `Workspace.swift:84`) unless listed in
  `persistentWorkspaces`. You asked for a **fixed, predefined, numbered set** — simpler, and
  what dowm should do.
- **Direct conflict between the two projects:** Rift *requires* "Displays have separate
  Spaces" ON; AeroSpace's guide recommends turning it OFF for stability. dowm uses no native
  Spaces, so it must work correctly under either setting and require neither.

## Finding D — does porting Rift's engine drag native Spaces in?

Checked directly, because it determines whether porting is safe:

| Component | `SpaceId` occurrences | Verdict |
|---|---|---|
| All 6 layout systems + `model/tree.rs` | **0** | Clean. Imposes no constraint on layout or movement. |
| `sys/{display_link,axuielement,enhanced_ui,event_tap,executor,run_loop,observer}.rs` | **0** | Clean. |
| `actor/app.rs` | 7, **all in comments** | Clean in code. |
| `actor/reactor/animation.rs` (presenter) | 75, but **0 calls to any CGS/SkyLight/space API** | `SpaceId` is only an opaque grouping key for per-space viewport cameras; its tests fabricate `SpaceId::new(1)`. Port = rename the key to `WorkspaceId`/`DisplayId`. |
| `sys/screen.rs` | 29 | **Needs splitting**: display enumeration (keep) is mixed with space queries (drop). |
| `sys/{skylight,window_server}.rs`, `actor/spaces.rs`, `sys/space_switch.rs` | many | Not in the port set at all. |

So porting the engine does **not** force native Spaces and does **not** constrain layout or
window movement. The only real work it implies is the key-type rename in the presenter and
splitting `screen.rs`.

Two findings from this check that feed the plan:

1. **`workspace_switch_layout()` already exists** (`animation.rs:1120`) — Rift has a
   workspace-switch animation entry point, directly reusable for animated corner parking.
2. **An OS constraint we inherit regardless of design:** `kAXWindowsAttribute` is
   space-filtered by macOS — AX enumerates only windows on the *current* native space
   (`app.rs:1295, 2163`). This is why Rift needs CGS machinery to see across spaces. Under
   AeroSpace's model every managed window stays on one native space, so AX sees them all and
   that machinery becomes unnecessary — an argument *for* the merge. dowm must still handle
   a user manually dragging a window onto another native space (detect and reclaim, or
   release it from management).

---

## Plan

### Design decisions

| Decision | Choice |
|---|---|
| Workspace identity | Fixed pool of N numbered workspaces, created at startup from config. No lazy creation, no GC. |
| Display identity | Stable display UUID (Rift's approach), **not** AeroSpace's CGPoint keying. |
| Workspace→display | Priority chain: force-assignment → currently-visible → last-seen display UUID → primary. |
| Hiding | Corner parking, monitor-aware corner choice, proportional position saved before hiding. |
| Native Spaces | Not used at all. Must work with "Displays have separate Spaces" either on or off. |
| Private APIs | SkyLight/CGS for fast *reads*, event subscriptions and window ordering only. Geometry always via AX. |
| Concurrency | Rift's three domains: reactor / presenter / one thread per app — ported, not rewritten. |
| Layouts at v1 | tiles, accordion, floating only; `LayoutSystem` trait shaped so Rift's others drop in later. |
| Foreign native Spaces | If a user drags a managed window onto another native Space, AX stops enumerating it — detect and either reclaim the window or release it from management. |
| Persistence | Keyed by workspace number + display UUID — both stable, so no fuzzy reconciliation layer needed. |
| Safety | Termination handler restores every window to a visible position on quit, crash and panic. |

**The product differentiator:** because hiding and showing are just AX frame writes, and the
presenter already interpolates toward arbitrary target rects, *workspace switching becomes an
animation for free* — the outgoing workspace slides out to the corner while the incoming one
slides in. Neither Rift (which animates only within a Space) nor AeroSpace (which doesn't
animate) does this.

### Repository shape

`/Users/dobbikov/Desktop/coding/utilities/dowm`, a Cargo workspace mirroring Rift's split:

```
dowm/                    # bin: the daemon
  crates/dowm-protocol   # OS-agnostic command/query/event types
  crates/dowm-client     # IPC client lib used by the CLI
  src/sys/               # AX, SkyLight reads, display link, event tap, run loop
  src/model/             # WindowStore, WorkspaceStore, DisplayStore, tree
  src/layout/            # LayoutSystem trait + implementations
  src/actor/             # reactor, per-app, presenter, input, config
```

### Milestones

Each ends in something runnable.

**M0 — Risk-retiring spike.** Before any architecture: one binary that finds a single app's
windows over AX, parks one window in its monitor's corner, and animates it back in at display
refresh rate via `CADisplayLink`. Measure sustained AX write throughput at 10, 20 and 40
windows. *This validates the entire thesis* — if AX writes can't sustain an animated
multi-window workspace switch, the design changes here and nowhere later.

**M1 — Core engine (ported).** Bring over Rift's three-domain concurrency: reactor actor,
presenter thread with latest-tick display link, one OS thread per app with AX observers.
Concretely: copy `sys/{display_link,axuielement,enhanced_ui,event_tap,executor,run_loop,
observer}.rs` as-is; copy `actor/app.rs` and `actor/reactor/animation.rs`, renaming the
presenter's `SpaceId` key to `DisplayId`; **split `sys/screen.rs`**, keeping display
enumeration and dropping every space query. Add an Apache-2.0 `NOTICE` crediting
acsandmann/rift and tmandry/glide-wm. Write fresh: `WindowStore` and display tracking keyed
by UUID. Event sources: AX observers, NSWorkspace, display reconfiguration, and the subset
of CGS connection notifications used for window create/destroy/reorder.

**M2 — Workspaces and visibility (the heart).** Fixed numbered workspace pool;
one-visible-per-display invariant; corner parking with monitor-aware corner selection,
proportional restore, and unhide-then-hide ordering; **the termination/panic handler that
restores all windows**. Animated transitions through the presenter. Commands: `workspace N`,
`move-node-to-workspace N`, `workspace-back-and-forth`, `move-workspace-to-monitor`,
`focus-monitor`, plus `workspace-to-monitor-force-assignment` config (support AeroSpace's
`MonitorDescription` forms: sequence number, `main`, `secondary`, name regex). Display
connect/disconnect, including plugging the hole on the source monitor.

**M3 — Tiling, AeroSpace parity only.** The tree model, then exactly three systems:
`tiles`, `accordion` and `floating` — written fresh behind a `LayoutSystem` trait shaped so
Rift's bsp, master-stack, scrolling and traditional can drop in later without a redesign
(they are zero-coupling, so adding one is a self-contained task whenever you want it). Plus
the normalization passes: flatten single-child containers, opposite orientation for nested
containers. Commands: `focus`, `move`, `join-with`, `split`, `resize` (width/height/smart,
redistributing sibling weights), `layout`, `fullscreen`, `balance-sizes`,
`flatten-workspace-tree`.

**M4 — Config, hotkeys, binding modes.** TOML config with hot reload; hotkey parsing and
dispatch; **binding modes** (`mode.main.binding`, `mode.resize.binding`, `mode.service.binding`
— a Rift gap you flagged); `on-window-detected` callbacks with app-id/title matchers;
`on-focus-changed` and `on-focused-monitor-changed` callbacks (your config uses both);
focus-follows-mouse; gaps.

**M5 — IPC, CLI, UI.** Protocol crate; **Unix domain socket with length-framed JSON**
(AeroSpace's transport — simpler than Rift's Mach server and sufficient); `dowm` CLI with
`list-*` queries and `subscribe`; menu bar indicator.

### Principal risks

- **A ~1px sliver of every hidden window stays visible** — macOS refuses to place a window
  fully off-screen. Inherent to the model and documented by AeroSpace. Consider additionally
  lowering parked windows' level or opacity via SkyLight, which AeroSpace cannot do because
  it avoids private APIs but dowm can.
- **Parked windows are still on-screen objects** — they surface in Mission Control,
  screenshots and screen sharing. Budget for the Exposé workaround and test Cmd-Tab.
- **Some apps resist off-screen positioning or clamp size on move.** Hence AeroSpace's
  size→position→size dance and its Zoom special-case. Needs a detect-and-fallback path.
- **AX write throughput under animation** is the make-or-break number; M0 exists to measure
  it before anything is built on top.
- **Animating a window out to a corner is visible by design** and may read worse than an
  instant switch. Prototype both in M2 and choose by eye.

## Verification

- **M0:** run the spike; confirm visually smooth motion, log sustained frame intervals, and
  record the window count that still holds 60fps. If it fails, revisit the design.
- **Unit/scenario tests:** copy Rift's seam — construct the reactor directly and feed
  synthetic events, asserting on `WorkspaceStore`/`WindowStore` with no AX or CGS. Port the
  scenarios in `rift/src/actor/reactor/global_workspaces.rs:538-1151` (display
  connect/disconnect, workspace routing) as the M2 acceptance suite.
- **Crash-safety test:** `kill -TERM` and force-panic the daemon with windows parked; every
  window must return to a visible position. Run this before daily-driving dowm.
- **Manual multi-monitor matrix** after M2: switch workspaces per display; move a workspace
  between displays; unplug/replug a display; change resolution; sleep/wake. Confirm no window
  is stranded. Repeat with "Displays have separate Spaces" both on and off.
- **Side-by-side against your AeroSpace config** (`~/.config/aerospace/aerospace.toml` —
  workspaces 1-9, `move-node-to-workspace`, `move-workspace-to-monitor --wrap-around`,
  resize/service binding modes, `on-window-detected` rules for WezTerm and Chrome) as the M4
  compatibility target.
