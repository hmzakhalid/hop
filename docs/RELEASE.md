# Releasing Hop

Hop is free software under the GPL-3.0. Releases are on [GitHub Releases](https://github.com/hmzakhalid/hop/releases). The website links to `releases/latest/download/Hop.dmg` and `…/Hop-Setup.exe`, so a new release updates every download link without touching the site.

## Cutting a release

1. **Version and changelog.** Bump `version` in the root `Cargo.toml`, and add a `## <version>` section to [CHANGELOG.md](../CHANGELOG.md). The release notes are that section, followed by [packaging/release-footer.md](../packaging/release-footer.md).
2. **Commit and push.**
3. **Build and check:**
   ```
   packaging/release.sh
   ```
   - **Builds:** both apps, from the committed tree, so the build id names a public commit.
   - **Paths:** keeps this machine's paths out of the binaries ([packaging/common.sh](../packaging/common.sh)).
   - **Leak check:** refuses to continue if a binary still contains your home folder, user name, computer name or git email.
   - **Output:** `Hop.dmg`, `Hop-Setup.exe` and `SHA256SUMS.txt`, in `dist/release/`.
4. **Try them** on a Mac and a PC.
5. **Publish:**
   ```
   packaging/release.sh --publish
   ```
   This creates the GitHub release `v<version>` with the three files.

**Tools:** the Windows build needs `brew install mingw-w64 makensis` and `rustup target add x86_64-pc-windows-gnu`. Publishing also needs `gh`, signed in.

## Mac app

`packaging/macos/build.sh` makes `dist/Hop.app` (universal: Apple silicon and Intel) and `dist/Hop-<version>.dmg`. Settings live in `packaging/macos/config.sh`.

- **Bundle identifier:** `io.github.hmzakhalid.hop`. Never change it: Accessibility permission, Open at Login and updates all key off it.
- **Signing without a Developer ID:**
  - **What signs it:** the **Hop Local Development** certificate (`packaging/macos/make-local-identity.sh` creates it).
  - **Why:**
    - macOS ties Accessibility permission to the signature, so a stable certificate means updates keep the permission. An ad-hoc signature changes with every build.
    - `release.sh` refuses to run without it.
  - **Back up the private key:** export it from Keychain Access as a `.p12`, and store it somewhere safe. Losing it means everyone re-grants Accessibility once, on the next update.
- **What people see:** Gatekeeper says it can't verify Hop, and people choose **Open Anyway** once, in Privacy & Security. The website and the README walk them through it.
- **Signing with a Developer ID (step only you can do):** the Apple Developer Program costs USD 99 per year.
  1. Set `HOP_SIGN_IDENTITY` to your "Developer ID Application: …" identity.
  2. Create a notarytool profile: `xcrun notarytool store-credentials hop-notary --apple-id … --team-id … --password <app-specific password>`.
  3. Set `HOP_NOTARY_PROFILE=hop-notary`.

  The script then notarizes and staples the app and the DMG, and the "Open Anyway" step goes away.
  - **Warning:** moving from the local certificate to a Developer ID changes the signature once, so people re-grant Accessibility on that update.
- **Updates (Sparkle 2, optional):**
  1. Run Sparkle's `./bin/generate_keys` once, and back up the private key it keeps in your Keychain.
  2. Set `HOP_SPARKLE_PUBLIC_KEY` and `HOP_SPARKLE_FEED_URL`. The site can host `appcast.xml`.
  3. For each release, run `./bin/generate_appcast` on a folder holding the DMG, and upload the result.

  The build pins Sparkle 2.10.1 by checksum. Keep it at 2.9.6 or later (GHSA-3x7w-j75x-ppq5). Without these settings, the app has no updater, and people download new versions from the site.

## Windows installer

`packaging/windows/build.sh` makes `dist/Hop-Setup-<version>.exe` on the Mac, with NSIS. The installer:
- installs to Program Files;
- runs `hop-agent install`, which sets up the service, its protected data and the firewall rule;
- starts the tray icon at every sign-in;
- adds Start-menu shortcuts (Hop, Pair a Mac with Hop, Uninstall) and an Apps & features entry;
- offers "Pair a Mac now" at the end.

The uninstaller removes the service, the firewall rule and the files. It keeps the pairing in ProgramData.

**Signing (step only you can do; the biggest gap today):**
- **Without a signature:**
  - SmartScreen warns, and people choose **More info → Run anyway**.
  - Microsoft Defender has quarantined unsigned test builds: a service that captures the screen and starts helper processes looks like remote-access malware.
- **To sign:**
  1. **Get a certificate.**
     - *Azure Artifact Signing* (formerly Trusted Signing): about $10 a month, for individuals in the US and Canada, and for organizations in more countries.
     - *Otherwise:* an OV certificate with cloud signing (SSL.com eSigner or Certum), which needs no USB token.
  2. **Set `HOP_WIN_SIGN`** to a command that signs the file passed as its last argument. `jsign` does this from the Mac, for either option. The script signs both programs, the installer and the uninstaller.
  3. **Submit the signed files** to Microsoft as a false positive: microsoft.com/wdsi/filesubmission, as a "software developer".
- **Until then:** submitting the unsigned `Hop-Setup.exe` there also helps Defender stop flagging it.

## Website

- **What it is:** [`site/`](../site/) is a static site, with no build step: `index.html`, plus `vercel.json` for headers.
- **Deploying:** `vercel deploy site --prod` publishes to https://gethop.vercel.app (Vercel project `gethop`).
- **Links:** the download buttons point at the latest GitHub release, so they need no change per version.

## Checks before a release

- **Tests:** `cargo test --workspace`.
- **The menus:** `hop menu` (a `--dev` build) prints the app's menus for every state, and flags any item with no handler.
- **Without a PC:** `tools/loopback-rig.sh` runs the whole input path on the Mac.
- **On real machines:** install over the previous version on both sides, then check that:
  - pairing survives;
  - switching works;
  - the lock screen takes input (service);
  - screen view opens.
- **Test matrix:**
  - Mac: macOS 14 and later, on Apple silicon and Intel.
  - PC: Windows 10 and 11; NVIDIA, AMD and Intel graphics; several monitors and DPI scales.
