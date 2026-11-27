; Hop for Windows: installer and uninstaller (NSIS 3). Built by build.sh, which defines
; VERSION, STAGE (the folder with the files to install) and OUTFILE, and SIGN when signing.
;
; The installer copies the programs, then runs `hop-agent install`, which does the real setup:
; the Windows service, its protected data folder and the firewall rule. The tray icon starts at
; every sign-in.

Unicode true
ManifestDPIAware true
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "x64.nsh"

!define PRODUCT "Hop"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hop"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${PRODUCT}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${PRODUCT}"
BrandingText "${PRODUCT} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${PRODUCT}"
VIAddVersionKey "FileDescription" "${PRODUCT} Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "(c) 2026 the Hop authors. Free software under the GPL-3.0."

!ifdef SIGN
  !finalize '${SIGN} "%1"'
  !uninstfinalize '${SIGN} "%1"'
!endif

!define MUI_ICON "${STAGE}\hop.ico"
!define MUI_UNICON "${STAGE}\hop.ico"
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TEXT "Hop lets you use your Mac's keyboard and trackpad on this PC.$\r$\n$\r$\nSetup installs a small Windows service, so your Mac also works on the sign-in screen, in UAC prompts and in admin windows such as Task Manager.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_TEXT "Hop is installed and running. Look for its icon in the taskbar.$\r$\n$\r$\nNow open Hop on your Mac and click Connect. This PC will ask you to allow it: click Yes."

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE}\LICENSE.txt"
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Hop needs 64-bit Windows 10 or later."
    Abort
  ${EndIf}
  SetRegView 64
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

; Stop everything that holds our files: the service (and its helper) and every tray icon.
!macro StopHop
  nsExec::ExecToLog '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command "Stop-Service HopAgent -ErrorAction SilentlyContinue; Get-Process hop-tray,hop-agent -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue; Start-Sleep -Milliseconds 500"'
  Pop $0
!macroend

Section "Hop"
  SetShellVarContext all
  !insertmacro StopHop
  SetOutPath "$INSTDIR"
  File "${STAGE}\hop-agent.exe"
  File "${STAGE}\hop-tray.exe"
  File "${STAGE}\hop.ico"
  File "${STAGE}\LICENSE.txt"
  WriteUninstaller "$INSTDIR\Uninstall Hop.exe"

  DetailPrint "Setting up the Hop service…"
  nsExec::ExecToLog '"$INSTDIR\hop-agent.exe" install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONEXCLAMATION "Hop could not set up its service (code $0).$\r$\nSee the details above, then run Setup again."
  ${EndIf}

  WriteRegStr HKLM "${RUN_KEY}" "${PRODUCT}" '"$INSTDIR\hop-tray.exe"'
  CreateDirectory "$SMPROGRAMS\${PRODUCT}"
  CreateShortcut "$SMPROGRAMS\${PRODUCT}\Hop.lnk" "$INSTDIR\hop-tray.exe"
  Delete "$SMPROGRAMS\${PRODUCT}\Pair a Mac with Hop.lnk" ; its name before 1.0.0's update
  CreateShortcut "$SMPROGRAMS\${PRODUCT}\Pair with a Code.lnk" "$INSTDIR\hop-tray.exe" "--pair"
  CreateShortcut "$SMPROGRAMS\${PRODUCT}\Uninstall Hop.lnk" "$INSTDIR\Uninstall Hop.exe"

  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${PRODUCT}"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\hop-tray.exe"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall Hop.exe"'
  WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall Hop.exe" /S'
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
  WriteRegDWORD HKLM "${UNINSTALL_KEY}" "EstimatedSize" 2048

  ; Start the tray icon now as the signed-in user, not elevated like Setup.
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\hop-tray.exe"'
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  ; Removes the service and the firewall rule; keeps the keys and pairing in ProgramData.
  nsExec::ExecToLog '"$INSTDIR\hop-agent.exe" uninstall'
  Pop $0
  !insertmacro StopHop
  Delete "$INSTDIR\hop-agent.exe"
  Delete "$INSTDIR\hop-tray.exe"
  Delete "$INSTDIR\hop.ico"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\Uninstall Hop.exe"
  RMDir "$INSTDIR"
  RMDir /r "$SMPROGRAMS\${PRODUCT}"
  DeleteRegValue HKLM "${RUN_KEY}" "${PRODUCT}"
  DeleteRegKey HKLM "${UNINSTALL_KEY}"
SectionEnd
