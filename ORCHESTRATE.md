# Plan-Orchestrate Result

**Plan**: `PLAN.md`
**Lang**: `rust` (see note)
**ECC mode**: `plugin`
**Steps**: 6 milestones (M0–M5). M1 and M2 are split, so there are 8 commands.
**Scope**: all

> **About the language.** When this was generated, `dowm/` had no code yet; it only contained `PLAN.md`. Automatic detection found no project files, so it would have fallen back to the generic `code-reviewer`. The plan clearly says this is a Rust Cargo workspace, so `rust-reviewer` is used instead.

> **About splits.** M1 and M2 each cover several unrelated pieces of work. They are split into 2a/2b and 3a/3b so each chain stays focused and has a clear finish line. Step ids are their order in the plan: M0 is step 1, M5 is step 6.

## Steps overview

| # | Title | Tags | Chain |
|---|---|---|---|
| 1 | M0 — Spike to test animated AX writes | impl | `ecc:tdd-guide,ecc:rust-reviewer` |
| 2a | M1 — Port the engine from Rift | migration, impl, refactor | `ecc:architect,ecc:tdd-guide,ecc:refactor-cleaner,ecc:rust-reviewer` |
| 2b | M1 — New WindowStore, display tracking, event sources | impl | `ecc:tdd-guide,ecc:rust-reviewer` |
| 3a | M2 — Workspace model and commands | impl, test | `ecc:tdd-guide,ecc:rust-reviewer` |
| 3b | M2 — Corner parking, animation, crash safety | impl | `ecc:tdd-guide,ecc:rust-reviewer` |
| 4 | M3 — Tiling (tiles, accordion, floating) | impl | `ecc:tdd-guide,ecc:rust-reviewer` |
| 5 | M4 — Config, hotkeys, binding modes | impl | `ecc:tdd-guide,ecc:rust-reviewer` |
| 6 | M5 — IPC, CLI, menu bar | impl, security | `ecc:tdd-guide,ecc:rust-reviewer,ecc:security-reviewer` |

---

## Step 1 — M0: Spike to test animated AX writes

**Intent**: Build one binary that hides a window in its monitor's corner and animates it back at the display's refresh rate. Measure how many AX writes per second it can sustain at 10, 20 and 40 windows.
**Tags**: impl
**Chain rationale**: The plan says "Before any architecture", which is not a design step, so no `planner` or `architect`. `e2e-runner` is left out because it drives a web browser and can't test macOS AX. `rust-reviewer` finishes the chain.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-1] M0 spike for dowm (Rust macOS tiling WM): one binary that finds a single app's windows over AX, parks one window just past its monitor's bottom corner (offset by its own size), and animates it back at display refresh via CADisplayLink; benchmark sustained AX write throughput at 10, 20 and 40 windows; Acceptance: motion visually smooth; frame-interval log recorded per window count; report states the max window count holding 60fps"
```

## Step 2a — M1: Port the engine from Rift

**Intent**: Copy Rift's FFI layer, per-app actor and presenter into the project. Rename the presenter's `SpaceId` key to `DisplayId`, split `screen.rs` so it keeps display listing and drops space queries, and add the Apache-2.0 `NOTICE`.
**Tags**: migration, impl, refactor
**Chain rationale**: `architect` goes first to set the port boundary and the `screen.rs` split. `tdd-guide` ports the code with tests, and `refactor-cleaner` removes leftover space-related code. `rust-reviewer` finishes the chain; it is placed last because a refactor chain must end with a reviewer. The out-of-scope list is taken from Finding B's "Does not transfer" list.

```bash
/ecc:orchestrate custom "ecc:architect,ecc:tdd-guide,ecc:refactor-cleaner,ecc:rust-reviewer" "[Plan: PLAN.md#step-2a] M1 port from utilities/rift (Apache-2.0) into dowm: copy sys/{display_link,axuielement,enhanced_ui,event_tap,executor,run_loop,observer}.rs as-is; port actor/app.rs and actor/reactor/animation.rs, renaming presenter SpaceId key to DisplayId; split sys/screen.rs keeping display enumeration, dropping space queries; add NOTICE crediting acsandmann/rift and tmandry/glide-wm; Acceptance: cargo build+clippy clean; no SpaceId or CGS space calls in ported code; presenter tests pass with DisplayId; Out of scope: actor/spaces.rs, sys/space_switch.rs, model/space_activation.rs, SpaceId-keyed persistence and its fuzzy reconciliation, space-keyed IPC queries"
```

## Step 2b — M1: New WindowStore, display tracking, event sources

**Intent**: Write a new `WindowStore` and display tracking, both keyed by stable display UUID. Connect AX, NSWorkspace, display-reconfiguration and CGS window events to the reactor, using the same test setup Rift uses.
**Tags**: impl
**Chain rationale**: This is new code with a clear test setup (reactor fed fake events), so `tdd-guide` fits. `rust-reviewer` finishes the chain.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-2b] M1 fresh core for dowm: WindowStore and display tracking keyed by stable display UUID (not CGPoint); wire reactor event sources — AX observers, NSWorkspace, display reconfiguration, and the CGS connection-notification subset for window create/destroy/reorder; reactor test seam built directly with synthetic events, no AX/CGS; Acceptance: scenario tests cover window create/destroy and display add/remove by UUID; daemon runs and logs live window events"
```

