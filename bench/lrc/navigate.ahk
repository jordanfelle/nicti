#Requires AutoHotkey v2.0
; navigate.ahk - #43 hero-scenario navigation helper.
;
; Moves Lightroom Classic's Develop selection by a sequence of arrow-key presses, then exits.
; Exists so `run-hero-series.ps1` can position each series' warm-up/run (switch always rewinds to
; image 1) or each crop/zoom target image (the "5 images per run" spread) *before* the timed
; capture starts -- hero.ahk itself deliberately never navigates (see its own header comment), so
; navigation never lands inside a capture and never gets scored as a spurious switch event.
;
; Usage: AutoHotkey64.exe navigate.ahk <spec> [delayMs] [lrcWindowTitle]
;   <spec>: comma-separated Left:N / Right:N segments, applied in order left-to-right,
;           e.g. "Left:49,Right:12" rewinds to image 1 then advances to image 13.
;   [delayMs]: settle delay between keypresses (default 80).
;   [lrcWindowTitle]: AHK window-title spec (default "ahk_exe Lightroom.exe").

#SingleInstance Force
SendMode "Event"
SetTitleMatchMode 2
SetKeyDelay -1

if A_Args.Length < 1 {
    MsgBox "Usage: navigate.ahk <spec> [delayMs] [lrcWindowTitle]"
    ExitApp 1
}
spec := A_Args[1]
delayMs := A_Args.Length >= 2 ? Integer(A_Args[2]) : 80
lrcTitle := A_Args.Length >= 3 ? A_Args[3] : "ahk_exe Lightroom.exe"

if !WinExist(lrcTitle) {
    MsgBox "Lightroom Classic window not found (matched against: " lrcTitle ")."
    ExitApp 1
}
WinActivate(lrcTitle)
Sleep(300) ; let focus land before the first keypress

for part in StrSplit(spec, ",") {
    kv := StrSplit(part, ":")
    if kv.Length != 2 {
        MsgBox "Bad navigate spec segment: '" part "' (expected Left:N or Right:N)"
        ExitApp 1
    }
    dir := Trim(kv[1])
    count := Integer(Trim(kv[2]))
    key := ""
    if dir = "Left"
        key := "{Left}"
    else if dir = "Right"
        key := "{Right}"
    else {
        MsgBox "Unknown navigate direction: '" dir "' (expected Left or Right)"
        ExitApp 1
    }
    Loop count {
        Send(key)
        Sleep(delayMs)
    }
}

ExitApp 0
