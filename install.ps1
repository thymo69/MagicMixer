# Builds Magic Mixer and installs it for the current user (no admin needed):
#   %LOCALAPPDATA%\Programs\MagicMixer\  (exes + icon)
#   Start menu and Desktop shortcuts "Magic Mixer"
# Re-run after pulling changes to update. Run: powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$target = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
$dest = Join-Path $env:LOCALAPPDATA 'Programs\MagicMixer'
New-Item -ItemType Directory -Force $dest | Out-Null

# Close a running copy so its exe can be replaced.
Get-Process magic-mixer-gui -ErrorAction SilentlyContinue | Stop-Process
Start-Sleep -Milliseconds 500

Copy-Item "$target\release\magic-mixer-gui.exe", "$target\release\magic-mixer.exe" $dest -Force
& "$dest\magic-mixer.exe" make-icon "$dest\MagicMixer.ico"

$shell = New-Object -ComObject WScript.Shell
$places = @(
    [Environment]::GetFolderPath('Programs'),        # Start menu
    [Environment]::GetFolderPath('Desktop')
)
foreach ($dir in $places) {
    $lnk = $shell.CreateShortcut((Join-Path $dir 'Magic Mixer.lnk'))
    $lnk.TargetPath = "$dest\magic-mixer-gui.exe"
    $lnk.WorkingDirectory = $dest
    $lnk.IconLocation = "$dest\MagicMixer.ico,0"
    $lnk.Description = 'Send any app''s sound anywhere'
    $lnk.Save()
    Write-Host "Shortcut: $(Join-Path $dir 'Magic Mixer.lnk')"
}
Write-Host "Installed to $dest"