## Step 3a — M2: Workspace model and commands

**Intent**: A fixed pool of numbered workspaces, with exactly one visible per display. Workspaces map to displays by a priority chain. Add the workspace and monitor commands, force-assignment config, and handling for displays connecting and disconnecting.
**Tags**: impl, test
**Chain rationale**: This is all reactor-level logic, so it can be fully tested with fake events. The acceptance suite is Rift's `global_workspaces.rs` scenarios, which `tdd-guide` writes. `e2e-runner` is left out because it is browser-based. `rust-reviewer` finishes the chain.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-3a] M2 workspace model in dowm reactor: fixed pool of N numbered workspaces from config, no lazy creation/GC; one-visible-per-display bidirectional invariant; workspace->display chain: force-assignment, currently visible, last-seen display UUID, primary; commands workspace N, move-node-to-workspace N, workspace-back-and-forth, move-workspace-to-monitor, focus-monitor; force-assignment by sequence number/main/secondary/name regex; display connect/disconnect plugs source-monitor hole; Acceptance: rift global_workspaces.rs:538-1151 scenarios ported and passing; invariant holds after every command"
```

## Step 3b — M2: Corner parking, animation, crash safety

**Intent**: Hide windows by moving them just past a monitor corner. The corner is chosen per monitor, and each window's position is saved proportionally before hiding. Animate workspace switches through the presenter, and add the handler that brings every window back into view on quit, signal or panic.
**Tags**: impl
**Chain rationale**: These are the riskiest behaviours for daily use, and both the crash test and the restore test can be checked directly. `rust-reviewer` finishes the chain. It also covers the unsafe signal and panic handling.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-3b] M2 visibility in dowm: corner parking past a monitor-aware bottom corner (probe both corners, diagonal weighted 10x, avoid neighbouring displays); save position proportionally before hiding; unhide visible workspaces before hiding others; size->position->size writes with app-clamp fallback; animate switches via presenter; termination/signal/panic handler restoring every window on-screen; Acceptance: kill -TERM and forced panic with parked windows leave none off-screen; proportional restore survives resolution change; animated and instant switch both selectable"
```

## Step 4 — M3: Tiling (tiles, accordion, floating)

**Intent**: Build the tree model, a `LayoutSystem` trait and three new layouts: tiles, accordion and floating. Add the tree clean-up passes (flatten one-child containers, nested containers take the opposite orientation) and the AeroSpace tiling commands.
**Tags**: impl
**Chain rationale**: "split" here is a command name, not a refactor, so it doesn't add the refactor tag. `rust-reviewer` finishes the chain and checks that the trait leaves room to add more layouts later. The out-of-scope clause comes from the plan's M3 text.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-4] M3 tiling for dowm: tree model plus LayoutSystem trait and exactly three fresh systems — tiles, accordion, floating; normalization flattens single-child containers and gives nested containers opposite orientation; commands focus, move, join-with, split, resize (width/height/smart, redistributing sibling weights), layout, fullscreen, balance-sizes, flatten-workspace-tree; Acceptance: tree/normalization unit tests pass; each command has a reactor scenario test; Out of scope: Rift's bsp, master-stack, scrolling and traditional layouts (drop in later without redesign)"
```

## Step 5 — M4: Config, hotkeys, binding modes

**Intent**: A TOML config that reloads while running, plus hotkeys and binding modes. Add the `on-window-detected`, `on-focus-changed` and `on-focused-monitor-changed` callbacks, focus-follows-mouse, and gaps.
**Tags**: impl
**Chain rationale**: The target is matching the existing AeroSpace config, which gives clear acceptance tests. `rust-reviewer` finishes the chain.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-5] M4 config for dowm: TOML config with hot reload; hotkey parsing and dispatch; binding modes mode.main/resize/service.binding; on-window-detected callbacks with app-id/title matchers; on-focus-changed and on-focused-monitor-changed callbacks; focus-follows-mouse; gaps; Acceptance: parse tests for every key; hot reload applies without restart; equivalents of ~/.config/aerospace/aerospace.toml (workspaces 1-9, resize/service modes, WezTerm/Chrome rules) work"
```

## Step 6 — M5: IPC, CLI, menu bar

