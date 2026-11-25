# Architecture

Hop is two programs and a shared core:

- **`hop`** runs on the Mac, as the menu-bar app `Hop.app` or as a command. It captures the keyboard and trackpad and sends them to the PC.
- **`hop-agent`** runs on the PC, normally as a Windows service, with `hop-tray` for the taskbar icon. It receives the input and replays it into Windows.
- **`hop-core`** is the shared core, and has no operating-system input code. Both programs use it. Almost all the logic lives there and is tested on any machine.

## Crates

| Crate | Builds for | Depends on | What it holds |
|---|---|---|---|
| `hop-core` | any OS | `snow`, `blake2`, `rand` | Protocol, encryption, key mapping, screen geometry, the Mac controller, the PC receiver, the Mac session, storage |
| `hop` | macOS | `hop-core`, `objc2-*`, `libc` | The menu-bar app, the live engine (event tap, cursor, badge, watchdog), preferences, CLI, screen viewer |
| `hop-agent` | Windows (dry-run elsewhere) | `hop-core`, `windows-sys` | `SendInput` injection, the Windows service, pairing beside it, the tray app, self-test |
| `hop-video` | Windows | `hop-core`, `windows` | Screen capture and hardware HEVC / H.264 encoding for screen view |

Dependencies point one way: the apps depend on `hop-core`, and `hop-core` depends on no app. Inside `hop-core`, `wire` and `clock` sit at the bottom, and `session` and `agent` at the top.

## How input flows

```
Mac                                                         PC
───                                                         ──
CGEventTap (run.rs)                                         agent::run (agent.rs)
   │ every key, click, move, scroll                            │ decrypt, replay window
   ▼                                                           ▼
Controller (controller.rs)  ── decides ──▶ Output           Receiver (receiver.rs)
   │  crossing? hotkey? lease?             { send, swallow,    │ each transition exactly once, in order;
   │  KeyMapper (mapper.rs): ⌘→Ctrl…         effect }          │ stalls on refusal or too-short clicks;
   │  layout.rs: virtual PC cursor                             │ releases what the Mac no longer holds
   ▼                                                           ▼
Session (session.rs) ─ Link (link.rs) ═══ UDP / Noise ═══▶  Injector trait
   │  sends, timers, acks, lease deadline                      │  win.rs: SendInput (scan codes,
   ▼                                                           │  absolute VIRTUALDESK moves, wheel)
Effects (run.rs): Cursor (cursor.rs) + badge (hud.rs)          ▼
                                                            Windows
```

1. The tap hands each macOS event to `Session::act`, which locks the `Controller` and calls it.
2. The controller returns an `Output`:
   - a full **state datagram** to send;
   - whether to **swallow** the Mac event;
   - an optional **effect** (`Enter`, or `Leave { warp_to, reason }`).
3. The session sends the datagram and carries out the effect while still holding the lock, so effects always happen in decision order.
4. The agent decrypts the datagram and passes it to the `Receiver`, which calls the `Injector`.
5. Acks travel back. Each ack confirms progress, carries the PC's monitors and cursor, and renews the lease.

## Threads

**Mac (`hop`):**

| Thread | Job |
|---|---|
| Main | Runs the event-tap callback and the AppKit run loop (menu, dialogs, badge, viewer) |
| Session receive | Acks and link events → `Controller::ack` / `session_changed` |
| Session timer | Heartbeats (15 ms while in control), retransmits (+2 ms), keepalives, the lease, reconnects |
| System watch | Secure Input every 100 ms, display layout every 2 s |
| Watchdog | Lock-free. At the lease deadline it takes the cursor back through `Cursor::revoke_if`, even if other threads hang. |
| Signals | SIGINT, SIGTERM and SIGHUP restore the cursor and tell the PC before exiting |
| Supervisor (app only) | Finds the paired PC, connects, follows it to a new address, reconnects after a pairing or settings change |

**PC (`hop-agent`):**

| Thread | Job |
|---|---|
| Main | The agent loop: bounded receive batches, the receiver's dead-man timer, monitors, acks. Time-critical priority. |
| Pairing | Only during a pairing window: accepts one TCP pairing |
| Service stop / Ctrl+C | Sets the stop flag, so the loop releases every key before exiting |

## Where to make a change

