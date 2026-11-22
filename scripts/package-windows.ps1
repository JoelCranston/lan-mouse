# Stage a self-contained Windows build of Lan Mouse: lan-mouse.exe plus the
# GTK runtime it needs, so it runs without a GTK install on PATH.
#
# Used by the release workflow for the .zip and the installer
# (build-aux/windows/lan-mouse.iss), and works the same locally:
#
#   cargo build --release
#   .\scripts\package-windows.ps1
param(
    [string]$GtkDir = "C:\gtk-build\gtk\x64\release",
    [string]$Exe = "target\release\lan-mouse.exe",
    [string]$OutDir = "target\lan-mouse-windows"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Exe)) { throw "missing $Exe; run cargo build --release first" }
if (-not (Test-Path "$GtkDir\bin")) { throw "missing GTK build at $GtkDir" }

if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir }
New-Item -ItemType Directory -Force $OutDir | Out-Null

Copy-Item $Exe $OutDir
Get-ChildItem -Path "$GtkDir\bin\*.dll" -File -Recurse | Copy-Item -Destination $OutDir

# gdk-pixbuf image loaders (SVG via librsvg) for the app and tray icons.
# GLib resolves the loaders.cache entries relative to the folder holding the
# DLLs, so the lib\ tree sits next to lan-mouse.exe.
$pixbuf = "$GtkDir\lib\gdk-pixbuf-2.0"
if (Test-Path $pixbuf) {
    New-Item -ItemType Directory -Force "$OutDir\lib" | Out-Null
    Copy-Item -Recurse $pixbuf "$OutDir\lib\"
} else {
    Write-Warning "no gdk-pixbuf loaders at $pixbuf; SVG icons will not render"
}

Write-Host "staged $((Get-ChildItem -Recurse -File $OutDir).Count) files in $OutDir"
