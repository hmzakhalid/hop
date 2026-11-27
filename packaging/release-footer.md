## Install

**Windows PC** (Windows 10 or 11): download **Hop-Setup.exe** and run it.
- If Windows says "Windows protected your PC", choose **More info → Run anyway**. Hop is not code-signed yet.
- Hop then runs in the background, with an icon in the taskbar.

**Mac** (macOS 14 or later, Apple silicon or Intel): download **Hop.dmg**, open it and drag Hop to Applications.
- The first time you open it, macOS says it can't verify Hop. Choose **Done**, then System Settings → **Privacy & Security** → **Open Anyway**.
- Allow **Accessibility** when asked. Hop then finds your PC: click **Connect**, then click **Yes** on the PC. Both screens show the same number.

Press **⌃⌥⌘→** to use the PC and **⌃⌥⌘←** to come back.

Full guide: [docs/USER_GUIDE.md](https://github.com/hmzakhalid/hop/blob/main/docs/USER_GUIDE.md). Check your download against `SHA256SUMS.txt` with `shasum -a 256 Hop.dmg` (Mac) or `certutil -hashfile Hop-Setup.exe SHA256` (PC).
