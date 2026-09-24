#Requires AutoHotkey v2.0
; hero.ahk - #43 hero-scenario input driver.
;
; Drives one timed pass (switch, crop, or zoom) against a Lightroom Classic Develop-module
; session that's already set up per README.md (bench catalog loaded, 50-image hero set synced
; with the edit stack, first pass through the set already done to warm it). Draws a
; keypress-indicator square that flashes on every injected input so whisker's capture analysis
; has an unambiguous t0 per event. Never navigates between images -- see navigate.ahk for that.
;
; Indicator-edge count per interaction (whisker's `events_detected`/`--edges`/`--window-edges`
; reference these positions): switch = 1 flash per keypress; crop/zoom = 3 flashes, [0] the
; mode-entry keypress (r/Z), [1]/[2] the drag's start/end (see ScriptedDrag).
;
; Usage: AutoHotkey64.exe hero.ahk <config.ini>
; See hero-config.ini.example for all keys.

#SingleInstance Force
SendMode "Event"
SetTitleMatchMode 2
SetKeyDelay -1
SetMouseDelay -1

if A_Args.Length < 1 {
    MsgBox "Usage: hero.ahk <config.ini>"
    ExitApp 1
}
configPath := A_Args[1]
if !FileExist(configPath) {
    MsgBox "Config file not found: " configPath
    ExitApp 1
}

interaction := IniRead(configPath, "general", "Interaction")
lrcTitle := IniRead(configPath, "general", "LrcWindowTitle", "ahk_exe Lightroom.exe")
preStartDelayMs := Integer(IniRead(configPath, "general", "PreStartDelayMs", "500"))

indicatorX := Integer(IniRead(configPath, "indicator", "X"))
indicatorY := Integer(IniRead(configPath, "indicator", "Y"))
indicatorSize := Integer(IniRead(configPath, "indicator", "Size"))
indicatorFlashMs := Integer(IniRead(configPath, "indicator", "FlashMs", "80"))

; --- Indicator: two overlapping always-on-top windows (black base, white flash) toggled via
; Show/Hide. More reliable across capture pipelines than trying to force a redraw after changing
; a control's background color. ---
indicatorRect := Format("x{} y{} w{} h{}", indicatorX, indicatorY, indicatorSize, indicatorSize)

blackGui := Gui("+AlwaysOnTop -Caption +ToolWindow +E0x08000000", "whisker-indicator-black") ; WS_EX_NOACTIVATE
blackGui.BackColor := "000000"
blackGui.Show(indicatorRect . " NoActivate")

whiteGui := Gui("+AlwaysOnTop -Caption +ToolWindow +E0x08000000", "whisker-indicator-white")
whiteGui.BackColor := "FFFFFF"
whiteGui.Show(indicatorRect . " Hide NoActivate")

; Non-blocking: shows the indicator and returns immediately (the auto-hide is scheduled via a
; one-shot timer instead of Sleep()-ing here) so the caller's very next statement -- the actual
; injected input -- fires at essentially the same instant as the indicator's rising edge. A
; blocking Flash() (show, Sleep(durationMs), hide, THEN return) would delay every injected input
; by durationMs relative to the indicator, inflating every latency whisker reports by that amount.
Flash(durationMs) {
    global whiteGui
    whiteGui.Show("NoActivate")
    SetTimer(HideIndicator, -durationMs)
}

HideIndicator() {
    global whiteGui
    whiteGui.Hide()
}

if !WinExist(lrcTitle) {
    MsgBox "Lightroom Classic window not found (matched against: " lrcTitle "). Launch it against the bench catalog first."
    ExitApp 1
}
WinActivate(lrcTitle)
Sleep(preStartDelayMs)

if interaction = "switch" {
    imageCount := Integer(IniRead(configPath, "switch", "ImageCount", "50"))
    interKeyDelayMs := Integer(IniRead(configPath, "switch", "InterKeyDelayMs", "700"))

    Loop imageCount - 1 {
        Flash(indicatorFlashMs)
        Send("{Right}")
        ; Flash() now returns immediately (see its definition) and hides itself on its own timer,
        ; so the full InterKeyDelayMs elapses here -- not InterKeyDelayMs minus the flash duration.
        Sleep(interKeyDelayMs)
    }
} else if interaction = "crop" {
    startX := Integer(IniRead(configPath, "crop", "StartX"))
    startY := Integer(IniRead(configPath, "crop", "StartY"))
    endX := Integer(IniRead(configPath, "crop", "EndX"))
    endY := Integer(IniRead(configPath, "crop", "EndY"))
    durationMs := Integer(IniRead(configPath, "crop", "DurationMs", "2000"))
    steps := Integer(IniRead(configPath, "crop", "Steps", "60"))
    exitCropAfter := IniRead(configPath, "crop", "ExitCropAfter", "1") = "1"
    revertCropAfter := IniRead(configPath, "crop", "RevertCropAfter", "1") = "1"

    Flash(indicatorFlashMs)
    Send("r")
    Sleep(300) ; let crop mode's overlay settle before the drag itself is timed
    ScriptedDrag(startX, startY, endX, endY, durationMs, steps)
    if exitCropAfter
        Send("{Enter}")
    ; Every repeat run on the same image must start from the same uncropped state and the same
    ; drag coordinates -- without this, run 2+ would drag against an already-cropped frame,
    ; silently invalidating both the crop geometry and every subsequent measurement on that image.
    if revertCropAfter
        Send("^z")
} else if interaction = "zoom" {
    panStartX := Integer(IniRead(configPath, "zoom", "PanStartX"))
    panStartY := Integer(IniRead(configPath, "zoom", "PanStartY"))
    panEndX := Integer(IniRead(configPath, "zoom", "PanEndX"))
    panEndY := Integer(IniRead(configPath, "zoom", "PanEndY"))
    durationMs := Integer(IniRead(configPath, "zoom", "DurationMs", "2000"))
    steps := Integer(IniRead(configPath, "zoom", "Steps", "60"))
    settleWaitMs := Integer(IniRead(configPath, "zoom", "SettleWaitMs", "1000"))

    Flash(indicatorFlashMs)
    Send("z")
    Sleep(settleWaitMs) ; captures the zoom-settled event before the pan drag begins
    ScriptedDrag(panStartX, panStartY, panEndX, panEndY, durationMs, steps)
    Send("z") ; back out of 1:1 zoom
} else {
    MsgBox "Unknown Interaction: " interaction " (expected switch, crop, or zoom)"
    ExitApp 1
}

Sleep(500) ; trailing buffer so the capture has settled frames after the last event
ExitApp 0

; Flashes the indicator at drag-start and drag-end (in addition to the mode-entry flash each
; caller already sent before this runs), so whisker's `drag --indicator-raw --window-edges 1,2`
; can derive the drag transition window from indicator edges instead of hand-picked frame numbers
; -- see whisker::drag_window_from_edges.
ScriptedDrag(x1, y1, x2, y2, durationMs, steps) {
    global indicatorFlashMs
    MouseMove(x1, y1, 0)
    Sleep(100)
    Flash(indicatorFlashMs) ; edge: drag start
    Click("down")
    Sleep(50)
    stepDelay := durationMs / steps
    Loop steps {
        t := A_Index / steps
        x := Round(x1 + (x2 - x1) * t)
        y := Round(y1 + (y2 - y1) * t)
        MouseMove(x, y, 0)
        Sleep(stepDelay)
    }
    Sleep(50)
    Flash(indicatorFlashMs) ; edge: drag end
    Click("up")
}
