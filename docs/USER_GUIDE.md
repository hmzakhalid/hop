# Hop user guide

## What you need

| Machine | Requirements |
|---|---|
| Mac | macOS 14 or later, Apple silicon or Intel |
| PC | Windows 10 or 11 |
| Both | The same network: Wi-Fi or Ethernet, ideally 5 GHz Wi-Fi or a cable for the PC |

## Set up

### On the PC

1. Download [`Hop-Setup.exe`](https://github.com/hmzakhalid/hop/releases/latest/download/Hop-Setup.exe), run it and follow it. It asks for administrator permission, because it installs a small Windows service: with it, your Mac also works on the sign-in screen, in UAC prompts and in admin windows such as Task Manager. See [WINDOWS_SERVICE.md](WINDOWS_SERVICE.md).
   - **"Windows protected your PC":** Hop is not code-signed yet, so SmartScreen does not recognise it. Choose **More info → Run anyway**.
   - **Antivirus:** some antivirus apps, Microsoft Defender included, may flag an unsigned app that reads input and captures the screen. If yours removes Hop, restore it from the antivirus history and allow it. The source is public, so you can check what it does.
2. At the end, leave **Pair a Mac now** ticked. A window shows a code like `7KQ2-M9XD-4TRB`. It works once and expires after 2 minutes.
3. Later, pair again from the **Hop icon in the taskbar → Pair a Mac…**, or Start → Hop → **Pair a Mac with Hop**.

### On the Mac

1. Download [`Hop.dmg`](https://github.com/hmzakhalid/hop/releases/latest/download/Hop.dmg), open it, and drag **Hop** to **Applications**.
2. Open Hop from Applications. Hop is not notarized by Apple yet, so the first time macOS says it cannot verify Hop. Choose **Done**, then open System Settings → **Privacy & Security**, scroll down, and choose **Open Anyway** next to the message about Hop. You do this once.
3. macOS asks for **Accessibility** access: System Settings → Privacy & Security → Accessibility → turn on **Hop**. Hop starts by itself once it is allowed, and a keyboard icon appears in the menu bar. If macOS also asks about **finding devices on your local network**, allow it.
4. Hop looks for your PC and asks for the code. Pick the PC and type the code it shows. (Later: menu-bar icon → **Pair with a PC…**.)
5. Optional: **Settings → Open at Login**.

The menu shows whether Hop is connected, lets you switch with a click, and holds the settings: where the PC is, switching by screen edge, ⌘ as the Windows key, and Mac shortcut translation.

## Switching

| Action | How |
|---|---|
| Go to the PC | ⌃⌥⌘ + the arrow toward the PC (→ by default) |
| Come back | ⌃⌥⌘ + the opposite arrow. Any ⌃⌥⌘ + arrow switches. |
| Switch with a click | Hop menu → **Switch to *PC*** |
| Switch by pushing the pointer across | Hop menu → Settings → **Switch by Pushing Past the Screen Edge**. It works both ways: off the Mac's edge to the PC, and off the PC's opposite edge back. |
| PC on another side | Hop menu → Settings → **Where is the PC?** |
| See where the keyboard is | While the PC has it, a blue badge at the top of the Mac's screen says so |

Edges are off by default, so both machines' own screen edges keep working: the Dock, the Start menu, window snapping and scrollbars.

## See the PC's screen

Hop menu → **Show *PC*'s Screen** opens the PC's screen in a window on the Mac. It is for when the PC's monitor is out of sight or across the room: the picture is streamed live (hardware HEVC or H.264 on the PC), usually within a frame or two.

- Press ⌃⌥⌘→ as usual to control the PC while you watch it in the window.
- If the PC stops sending pictures for a couple of seconds, the window says so.
- It works on the lock screen and sign-in screen too, when the Hop service is installed.

## Keys on the PC

| Mac | Windows |
|---|---|
| ⌘ Command | Ctrl, so ⌘C, ⌘V, ⌘Z and ⌘S work as you expect |
| ⌥ Option | Alt (right ⌥ is AltGr for accented characters) |
| ⌃ Control | the Windows key |
| ⌘Tab | Alt+Tab (hold ⌘ to keep the switcher open) |
| ⌘Q | Alt+F4 |
| ⌘Space, or tapping ⌃ | Start |
| ⌘← / ⌘→ | Home / End |
| ⌘↑ / ⌘↓ | Ctrl+Home / Ctrl+End |
| ⌥← / ⌥→ / ⌥⌫ | Ctrl+← / Ctrl+→ / Ctrl+Backspace |
| ⌃L | Win+L (lock) |
| ⌃D / ⌃E / ⌃I | Win+D (desktop) / Win+E (Explorer) / Win+I (Settings) |
| ⌃⇧S | Win+Shift+S (screenshot) |
| ⌘⇧Esc | Ctrl+Shift+Esc (Task Manager) |

Ways to change this:
- `--cmd-as-win` makes ⌘ the Windows key and ⌃ plain Ctrl.
- `--no-chords` turns the shortcut translations off.
- If macOS grabs a combination first (⌃← and ⌃→ switch Mac desktops), turn that shortcut off in System Settings → Keyboard → Keyboard Shortcuts.

The PC's own keyboard layout decides which characters you get, as with any keyboard plugged into it.

**Mouse, trackpad and scrolling:**
- **Pointer speed:** follows the Mac's acceleration, scaled to the PC's display scaling. `--speed` adjusts it.
- **Scrolling:** trackpad scrolling is smooth, momentum included. `--scroll-speed` adjusts it.
- **Taps in games:** a trackpad tap is held for at least 50 ms on the PC, so games that read the mouse once per frame see it.

## Speed

```
hop bench
```
It reports the round trip to the PC: p50, p90, p99 and loss. One way is about half of that.

**For the lowest latency:**
- Put the PC on Ethernet.
- Use 5 GHz Wi-Fi on the Mac.
- If `hop bench` shows occasional spikes, AirDrop and Handoff (AWDL) are a common cause. Try `sudo ifconfig awdl0 down` while you work; `sudo ifconfig awdl0 up` turns it back on.

`hop --stats` prints the round trip, memory and CPU every 5 seconds. `hop-agent --stats` prints memory and CPU on the PC every 10 seconds.

## Troubleshooting

| Problem | Fix |
|---|---|
| Pairing finds no PC | Is Hop installed on the PC (its icon is in the taskbar), on the same network? On the Mac, allow Hop to find devices on the local network (System Settings → Privacy & Security → Local Network). Check that Windows Firewall allows `hop-agent`. |
| ⌃⌥⌘→ does nothing, and the menu says "Paused: Secure Input is on" | A password field or a password manager (Bitwarden, 1Password, a browser login, Discord's login) has turned on macOS Secure Input, which hides keystrokes from every app, Hop included. Close it or lock it. If it stays on, lock and unlock the Mac (⌃⌘Q). As a last resort, log out and back in. |
| The menu says Hop needs Accessibility access | System Settings → Privacy & Security → Accessibility → turn on Hop. If it is on but Hop still asks, select Hop in that list, remove it with **−**, then open Hop again and allow it. |
| "Hop cannot be opened" or "Apple could not verify Hop" | System Settings → Privacy & Security → scroll down → **Open Anyway** next to the message about Hop. |
| "The PC is not accepting pairing" | On the PC, choose **Pair a Mac…** from the Hop icon for a fresh code. |
| "Accessibility access" | Turn it on for your terminal app in System Settings → Privacy & Security → Accessibility. |
| Control comes back to the Mac on the lock screen or in Task Manager | The Hop service is not running: the taskbar icon says so. Run Setup again. |
| The hotkey does not bring you back | Close `hop-agent` on the PC with its own keyboard or mouse; the Mac gets its input back within half a second. |
| The PC's pointer is invisible but moves (no mouse plugged into the PC) | Windows hides the pointer when it finds no mouse. Hop turns on Windows' Mouse Keys the first time it moves a hidden pointer, which makes Windows draw it again, and turns it back off when Hop stops. While it is on, the keypad moves the pointer only in the Num Lock state you are not using. |
| A key seems stuck on the PC | Press and release it on the Mac. The PC also releases everything if the Mac goes quiet for 0.75 s. |
| Starting over | Hop menu → **Forget This PC…** on the Mac, and `hop-agent unpair` in an administrator terminal on the PC, then pair again. |

## Uninstall

- **Mac:** Hop menu → **Quit Hop**, then drag Hop from Applications to the Trash. Its settings and pairing are in `~/Library/Application Support/Hop`.
- **PC:** Settings → Apps → Installed apps → **Hop** → Uninstall. This removes the service and its firewall rule.

## Safety guarantees

- **The Mac never stays locked out:**
  - If the PC stops answering for half a second, the Mac takes its keyboard and pointer back.
  - A separate watchdog does the same even if another part of Hop hangs.
  - Quitting (Ctrl+C or closing the terminal) always restores the pointer.
- **The PC never keeps a stuck key:**
  - Every message lists exactly which keys should be down.
  - The PC releases everything when the Mac goes quiet.
- **Password fields stay on the Mac:** while a Mac password field has Secure Input on, Hop does not switch, and it hands control back if Secure Input turns on.

## Command line

Everything the app does also works from a terminal, which is handy for scripts and for testing. `Hop.app/Contents/MacOS/Hop` is the `hop` command:

| Command | What it does |
|---|---|
| `hop pair [--host IP]` | Pair, typing the code in the terminal |
| `hop run [--side S] [--edges] [--cmd-as-win] [--no-chords] [--speed X] [--stats]` | Run without the menu bar (macOS asks for Accessibility for the terminal app) |
| `hop bench` | Measure the round trip to the PC |
| `hop unpair` | Forget the PC |

On the PC, `hop-agent` runs the same agent in a console window, for when you do not want the service:
- `hop-agent` shows a code and runs;
- `hop-agent pair` shows a fresh code;
- `hop-agent install` and `hop-agent uninstall` manage the service from an administrator terminal.
