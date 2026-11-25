# Architecture

Hop is two programs and a shared core. Beside input, a second channel streams the PC's screen to a window on the Mac (screen view):

- **`hop`** runs on the Mac, as the menu-bar app `Hop.app` or as a command. It captures the keyboard and trackpad and sends them to the PC.
- **`hop-agent`** runs on the PC, normally as a Windows service, with `hop-tray` for the taskbar icon. It receives the input and replays it into Windows.
- **`hop-core`** is the shared core, and has no operating-system input code. Both programs use it. Almost all the logic lives there and is tested on any machine.

## Crates

| Crate | Builds for | Depends on | What it holds |
|---|---|---|---|
| `hop-core` | any OS | `snow`, `blake2`, `rand`, `reed-solomon-erasure` | Protocol, encryption, key mapping, screen geometry, the Mac controller, the PC receiver, the Mac session, the PC's session acceptor, the video channel, storage |
| `hop` | macOS | `hop-core`, `objc2-*`, `libc` | The menu-bar app, the live engine (event tap, cursor, badge, watchdog), preferences, CLI, screen viewer |
| `hop-agent` | Windows (dry-run elsewhere) | `hop-core`, `windows-sys` | `SendInput` injection, the Windows service, pairing beside it, the tray app, self-test |
| `hop-video` | Windows | `hop-core`, `windows` | Screen capture and hardware HEVC / H.264 encoding for screen view |

Dependencies point one way: the apps depend on `hop-core`, and `hop-core` depends on no app. Inside `hop-core`, `wire` and `clock` sit at the bottom; `crypto`, `link` and `responder` above them; and `session`, `agent` and `stream` at the top. Every message type, input or video, is defined in `wire`, and its encoding is pinned by a test, because Hop has no protocol version: a Mac and a PC a release apart must still understand each other.

## How input flows

