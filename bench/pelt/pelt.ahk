#Requires AutoHotkey v2.0
; pelt.ahk - #68 (ADR-0006) GUI-framework candidate input driver.
;
; Drives one timed pass (grid scroll, loupe next/prev, slider drag, or viewport pan) against a
; running pelt-egui/pelt-iced/pelt-slint window. Same keypress-indicator-square technique as
; bench/lrc/hero.ahk (see its own comments for why the Flash() call is non-blocking), reused
; unchanged so bench/whisker's analysis needs no candidate-specific logic.
;
; Usage: AutoHotkey64.exe pelt.ahk <config.ini>
; See pelt-config.ini.example for all keys.

#SingleInstance Force
SendMode "Event"
SetTitleMatchMode 2
SetKeyDelay -1
SetMouseDelay -1

if A_Args.Length < 1 {
    MsgBox "Usage: pelt.ahk <config.ini>"
    ExitApp 1
}
configPath := A_Args[1]
if !FileExist(configPath) {
    MsgBox "Config file not found: " configPath
    ExitApp 1
}

candidate := IniRead(configPath, "general", "Candidate")
interaction := IniRead(configPath, "general", "Interaction")
preStartDelayMs := Integer(IniRead(configPath, "general", "PreStartDelayMs", "500"))
windowTitle := "pelt-" candidate

indicatorX := Integer(IniRead(configPath, "indicator", "X"))
indicatorY := Integer(IniRead(configPath, "indicator", "Y"))
indicatorSize := Integer(IniRead(configPath, "indicator", "Size"))
indicatorFlashMs := Integer(IniRead(configPath, "indicator", "FlashMs", "80"))

indicatorRect := Format("x{} y{} w{} h{}", indicatorX, indicatorY, indicatorSize, indicatorSize)

blackGui := Gui("+AlwaysOnTop -Caption +ToolWindow +E0x08000000", "whisker-indicator-black")
blackGui.BackColor := "000000"
blackGui.Show(indicatorRect . " NoActivate")

whiteGui := Gui("+AlwaysOnTop -Caption +ToolWindow +E0x08000000", "whisker-indicator-white")
whiteGui.BackColor := "FFFFFF"
whiteGui.Show(indicatorRect . " Hide NoActivate")

Flash(durationMs) {
    global whiteGui
    whiteGui.Show("NoActivate")
    SetTimer(HideIndicator, -durationMs)
}

HideIndicator() {
    global whiteGui
    whiteGui.Hide()
}

if !WinExist(windowTitle) {
    MsgBox "Window not found (matched against: " windowTitle "). Launch that pelt-* binary first."
    ExitApp 1
}
WinActivate(windowTitle)
Sleep(preStartDelayMs)

if interaction = "loupe" {
    imageCount := Integer(IniRead(configPath, "loupe", "ImageCount", "50"))
    interKeyDelayMs := Integer(IniRead(configPath, "loupe", "InterKeyDelayMs", "700"))

    Loop imageCount - 1 {
        Flash(indicatorFlashMs)
        Send("{Right}")
        Sleep(interKeyDelayMs)
    }
} else if interaction = "grid" {
    startX := Integer(IniRead(configPath, "grid", "StartX"))
    startY := Integer(IniRead(configPath, "grid", "StartY"))
    durationMs := Integer(IniRead(configPath, "grid", "DurationMs", "4000"))
    steps := Integer(IniRead(configPath, "grid", "Steps", "120"))
    wheelClicks := Integer(IniRead(configPath, "grid", "WheelClicks", "40"))

    MouseMove(startX, startY, 0)
    Flash(indicatorFlashMs)
    stepDelay := durationMs / steps
    clicksPerStep := Max(1, Round(wheelClicks / steps))
    Loop steps {
        Send("{WheelDown " clicksPerStep "}")
        Sleep(stepDelay)
    }
} else if interaction = "slider" {
    startX := Integer(IniRead(configPath, "slider", "StartX"))
    startY := Integer(IniRead(configPath, "slider", "StartY"))
    endX := Integer(IniRead(configPath, "slider", "EndX"))
    endY := Integer(IniRead(configPath, "slider", "EndY"))
    durationMs := Integer(IniRead(configPath, "slider", "DurationMs", "2000"))
    steps := Integer(IniRead(configPath, "slider", "Steps", "60"))

    Flash(indicatorFlashMs)
    ScriptedDrag(startX, startY, endX, endY, durationMs, steps)
} else if interaction = "pan" {
    panStartX := Integer(IniRead(configPath, "pan", "PanStartX"))
    panStartY := Integer(IniRead(configPath, "pan", "PanStartY"))
    panEndX := Integer(IniRead(configPath, "pan", "PanEndX"))
    panEndY := Integer(IniRead(configPath, "pan", "PanEndY"))
    durationMs := Integer(IniRead(configPath, "pan", "DurationMs", "2000"))
    steps := Integer(IniRead(configPath, "pan", "Steps", "60"))

    Flash(indicatorFlashMs)
    ScriptedDrag(panStartX, panStartY, panEndX, panEndY, durationMs, steps)
} else {
    MsgBox "Unknown Interaction: " interaction " (expected grid, loupe, slider, or pan)"
    ExitApp 1
}

Sleep(500)
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
