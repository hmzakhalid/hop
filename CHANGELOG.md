# Changelog

## 1.0.0

The first public release. Hop is free and open source under the GPL-3.0.

Updated on 28 November 2026. If you downloaded Hop before then, download it again on both machines and install it over the old one:
- **Pairing is a click:** Connect on the Mac, Yes on the PC, with the same number on both screens. A code still works, from **Use a Code…**.
- A Mac paired by the first download no longer shows the pairing window at every launch.
- Reinstalling Hop on the PC no longer swaps the PC's key, which broke pairing.
- The Mac keeps up when the PC moves to a new network address.
- A pairing code stops working as soon as it expires.
- Reopening the screen view window works every time.
- ⌃L locks the PC once, not again on every key repeat.
- Mouse buttons 8 and up each work on their own.
- Quitting Hop always gives the Mac its keyboard back first.
- Smaller fixes to pairing, timing and the command line.

- **Use your Mac's keyboard and trackpad on a Windows PC.** Press ⌃⌥⌘→ to go to the PC and ⌃⌥⌘← to come back, or push the pointer past the screen edge.
- **Mac shortcuts on Windows:**
  - ⌘C, ⌘V, ⌘Z and ⌘S work as Ctrl.
  - ⌘Tab works as Alt+Tab, ⌘Q as Alt+F4, ⌥← as Ctrl+←, ⌃L locks the PC, and more.
- **The whole PC:** the sign-in and lock screens, UAC prompts, admin windows and games, through a small Windows service.
- **See the PC's screen on the Mac:** a live window streamed with the PC's hardware encoder (HEVC or H.264).
- **Fast:** about 5 ms round trip on home Wi-Fi; Hop's own overhead is about 0.1 ms per side.
- **Safe:**
  - The Mac always gets its keyboard back within half a second if the PC stops answering.
  - No key ever stays stuck on the PC.
  - Password fields never cross over.
- **Private:**
  - Paired with a click on each machine (or a one-time code), end-to-end encrypted (Noise, ChaCha20-Poly1305).
  - A direct connection between your two computers: no account, no cloud server, no tracking.
- **Easy setup:** the Mac app finds the PC and offers to connect the first time it opens; the PC asks you to allow it.

Known limits:
- **Not signed yet:**
  - The Mac app is not notarized, so macOS asks you to choose "Open Anyway" once.
  - The Windows installer is not code-signed, so SmartScreen asks you to choose "Run anyway", and some antivirus apps may flag it.
- **No automatic updates yet:** download new versions from the website or GitHub.