```
Mac                                                         PC
───                                                         ──
CGEventTap (live.rs)                                        agent::run (agent.rs)
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
Effects (live.rs): Cursor (cursor.rs) + badge (hud.rs)         ▼
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
| Signals | Started first thing. SIGINT, SIGTERM and SIGHUP run the quit hooks (the same as the menu's Quit): restore the cursor, tell the PC, save settings |
| Supervisor (app only) | Finds the paired PC, connects, follows it to a new address, reconnects after a pairing or settings change |
| Viewer (while screen view is open) | The video client and decoder; with control, a cursor follower. Each opening of the window has its own stop flag, set when it closes |

**PC (`hop-agent`):**

| Thread | Job |
|---|---|
| Main | The agent loop: bounded receive batches, the receiver's dead-man timer, monitors, acks. Time-critical priority. |
| Pairing | Only during a pairing window: accepts TCP pairings, each bounded by one deadline |
| Video supervisor | Keeps `hop-agent video` (a separate process) running, with a backoff |
| Service stop / Ctrl+C | Sets the stop flag, so the loop releases every key before exiting |

## Where to make a change

| You want to… | Change |
|---|---|
| Add or change a key mapping or a chord | `hop-core/src/keymap.rs` (table) or `mapper.rs` (chords), with a test in `mapper.rs` |
| Change when control switches | `controller.rs` (`mouse_moved`, `key`, `can_cross`) and `layout.rs` for geometry |
| Change a timer | Shared by both sides: `LEASE_US` and `DEADMAN_US` in `wire.rs` (a compile-time check keeps the dead-man longer than the lease). Mac only: the constants at the top of `controller.rs`. Reconnects: `link.rs`. |
| Add a message type | `wire.rs` (`Msg` encode and decode, and the pinned-bytes test), then handle it in `session.rs` (Mac) or `agent.rs` (PC), or for video in `stream.rs` |
| Change screen view | Frame rules (fragments, parity, loss, rate control): `video.rs`. The channel at both ends: `stream.rs`. Capture and encode: `hop-video`. The Mac window: `hop/src/screen.rs` and `view.rs`. The PC process: `hop-agent/src/video.rs` |
| Change what is stored on disk | `store.rs` (`PcRecord`, `MacRecord`, `Store`; pairing changes go through `Store::lock`) |
| Change how input reaches Windows | `hop-agent/src/win.rs` (`Injector` and `Platform` for `WinPlatform`) |
| Change what happens on the Mac at a crossing | `hop/src/live.rs` (`MacEffects`) and `cursor.rs` |
| Change the menu-bar app (menu, dialogs, connection) | `hop/src/app.rs`; check with `hop menu` |
| Change packaging | Names, paths and the release build: `packaging/common.sh`. Then `packaging/macos/` (bundle, signing, Sparkle, DMG), `packaging/windows/` (installer) and `packaging/release.sh` |
| Change the badge | `hop/src/hud.rs` (look and text) |
| Change a command or flag | `hop/src/cli.rs` or `hop-agent/src/cli.rs` (each command takes only its own options; both parse from an iterator, with tests) |
| Change a menu label the other machine quotes | `hop-core/src/ui.rs` |
| Change the console versus service behaviour | `hop-agent/src/role.rs` |
| Change the service install or security | `hop-agent/src/service.rs` |

## Module reference

### hop-core

| Module | Purpose |
|---|---|
| `wire` | Packet and message formats for both channels; Windows key-code space (`code`); which mouse buttons travel |
| `crypto` | Noise pairing (XXpsk3 over TCP, one deadline, `PairError`), sessions (IK over UDP), datagram sealing, replay window, pairing codes |
| `keymap` | Mac key codes to Windows scan codes; modifier bits; `mac::HYPER` |
| `mapper` | `KeyMapper`: Mac key events to the Windows key set and ordered transitions (chords, lazy modifiers, entry-held modifiers) |
| `layout` | Edges, entry and exit points, virtual PC cursor across monitors, DPI scale, `Side` |
| `controller` | The Mac decision maker: crossings, the lease, transitions queue, closing queue, heartbeats, RTT |
| `session` | `Session`: drives the controller over a link; `Effects` for what a crossing means locally |
| `link` | Discovery, pairing client, `Link` (encrypted UDP to one PC; `maintain` connects and reconnects) |
| `receiver` | `Receiver`: applies state to an `Injector`; `MIN_CLICK_US`; dead-man; retries; `Recorder` for tests (`test-support` feature) |
| `responder` | `Responder`: the PC end of either channel (paired-key handshakes, replay-safe promotion, address following, the unknown-session hint) |
| `agent` | The PC input loop: discovery replies, pairing window, acks |
| `video` | Screen-view rules: `Encoded` frames, fragments with Reed–Solomon parity, reassembly and loss, rate control, clock offset |
| `stream` | The video channel: `serve` (PC) and `Client` (Mac, which keeps asking for frames on its own schedule); `FileSource` plays recordings |
| `store` | `MacStore` / `AgentStore`: key file plus peer record, owner-only; `Store::lock` for changes, with a generation token |
| `ui` | Menu labels one machine's text quotes from the other |
| `h264` | Annex B parsing; length-prefixed frames for the Mac decoder |
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
| `pc` | The paired PC, as a library for the app and the commands: pair, forget, find, connect; its timings |
| `commands` | `hop pair`, `unpair`, `bench`, `view`: each returns an error for `main` to print |
| `run` | `hop run`: connect from a terminal and run the live engine |
| `screen` | The screen-view window's threads: one viewer per opening, each with its own stop flag |
| `cursor` | Hide, freeze, park and revoke the Mac cursor safely across threads |
| `macos` | FFI: event tap, cursor calls, displays, permissions, Secure Input, sockets |
| `hud` | The badge (AppKit panel) |
| `view` | The viewer window: `AVSampleBufferDisplayLayer`, the decoder (which reports the picture size), the cursor overlay, the close callback |
| `devtools` | `drive`, `view --file`, `badge`, `menu` (only with the `dev-tools` feature; packaged builds leave them out) |

### hop-agent (PC)

| Module | Purpose |
|---|---|
| `main` | Dispatch; `pair` goes beside the service when it is installed |
| `cli` | `Command` parsing, per-command options, usage text |
| `video` | Screen view on the PC: `VideoArgs` (the child's command line, built and parsed in one place), the supervisor, the server |
| `backoff` | Relaunch delays for the service's helper and the video process |
| `lock` | When ⌃L locks the PC (Windows ignores an injected Win+L) |
| `scm` | Service Control Manager queries, shared with `hop-tray` |
| `bin/hop-tray` | The taskbar icon: service status, "Pair a Mac…" (elevated `hop-agent pair`) |
| `role` | Console versus service: storage, log, pairing, injector, the run loop setup |
| `platform` | OS pieces: injector choice, Ctrl+C, service stop, memory and CPU, child processes tied to the agent |
| `win` | `WinPlatform`: `SendInput`, monitors and DPI, cursor, secure-desktop following, Mouse Keys for a hidden pointer; a caught mode (swallow, selftest) that acts only through `SendInput` |
| `service` | Install and uninstall, pairing beside the service, the service process, the SYSTEM helper launcher |
| `hooks`, `selftest` | Low-level hooks that observe Hop's own (tagged) input; the self-test |
| `dryrun` | A printing injector for non-Windows hosts |

### hop-video

| Module | Purpose |
|---|---|
| `capture` | `Capturer`: DXGI Desktop Duplication (following the lock screen when asked), then the D3D11 video processor (BGRA to NV12), then the Media Foundation encoder: hardware HEVC, else hardware H.264 (NVENC, AMF or QuickSync), else software H.264 |
| `source` | `ScreenSource`: the capturer as the video channel's `Source` |
| `bin/hop-capture` | Records a monitor to an `.h264` file with timings |
