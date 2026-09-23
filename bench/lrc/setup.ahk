#Requires AutoHotkey v2.0
; setup.ahk - creates a throwaway hero-scenario catalog and imports the 50-file working set.
;
; Scope: this script only handles the reliably-scriptable half of setup (new catalog, import by
; folder). Applying the edit stack (WB/vibrance/masks/AI Denoise) and syncing it across the set is
; NOT scripted here -- LRC's mask/denoise UI doesn't have stable enough coordinates/timing to
; automate blind, and getting it wrong would silently corrupt the "same edit stack on all 50
; images" precondition every downstream measurement depends on. See README.md's manual checklist
; for that half; do it once per LRC config (originals / smart-previews) and verify by eye before
; running hero.ahk.
;
; Usage: AutoHotkey64.exe setup.ahk <config-name> <source-dir>
;   config-name: originals | smart-previews (only affects the catalog file name)
;   source-dir:  folder containing the 50 hero-set files (see docs/benchmarks/hero-scenario.md)

#SingleInstance Force
SendMode "Event"
SetTitleMatchMode 2

if A_Args.Length < 2 {
    MsgBox "Usage: setup.ahk <config-name: originals|smart-previews> <source-dir>"
    ExitApp 1
}
configName := A_Args[1]
sourceDir := A_Args[2]

if !DirExist(sourceDir) {
    MsgBox "Source dir not found: " sourceDir
    ExitApp 1
}

catalogDir := "H:\NictiBench\lrc-bench"
DirCreate(catalogDir)
catalogPath := catalogDir "\hero-" configName ".lrcat"

if FileExist(catalogPath) {
    result := MsgBox("Catalog already exists:`n" catalogPath "`n`nDelete and recreate?", "setup.ahk", "YesNo")
    if result = "No"
        ExitApp 0
    FileDelete(catalogPath)
}

if !WinExist("ahk_exe Lightroom.exe") {
    MsgBox "Launch Lightroom Classic first (any catalog), then re-run this script."
    ExitApp 1
}
WinActivate("ahk_exe Lightroom.exe")
Sleep(500)

; File > New Catalog...
Send("!f")
Sleep(300)
Send("n")
Sleep(1500) ; new-catalog dialog + relaunch takes a few seconds

; The "Create Folder or Locate Catalog" dialog is a standard Windows save dialog.
if WinWait("Create Folder with Catalog", , 10) {
    WinActivate("Create Folder with Catalog")
    Send("^a")
    SendText(catalogPath) ; literal text -- avoids Send()'s {}/^!+# key-code interpretation
    Send("{Enter}")
} else {
    MsgBox "New-catalog dialog didn't appear as expected -- create '" catalogPath "' manually via File > New Catalog, then re-run with an existing empty catalog open."
    ExitApp 1
}

; LRC relaunches into the new (empty) catalog.
if !WinWait("ahk_exe Lightroom.exe", , 30) {
    MsgBox "Lightroom didn't relaunch into the new catalog within 30s -- check it manually."
    ExitApp 1
}
Sleep(2000)
WinActivate("ahk_exe Lightroom.exe")

; File > Import Photos and Video...
Send("^{Shift}i")
if !WinWait("Import", , 15) {
    MsgBox "Import dialog didn't appear -- import the " sourceDir " folder manually (Add, not Copy/Move)."
    ExitApp 1
}
MsgBox "Import dialog is open. Navigate to:`n" sourceDir "`n`nSelect all 50 files, choose 'Add' (not Copy/Move -- these are the frozen ref-10k originals), then click Import.`n`nClick OK here once the import has finished."
ExitApp 0
