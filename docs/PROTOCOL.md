# Protocol

Everything is little-endian. One UDP port, **47820** by default, carries discovery, session handshakes and encrypted data. The same port number over TCP carries pairing.

## Packets

| Type | Byte | Layout | Authenticated |
|---|---|---|---|
| Probe | `0x01` | `MAGIC "HOP1"`, `u32 probe_id` | no (carries nothing secret) |
| Probe reply | `0x02` | `MAGIC`, `u32 probe_id`, `[32] pc_public_key`, `u8 len`, `name` | no |
| Session init | `0x20` | `u32 initiator_index`, Noise IK message 1 | Noise |
| Session accept | `0x21` | `u32 initiator_index`, `u32 responder_index`, Noise IK message 2 | Noise |
| Data | `0x30` | `u32 receiver_index`, `u64 nonce`, ChaCha20-Poly1305 ciphertext | yes |
| Unknown session | `0x31` | `u32 index` | no (a hint only; see below) |

## Messages (inside Data)

### `State` (Mac → PC)
Every datagram carries the full input state, so any one of them repairs earlier loss. The fields:

- `epoch`, `active`, and `send_time_us` (echoed back for the round trip and the lease);
- the absolute cursor position `x`, `y` in PC pixels, so the cursor cannot drift;
- cumulative scroll totals `scroll_v`, `scroll_h` in Windows wheel units, where 120 is one notch;
- `buttons` and `pressed`: the Windows keys that should be down right now;
- `first_event` and `transitions`: every key or button change the PC has not yet acknowledged, each with the cursor position where it happened.

### `Ack` (PC → Mac)
- `epoch` and `applied_event`, the highest contiguous transition applied;
- `echo_time_us` and `hold_us`;
- `flags`: `ACTIVE`, `BLOCKED` (secure desktop) or `EPOCH_DEAD` (dead-man fired);
- the PC cursor;
- the monitors, as rectangles in physical pixels plus DPI scale.

### `Ping` / `Pong`
Latency measurement (`hop bench`).

## Pairing

Two ways, with the same result: each side stores the other's static key (`store.rs`). From then on, the PC accepts sessions from that one key only. A new pairing ends the previous Mac's input session at once, and the PC releases whatever it held.

### By asking (the usual way)

The Mac asks; someone at the PC allows it (`ask.rs`). Over TCP on the agent's port + 2 (47822), so a code shown beside the Windows service, which holds the TCP twin of the agent's port, never collides with it.

1. **Handshake:** `Noise_XX_25519_ChaChaPoly_BLAKE2s`, prologue `hop ask v1`. Message 2's payload is a commitment to the PC's 32-byte nonce, BLAKE2s(`hop ask commit v1\0` ‖ nonce), then the PC's name. Message 3's payload is the Mac's 32-byte nonce, then the Mac's name.
2. **Reveal:** the PC sends its nonce. The Mac checks it against the commitment.
3. **Check number:** the first 4 bytes (little-endian) of BLAKE2s(`hop ask check v1\0` ‖ handshake hash ‖ Mac nonce ‖ PC nonce), modulo 1,000,000, shown as `482 913` on both screens. A machine in the middle runs two handshakes and must fix its own nonce in each before it learns the other side's, so it cannot make the two numbers agree, except by a one-in-a-million chance per try.
4. **Answer:** the PC asks whoever is there, and sends one byte: `y` allowed, `n` declined, `a` needs an administrator, `t` nobody answered.
5. **Saved:** on `y`, the Mac stores the PC, then sends `saved`. The PC stores the Mac only on `saved`, so a Mac that gave up never replaces the PC's pairing.

**Deadlines:** the handshake and the reveal take at most 10 s; the person at the PC has 60 s (the Mac waits 5 s more); `saved` must arrive within 10 s.

**Who answers:** the console agent asks in its terminal. The Windows service's helper shows a Yes/No prompt to the user signed in to its session, but only if that user is an administrator, as a code beside the service needs one.

**Spam:** one request at a time. After a request is turned down or fails, the PC drops further requests for 10 s.

### With a code

The agent shows a 12-character code: 60 bits, valid for 2 minutes, at most 3 wrong attempts.

**Handshake:** `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s` over TCP on the agent's port. The PSK is BLAKE2s over a domain separator and the normalised code.

**Confirmation:** the PC answers with a confirmation sealed under the session keys. The Mac pins the PC's X25519 key only after that confirmation checks out, so a responder that does not know the code cannot get pinned.

## Sessions

- **Handshake:** `Noise_IK_25519_ChaChaPoly_BLAKE2s` over UDP. The Mac retries the handshake every 400 ms until it is accepted.
- **Replacing a session:** the PC keeps the live session until the new one delivers its first valid datagram. A replayed `SESSION_INIT` therefore cannot cut off input, and no clock is involved.
- **Datagrams:** each is sealed with an explicit 64-bit nonce. The receiver advances a 64-packet replay window only after authentication succeeds.
- **Reconnects:** if the PC has been silent for 3 s, the Mac re-handshakes. The PC answers data for an unknown index with an `Unknown session` hint. Because the hint is unauthenticated, the Mac honours it only when it names the current session and the PC has been silent for 1 s.
- **Network changes:** an authenticated packet from a new address moves the link to that address.

## Control

### Epochs
Every crossing starts a new epoch, and transition ids restart at 1. Modifiers held at the moment of crossing, such as the ⌃⌥⌘ of the hotkey, are not sent until they are pressed again.

### Transitions
- **Delivery:** the Mac keeps every transition until it is acknowledged. They ride in every datagram, are resent 2 ms after they first go out, and resent again with each 15 ms heartbeat.
- **Order:** the PC applies them exactly once and in order. A gap stops progress.
- **Overflow:** more than 64 unacknowledged transitions end the epoch.

### Waiting transitions (receiver)
A transition waits, and everything after it waits behind it (motion, scroll and repair included), when:
- Windows refused it (UIPI), or
- it is a button release less than **50 ms** after its press. Trackpad taps would otherwise be too short for games that read the mouse once per frame.

The Mac keeps resending, so the next datagram retries.

### Repair
After applying a datagram's transitions, the PC releases any key or button it holds that the datagram's pressed set no longer lists. Releases Windows refuses are retried on every tick.

### Handing back
- **Closing datagrams:** when control returns to the Mac, the transitions still in flight ride in the closing datagrams (marked `active = false`). These are resent every heartbeat while the PC keeps making progress.
- **Release:** the PC applies them, then releases everything.
- **No overtaking:** a new crossing waits until they have landed.

### Menu masking
Before releasing Alt or Win, the PC taps the unassigned key `vkE8`, so the menu bar or Start menu doesn't open. When a chord swaps modifiers, the new one is pressed before the old one is released.

## Safety timers

| Timer | Where | What it does |
|---|---|---|
| Lease, 500 ms | Mac | No acknowledgement of a state sent in the last 500 ms ends control. The deadline is measured from the echoed **send** time, so delayed acks cannot stretch it. |
| Watchdog | Mac | A lock-free thread enforces the same deadline on the cursor even if the session is stuck. A generation counter keeps it from revoking a newer crossing. |
| Dead-man, 750 ms | PC | Silence while anything is held releases everything and marks the epoch dead. It is longer than the lease, so state repair can never re-press a dropped drag. |
| Secure Input | Mac | Polled every 100 ms. Blocks crossing, and ends control if it turns on. |
| Bounded batches | PC | At most 64 packets or 2 ms per batch, so a junk flood cannot starve the timers. |