**Intent**: Add the protocol and client crates, and a Unix socket that sends JSON with a length prefix on each message. Build the `dowm` CLI with `list-*` and `subscribe`, plus a menu bar indicator.
**Tags**: impl, security
**Chain rationale**: The plan doesn't use security words for this step. The tag was added because any local process that can reach the socket can control every window, so file permissions and message checks matter. `security-reviewer` finishes the chain.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer,ecc:security-reviewer" "[Plan: PLAN.md#step-6] M5 IPC for dowm: crates/dowm-protocol (OS-agnostic command/query/event types) and crates/dowm-client; Unix domain socket with length-framed JSON; dowm CLI with list-* queries and subscribe; menu bar indicator; Acceptance: protocol round-trip tests pass; CLI drives every M2-M4 command over the socket; socket is user-only and rejects oversized or malformed frames"
```

---

## Batch execution

Run step 1 on its own first and look at its throughput numbers before running the rest. The plan says the design may change based on M0.

```bash
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-1] M0 spike for dowm (Rust macOS tiling WM): one binary that finds a single app's windows over AX, parks one window just past its monitor's bottom corner (offset by its own size), and animates it back at display refresh via CADisplayLink; benchmark sustained AX write throughput at 10, 20 and 40 windows; Acceptance: motion visually smooth; frame-interval log recorded per window count; report states the max window count holding 60fps"
/ecc:orchestrate custom "ecc:architect,ecc:tdd-guide,ecc:refactor-cleaner,ecc:rust-reviewer" "[Plan: PLAN.md#step-2a] M1 port from utilities/rift (Apache-2.0) into dowm: copy sys/{display_link,axuielement,enhanced_ui,event_tap,executor,run_loop,observer}.rs as-is; port actor/app.rs and actor/reactor/animation.rs, renaming presenter SpaceId key to DisplayId; split sys/screen.rs keeping display enumeration, dropping space queries; add NOTICE crediting acsandmann/rift and tmandry/glide-wm; Acceptance: cargo build+clippy clean; no SpaceId or CGS space calls in ported code; presenter tests pass with DisplayId; Out of scope: actor/spaces.rs, sys/space_switch.rs, model/space_activation.rs, SpaceId-keyed persistence and its fuzzy reconciliation, space-keyed IPC queries"
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-2b] M1 fresh core for dowm: WindowStore and display tracking keyed by stable display UUID (not CGPoint); wire reactor event sources — AX observers, NSWorkspace, display reconfiguration, and the CGS connection-notification subset for window create/destroy/reorder; reactor test seam built directly with synthetic events, no AX/CGS; Acceptance: scenario tests cover window create/destroy and display add/remove by UUID; daemon runs and logs live window events"
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-3a] M2 workspace model in dowm reactor: fixed pool of N numbered workspaces from config, no lazy creation/GC; one-visible-per-display bidirectional invariant; workspace->display chain: force-assignment, currently visible, last-seen display UUID, primary; commands workspace N, move-node-to-workspace N, workspace-back-and-forth, move-workspace-to-monitor, focus-monitor; force-assignment by sequence number/main/secondary/name regex; display connect/disconnect plugs source-monitor hole; Acceptance: rift global_workspaces.rs:538-1151 scenarios ported and passing; invariant holds after every command"
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-3b] M2 visibility in dowm: corner parking past a monitor-aware bottom corner (probe both corners, diagonal weighted 10x, avoid neighbouring displays); save position proportionally before hiding; unhide visible workspaces before hiding others; size->position->size writes with app-clamp fallback; animate switches via presenter; termination/signal/panic handler restoring every window on-screen; Acceptance: kill -TERM and forced panic with parked windows leave none off-screen; proportional restore survives resolution change; animated and instant switch both selectable"
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-4] M3 tiling for dowm: tree model plus LayoutSystem trait and exactly three fresh systems — tiles, accordion, floating; normalization flattens single-child containers and gives nested containers opposite orientation; commands focus, move, join-with, split, resize (width/height/smart, redistributing sibling weights), layout, fullscreen, balance-sizes, flatten-workspace-tree; Acceptance: tree/normalization unit tests pass; each command has a reactor scenario test; Out of scope: Rift's bsp, master-stack, scrolling and traditional layouts (drop in later without redesign)"
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer" "[Plan: PLAN.md#step-5] M4 config for dowm: TOML config with hot reload; hotkey parsing and dispatch; binding modes mode.main/resize/service.binding; on-window-detected callbacks with app-id/title matchers; on-focus-changed and on-focused-monitor-changed callbacks; focus-follows-mouse; gaps; Acceptance: parse tests for every key; hot reload applies without restart; equivalents of ~/.config/aerospace/aerospace.toml (workspaces 1-9, resize/service modes, WezTerm/Chrome rules) work"
/ecc:orchestrate custom "ecc:tdd-guide,ecc:rust-reviewer,ecc:security-reviewer" "[Plan: PLAN.md#step-6] M5 IPC for dowm: crates/dowm-protocol (OS-agnostic command/query/event types) and crates/dowm-client; Unix domain socket with length-framed JSON; dowm CLI with list-* queries and subscribe; menu bar indicator; Acceptance: protocol round-trip tests pass; CLI drives every M2-M4 command over the socket; socket is user-only and rejects oversized or malformed frames"
```
