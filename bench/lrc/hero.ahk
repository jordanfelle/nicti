#Requires AutoHotkey v2.0
; hero.ahk - #43 hero-scenario input driver.
;
; Drives one timed pass (switch, crop, or zoom) against a Lightroom Classic Develop-module
; session that's already set up per README.md (bench catalog loaded, 50-image hero set synced
; with the edit stack, first pass through the set already done to warm it). Draws a
; keypress-indicator square that flashes on every injected input so whisker's capture analysis
; has an unambiguous t0 per event.
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

Flash(durationMs) {
    global whiteGui
    whiteGui.Show("NoActivate")
    Sleep(durationMs)
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
        Sleep(interKeyDelayMs - indicatorFlashMs)
    }
} else if interaction = "crop" {
    startX := Integer(IniRead(configPath, "crop", "StartX"))
    startY := Integer(IniRead(configPath, "crop", "StartY"))
    endX := Integer(IniRead(configPath, "crop", "EndX"))
    endY := Integer(IniRead(configPath, "crop", "EndY"))
    durationMs := Integer(IniRead(configPath, "crop", "DurationMs", "2000"))
    steps := Integer(IniRead(configPath, "crop", "Steps", "60"))
    exitCropAfter := IniRead(configPath, "crop", "ExitCropAfter", "1") = "1"

    Flash(indicatorFlashMs)
    Send("r")
    Sleep(300) ; let crop mode's overlay settle before the drag itself is timed
    ScriptedDrag(startX, startY, endX, endY, durationMs, steps)
    if exitCropAfter
        Send("{Enter}")
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

ScriptedDrag(x1, y1, x2, y2, durationMs, steps) {
    MouseMove(x1, y1, 0)
    Sleep(100)
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
    Click("up")
}
