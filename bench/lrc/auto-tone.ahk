#Requires AutoHotkey v2.0
; auto-tone.ahk - ADR-0099 (#99/#202): creates a throwaway auto-tone catalog, imports a sample
; set, and applies LRC's own "Auto Settings" to every image in it.
;
; Unlike hero.ahk/setup.ahk, this needs no manual edit-stack application and no Ctrl+S/XMP write:
; Auto Settings is a single deterministic action (Ctrl+U over the whole selection), and #202 reads
; the result straight out of this throwaway .lrcat via `pupil`'s `truth` module -- see ADR-0099's
; Decision section. This still follows setup.ahk's "don't automate the file-picker" caution: the
; import step itself is manual (see below), same reasoning as setup.ahk's own comment on why the
; edit-stack step isn't scripted there.
;
; Usage: AutoHotkey64.exe auto-tone.ahk <source-dir> [bench-root]
;   source-dir:  folder containing the sample set to import (e.g. a `nicti-prowl select`/`refset`
;                pick copied out of ref-10k, or a subset of H:\NictiBench-subset -- #202 decides
;                which, once it can actually reach ref-10k on the reference machine).
;   bench-root:  optional -- where to create the auto-tone-bench catalog dir. Defaults to the
;                NICTI_BENCH_ROOT env var; if neither is given, errors instead of guessing a drive.
;
; This NEVER touches the user's real Lightroom catalog -- it always creates a fresh, empty
; catalog first (same as setup.ahk), and only ever operates on that.

#SingleInstance Force
SendMode "Event"
SetTitleMatchMode 2

if A_Args.Length < 1 {
    MsgBox "Usage: auto-tone.ahk <source-dir> [bench-root]"
    ExitApp 1
}
sourceDir := A_Args[1]
benchRoot := A_Args.Length >= 2 ? A_Args[2] : EnvGet("NICTI_BENCH_ROOT")

if !DirExist(sourceDir) {
    MsgBox "Source dir not found: " sourceDir
    ExitApp 1
}

if benchRoot = "" {
    MsgBox "No bench root given and NICTI_BENCH_ROOT is not set -- pass it as a 2nd argument or set the env var (e.g. a scratch drive of your own choosing)."
    ExitApp 1
}

catalogDir := benchRoot "\lrc-bench"
DirCreate(catalogDir)
catalogPath := catalogDir "\auto-tone.lrcat"

if FileExist(catalogPath) {
    result := MsgBox("Catalog already exists:`n" catalogPath "`n`nDelete and recreate?", "auto-tone.ahk", "YesNo")
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
Sleep(1500)

if WinWait("Create Folder with Catalog", , 10) {
    WinActivate("Create Folder with Catalog")
    Send("^a")
    SendText(catalogPath)
    Send("{Enter}")
} else {
    MsgBox "New-catalog dialog didn't appear as expected -- create '" catalogPath "' manually via File > New Catalog, then re-run with an existing empty catalog open."
    ExitApp 1
}

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
MsgBox "Import dialog is open. Navigate to:`n" sourceDir "`n`nSelect all files, choose 'Add' (not Copy/Move), then click Import.`n`nClick OK here once the import has finished."

WinActivate("ahk_exe Lightroom.exe")
Sleep(1000)

; Select all (Library grid) then apply Auto Settings to the whole selection.
Send("^a")
Sleep(500)
Send("^u")

MsgBox "Auto Settings applied to the whole selection.`n`nWait for LRC's develop-settings write to finish (watch the activity spinner), then click OK -- this closes without saving XMP, since #202 reads the result straight out of:`n" catalogPath
ExitApp 0
