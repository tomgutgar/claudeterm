# Installs claudeterm on Windows: builds `cct`, copies the icon and puts two
# desktop shortcuts that open it in Windows Terminal: the full dashboard and
# the compact mode (small always-on-top window).
#   powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = 'Stop'

cargo install --path $PSScriptRoot
if ($LASTEXITCODE -ne 0) { throw 'cargo install failed' }

# The icon lives outside the repo, so the shortcut keeps working if the repo moves or is deleted.
$dir = Join-Path $env:LOCALAPPDATA 'claudeterm'
New-Item -ItemType Directory -Force $dir | Out-Null
$ico = Join-Path $dir 'logo.ico'
Copy-Item (Join-Path $PSScriptRoot 'assets\logo.ico') $ico -Force

$cct = Join-Path $env:USERPROFILE '.cargo\bin\cct.exe'
$wt = Get-Command wt.exe -ErrorAction SilentlyContinue

function Shortcut($name, $wtArgs, $cctArgs, $desc) {
    # GetFolderPath honours a Desktop moved to OneDrive.
    $lnk = Join-Path ([Environment]::GetFolderPath('Desktop')) "$name.lnk"
    $s = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
    if ($wt) {
        $s.TargetPath = $wt.Source
        # "Command Prompt" profile (same GUID on every install), the one new
        # sessions open with: same colours and background.
        $s.Arguments = "$wtArgs -p {0caa0dad-35be-5f56-a8ff-afceeeaa6101} --title `"$name`" `"$cct`" $cctArgs"
    } else {
        $s.TargetPath = $cct  # no Windows Terminal: the classic console
        $s.Arguments = $cctArgs
    }
    $s.WorkingDirectory = $env:USERPROFILE
    $s.IconLocation = "$ico,0"
    $s.Description = $desc
    $s.Save()
    Write-Host "Created: $lnk"
}

Shortcut 'claudeterm' '' '' 'Claude Code dashboard'
Shortcut 'claudeterm mini' '-w new --size 48,6' '--mini' 'Claude Code usage (5 h and weekly), always on top'
