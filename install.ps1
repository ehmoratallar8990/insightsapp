# Insight installer for Windows 10/11 (no admin needed).
#   irm https://raw.githubusercontent.com/ehmoratallar8990/insightsapp/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'ehmoratallar8990/insightsapp'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\Insight'
$zip = Join-Path $env:TEMP 'Insight-Windows.zip'
Write-Host '> Downloading Insight for Windows...' -ForegroundColor Magenta
Invoke-WebRequest "https://github.com/$repo/releases/latest/download/Insight-Windows-x86_64.zip" -OutFile $zip
Get-Process insight -ErrorAction SilentlyContinue | Stop-Process -Force
New-Item -ItemType Directory -Force $dest | Out-Null
Expand-Archive $zip -DestinationPath $dest -Force
Remove-Item $zip
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'Insight.lnk'
$sh = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$sh.TargetPath = Join-Path $dest 'Insight.exe'
$sh.Save()
Write-Host "> Installed to $dest (Start menu: Insight)" -ForegroundColor Magenta
Start-Process (Join-Path $dest 'Insight.exe')