| You want to… | Change |
|---|---|
| Add or change a key mapping or a chord | `hop-core/src/keymap.rs` (table) or `mapper.rs` (chords), with a test in `mapper.rs` |
| Change when control switches | `controller.rs` (`mouse_moved`, `key`, `can_cross`) and `layout.rs` for geometry |
| Change a timer | Shared by both sides: `LEASE_US` and `DEADMAN_US` in `wire.rs` (a compile-time check keeps the dead-man longer than the lease). Mac only: the constants at the top of `controller.rs`. Reconnects: `link.rs`. |
| Add a message type | `wire.rs` (`Msg` encode and decode), then handle it in `session.rs` (Mac) or `agent.rs` (PC) |
| Change what is stored on disk | `store.rs` (`PcRecord`, `MacRecord`, `Store`) |
| Change how input reaches Windows | `hop-agent/src/win.rs` (`Injector` and `Platform` for `WinPlatform`) |
| Change what happens on the Mac at a crossing | `hop/src/live.rs` (`MacEffects`) and `cursor.rs` |
| Change the menu-bar app (menu, dialogs, connection) | `hop/src/app.rs`; check with `hop menu` |
| Change packaging | `packaging/macos/` (bundle, signing, Sparkle, DMG) and `packaging/windows/` (installer) |
| Change the badge | `hop/src/hud.rs` (look and text) |
| Change a command or flag | `hop/src/cli.rs` or `hop-agent/src/main.rs` |
| Change the console versus service behaviour | `hop-agent/src/role.rs` |
| Change the service install or security | `hop-agent/src/service.rs` |

## Module reference

### hop-core

| Module | Purpose |
|---|---|
| `wire` | Packet and message formats; Windows key-code space (`code`) |
| `crypto` | Noise pairing (XXpsk3 over TCP), sessions (IK over UDP), datagram sealing, replay window, pairing codes |
| `keymap` | Mac key codes to Windows scan codes; modifier bits; `mac::HYPER` |
| `mapper` | `KeyMapper`: Mac key events to the Windows key set and ordered transitions (chords, lazy modifiers, entry-held modifiers) |
| `layout` | Edges, entry and exit points, virtual PC cursor across monitors, DPI scale, `Side` |
| `controller` | The Mac decision maker: crossings, the lease, transitions queue, closing queue, heartbeats, RTT |
| `session` | `Session`: drives the controller over a link; `Effects` for what a crossing means locally |
| `link` | Discovery, pairing client, `Link` (encrypted UDP to one PC; `maintain` connects and reconnects) |
| `receiver` | `Receiver`: applies state to an `Injector`; `MIN_CLICK_US`; dead-man; retries; `Recorder` for tests |
| `agent` | The PC loop: discovery replies, pairing window, paired-key-only sessions, acks |
| `store` | `MacStore` / `AgentStore`: key file plus peer record, owner-only |
| `h264` | Annex B to AVCC for the Mac decoder |
| `stats` | Percentile summaries |
| `clock` | One monotonic microsecond clock |

### hop (Mac)

| Module | Purpose |
|---|---|
| `main` | Dispatch; `Hop.app` opened with no arguments starts the app |
| `app` | The menu-bar app: status item and menu, the connection supervisor, pairing dialogs |
| `live` | The live engine shared by the app and `hop run`: tap callback, `MacEffects`, watchdog, signals, system watch, stats; drives whichever `Session` is attached |
| `prefs` | The user's settings, saved as a `ControllerConfig` |
| `login` | Open at Login (`SMAppService`) |
| `updates` | Sparkle, loaded at runtime when the bundle ships it |
| `cli` | `Command` and its options, usage text |
| `pc` | Pair, unpair, find and connect to the PC |
| `run` | `hop run`: connect from a terminal and run the live engine |
| `cursor` | Hide, freeze, park and revoke the Mac cursor safely across threads |
| `macos` | FFI: event tap, cursor calls, displays, permissions, Secure Input, sockets |
| `hud` | The badge (AppKit panel) |
| `view` | The screen viewer (`AVSampleBufferDisplayLayer`) |
| `tools` | `bench` |
| `devtools` | `drive`, `view --file`, `badge`, `menu` (only with the `dev-tools` feature; packaged builds leave them out) |

### hop-agent (PC)

| Module | Purpose |
|---|---|
| `main` | Commands; `pair` goes beside the service when it is installed |
| `scm` | Service Control Manager queries, shared with `hop-tray` |
| `bin/hop-tray` | The taskbar icon: service status, "Pair a Mac…" (elevated `hop-agent pair`) |
| `role` | Console versus service: storage, log, pairing, injector, the run loop setup |
| `platform` | OS pieces: injector choice, Ctrl+C, service stop, memory and CPU |
| `win` | `WinPlatform`: `SendInput`, monitors and DPI, cursor, secure-desktop following |
| `service` | Install and uninstall, pairing beside the service, the service process, the SYSTEM helper launcher |
| `hooks`, `selftest` | Low-level hooks that observe Hop's own (tagged) input; the self-test |
| `dryrun` | A printing injector for non-Windows hosts |

### hop-video

| Module | Purpose |
|---|---|
| `capture` | `Capturer`: DXGI Desktop Duplication, then the D3D11 video processor (BGRA to NV12), then the Media Foundation H.264 encoder (NVENC, AMF or QuickSync, with a software fallback) |
| `bin/hop-capture` | Records a monitor to an `.h264` file with timings |
