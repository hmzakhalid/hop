# Pairing by asking (2026-11-27)

**Why:** pairing with a code took four steps across two machines: open "Pair a Mac" on the PC, accept a Windows permission prompt, read a 12-character code, and type it on the Mac. People expect what phones and headphones do: pick the device, then allow it on the other one. Now the Mac offers **Connect**, the PC asks **Yes / No**, and the code is the second way.

**What changed:**
- **The exchange (`hop-core/src/ask.rs`):** Noise XX with a committed nonce, so both screens show a six-digit number that matches only when nothing is in the middle. The PC answers in one byte. It stores the Mac only after the Mac says it has stored the PC, so a Mac that gave up never replaces a working pairing. See [PROTOCOL.md](../PROTOCOL.md).
- **The PC agent (`agent.rs`):** requests run on their own thread, one at a time, through an `Asker`. The agent stores every pairing itself, with the generation check, for codes and requests alike, and switches to it live. A different Mac ends the old one's input session and releases its keys.
- **Who answers (`hop-agent/src/asker.rs`):** the console agent asks in its terminal. The service's helper asks the signed-in user with `WTSSendMessage`, only if that user is an administrator.
- **The Mac (`pc.rs`, `app.rs`, `hop pair`):** Connect, then "Click Yes on the PC" with the number; **Use a Code…** opens the old dialog; `hop pair --code` is the old command.
- **A first-run bug:** a Mac paired before generations existed (the first 1.0.0 download) counted as never paired, so the pairing window opened at every launch.

## Special cases kept on purpose

| Special case | The constraint behind it |
|---|---|
| Requests use their own TCP port (agent port + 2, 47822) | A code shown beside the Windows service holds the TCP twin of the agent's port, so one listener cannot serve both |
| Only an administrator signed in at the PC can say yes | A Mac paired with the service types on the sign-in screen and into UAC prompts; a code beside the service needs an administrator too |
| After a request is turned down or fails, requests are dropped for 10 s | Otherwise anyone on the network could keep a prompt on the PC's screen |
| The generation is taken when the person says yes, not when the request arrives | Requests nobody allowed must not cancel a code pairing under way |
| A replaced Mac's screen view runs until its window closes | The video process checks the pairing at each handshake only; the input session, which can type, ends at once |
| If the PC cannot store the Mac after the Mac stored the PC, the Mac believes it is paired | The two stores are on two machines; pairing again fixes it |
| The Mac's "Click Yes" dialog is closed from the answer with `abortModal` | AppKit has no other way to end a modal alert from code |
