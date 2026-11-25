# The Windows service

## Why it exists

A normal program cannot send input to three parts of Windows:
- **the lock screen and UAC prompts**, which run on a separate secure desktop;
- **admin windows** such as Task Manager, which ignore input from lower-privileged programs (UIPI).

Without the service, the agent detects these and hands control back to the Mac. With the service, the Mac can use them too.

## How it works

1. `hop-agent install` (run as administrator) sets everything up:
   - copies the agent to `%ProgramFiles%\Hop\hop-agent.exe`, which only admins can write;
   - creates `%ProgramData%\Hop` in one step, already restricted to SYSTEM and Administrators (owner Administrators, protected DACL). An existing directory is kept only if it and its files are provably private; otherwise it is moved aside;
   - copies the agent's key and pairing there, so the Mac stays paired;
   - registers the `HopAgent` service (LocalSystem, automatic start, restart on failure);
   - adds a firewall rule for that program;
   - starts the service.
2. **The service** runs in session 0. It keeps one helper running in the active console session: the same program in `system-helper` mode, with a SYSTEM token. It restarts the helper if it exits, or if the console session changes (user switch, sign-out). A helper that keeps failing is retried with a growing delay, up to a minute, and each exit code is logged.
3. **The helper** runs the normal agent loop with two differences:
   - Before injecting, it moves its thread onto whichever desktop currently has input: the normal desktop, the lock screen or a UAC prompt.
   - Running as SYSTEM, admin windows accept its input.
4. **Protection first:** the helper refuses to start if `%ProgramData%\Hop` is missing, rather than creating it without its protection. The service re-creates it, protected, when it starts. The helper also refuses to start without its stop event, because a helper that could only be terminated could not release held keys.
5. **Stopping:** each helper launch gets its own stop event with a random name that only SYSTEM can open. The service signals it, then waits for the helper to exit, terminating it if necessary and confirming the exit. The helper releases every held key before it exits.

Logs: `%ProgramData%\Hop\agent.log` (helper, including why it stopped) and `%ProgramData%\Hop\service.log` (service: helper starts, exit codes, launch failures). Both are kept across restarts and start over past 1 MB.

## Pairing with the service running

The helper listens on the UDP port only. It never opens the TCP pairing port, and never shows a code. `hop-agent pair`, run as an administrator, does the pairing on its own:
1. it reads the **service's** key from `%ProgramData%\Hop`, so the Mac pins the service's identity;
2. it listens on the same port number over TCP, beside the running service;
3. it saves the new Mac to the service's store;
4. it restarts the service, so the helper loads the new pairing.

Cancelling at any point, or closing the window, leaves the service untouched.

Pairing needs an administrator because a Mac paired with the service can type on the sign-in screen and into UAC prompts. The tray never pairs by itself: it asks Windows to run `hop-agent pair` elevated, so Windows shows its permission prompt.

## Security model

- **Only the paired Mac:** only the Mac whose key is pinned in the protected directory can open a session, and every datagram is authenticated. Being on the same network gives an attacker nothing.
- **Nothing a standard user can change:** the program, the keys and the pairing are writable only by SYSTEM and Administrators.
- **Tools run by full path:** the installer runs Windows tools (`sc.exe`, `taskkill.exe`, PowerShell) by absolute System32 path, so a look-alike placed next to the installer is never run.
- **No pairing:** the helper never offers pairing, since it has no console and nobody to show a code to. Pair first without the service, then install.
- **Revocation:** `hop-agent unpair` as administrator removes the service's copy of the pairing. It then restarts the service and confirms the old helper is gone.
- **Residual risk:** the installer copies itself. Run `install` from a folder only you can write to, as with any installer.

## Commands

| Command | What it does |
|---|---|
| `hop-agent install` | Install or update the service (administrator) |
| `hop-agent uninstall` | Stop and remove the service, the firewall rule and the program. The pairing in ProgramData is kept. Anything that could not be removed is listed. (administrator) |
| `hop-agent pair` | Show a code and pair a Mac. With the service installed, it needs an administrator, and pairs beside the running service (see below). The tray's "Pair a Mac…" runs this. |
| `hop-agent unpair` | Forget the Mac. Also clears the service's copy (administrator, if the service is installed). |

## Not yet supported

- Ctrl+Alt+Del (it needs `SendSAS` and a policy change).
