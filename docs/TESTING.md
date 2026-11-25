# Testing

## The suite

```
cargo test --workspace
```

**Unit tests:**
- **Key mapping and chords:** `mapper.rs`, `keymap.rs`.
- **Geometry:** `layout.rs`.
- **The controller:** leases, crossings, the transition queue, the closing queue.
- **The receiver:** exactly-once delivery, repair, refusals, minimum click, dead-man.
- **Crypto:** pairing, sessions, replay.
- **The rest:** wire format, storage, H.264 parsing, CLI parsing.

**End-to-end** (`hop-core/tests/e2e.rs`): the real agent loop and the real Mac `Session`, over UDP on loopback, through a proxy that drops, duplicates and reorders packets. It checks that:
- every key arrives exactly once and in order;
- no key is left stuck;
- the cursor lands exactly where it should;
- control returns within the lease when the PC dies;
- unpaired Macs are rejected;
- a replayed handshake cannot cut off a session;
- a junk flood cannot starve the PC's dead-man timer.

The same test binaries cross-compile for Windows and pass there:

```
cargo test -p hop-core --target x86_64-pc-windows-gnu --no-run
```
Then copy `target/x86_64-pc-windows-gnu/debug/deps/{hop_core,e2e}-*.exe` to the PC and run them.

## On a real PC, without typing into it

| Tool | What it does |
|---|---|
| `hop-agent selftest` | Injects real input through the agent's own code and catches it with low-level hooks before any app sees it. It checks what Windows received: exact cursor pixels on every monitor, scan codes and extended flags, buttons and wheel. Hop tags its own input, so physical input and other programs are never touched. |
| `hop-agent --swallow` with `hop drive` | Runs the agent for real but catches its own input. `hop drive` sends a scripted session (typing, ⌘C, ⌥←, ⌘Tab, clicks, a drag, scrolling) through the real controller, and the agent prints what Windows received. |

## On this Mac, without a PC

| Tool | What it does |
|---|---|
| `tools/loopback-rig.sh` | Starts a dry-run agent on this Mac with throwaway keys, pairs with it and runs `hop drive`: the whole path (pairing, session, controller, agent, receiver) without a PC, without an event tap, and without touching the real pairing |
| `hop menu` | Builds the menu-bar app's real menus for each state and prints them, flagging any item whose action has no handler |
| `packaging/macos/build.sh --dev` | A bundle with the developer commands, so `Hop.app/Contents/MacOS/Hop menu` checks that Sparkle loads inside the signed app |

## Latency and resources

| Command | What it shows |
|---|---|
| `hop bench` | Round-trip percentiles and loss to the PC |
| `hop --stats`, `hop-agent --stats` | Live round trip, memory and CPU |
| `hop-capture --seconds 5 --repeat` | Screen capture and encode timings on the PC (screen view) |

The definitive check is a 240 fps phone video: a key press on the Mac against the PC's screen, compared with a USB keyboard plugged into the PC.
