; Nicti Windows installer (#249/ADR-0249).
;
; Per-user install, matching Chrome's non-admin mode -- no UAC prompt ever, so nicti-shed
; (crates/nicti-shed, #249 PR 2/2) can silently re-run this installer in place to apply an
; update without asking the user to elevate. A per-machine install with a privileged updater
; service (the Lightroom Classic / Chrome admin-mode model) is out of scope for v1 -- see
; docs/adr/0249-windows-installer-and-updates.md.
;
; Built via: makensis /DVERSION=<x.y.z> /DSRCDIR=<path to the built nicti.exe's directory> nicti.nsi
; Produces: Nicti-Setup-<x.y.z>-x64.exe in the current directory.

!ifndef VERSION
  !error "VERSION must be defined, e.g. makensis /DVERSION=0.1.0 nicti.nsi"
!endif
!ifndef SRCDIR
  !error "SRCDIR must be defined (directory containing the built nicti.exe)"
!endif

!include "MUI2.nsh"
!include "FileFunc.nsh"

Name "Nicti"
OutFile "Nicti-Setup-${VERSION}-x64.exe"
; Per-user, HKCU-only install -- no admin rights requested, no UAC prompt.
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\Nicti"
InstallDirRegKey HKCU "Software\Nicti" "InstallDir"
Unicode true
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "FileDescription" "Nicti installer"
VIAddVersionKey "ProductName" "Nicti"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "AGPL-3.0-or-later"

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; /UPDATE: set by nicti-shed (#249 PR 2/2) when re-invoking this installer in place over a
; running install. Waits for any running nicti.exe to exit (an update downloaded and verified
; while the app that triggered the check is still open) before overwriting its files, then
; relaunches the app once installed -- so the user never has to manually reopen it. A plain
; `/S`-only silent install (a fresh, non-update install) does not relaunch anything.
Var IsUpdate

Function .onInit
  ${GetParameters} $R0
  ${GetOptions} $R0 "/UPDATE" $R1
  IfErrors +2 0
    StrCpy $IsUpdate "1"
FunctionEnd

; Copies nicti.exe into $INSTDIR. On a fresh install there's nothing to wait for. On an
; /UPDATE re-install there may still be a running instance -- `nicti-shed::net::download_and_apply`
; spawns this installer and exits *immediately after*, so its own process teardown (including
; real GPU-device teardown, ADR-0016) is not guaranteed to have finished by the time this runs;
; a single fixed sleep is not a real guarantee either. So this force-terminates any running
; instance and then retries the file copy until the destination actually unlocks, rather than
; assuming any fixed window is enough -- and aborts cleanly (never leaving a half-updated install,
; since `File` fails atomically when the destination is locked) if it never does.
Function InstallExe
  StrCmp $IsUpdate "1" 0 do_copy
    ; /F: force-terminate. A graceful close request can simply fail to do anything if the
    ; process is unresponsive or mid-teardown -- this is an update overwriting the running
    ; instance's own files, not a "please save your work" prompt; the app has nothing unsaved to
    ; lose here (ADR-0021's catalog DB is the authority, not in-memory state).
    nsExec::ExecToStack 'taskkill /F /IM nicti.exe'
    Pop $0

  StrCpy $2 0 ; retry counter
  do_copy:
    ClearErrors
    SetOutPath "$INSTDIR"
    File "${SRCDIR}\nicti.exe"
    IfErrors 0 copy_done
      StrCmp $IsUpdate "1" 0 copy_failed ; a fresh install failing isn't a lock/retry situation
      IntOp $2 $2 + 1
      IntCmp $2 20 copy_failed
      Sleep 250
      Goto do_copy
  copy_failed:
    MessageBox MB_OK|MB_ICONEXCLAMATION "Nicti could not be updated because it's still running. Close Nicti and run this installer again."
    Abort
  copy_done:
FunctionEnd

Section "Nicti" SecMain
  Call InstallExe

  WriteRegStr HKCU "Software\Nicti" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Nicti" "Version" "${VERSION}"

  ; HKCU uninstall entry (per-user install -- never HKLM).
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "DisplayName" "Nicti"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "Publisher" "Nicti"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti" "NoRepair" 1

  CreateDirectory "$SMPROGRAMS\Nicti"
  CreateShortcut "$SMPROGRAMS\Nicti\Nicti.lnk" "$INSTDIR\nicti.exe"
  CreateShortcut "$SMPROGRAMS\Nicti\Uninstall Nicti.lnk" "$INSTDIR\Uninstall.exe"

  WriteUninstaller "$INSTDIR\Uninstall.exe"

  StrCmp $IsUpdate "1" 0 +2
    Exec '"$INSTDIR\nicti.exe"'
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\nicti.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\Nicti\Nicti.lnk"
  Delete "$SMPROGRAMS\Nicti\Uninstall Nicti.lnk"
  RMDir "$SMPROGRAMS\Nicti"

  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Nicti"
  DeleteRegKey HKCU "Software\Nicti"
SectionEnd
