# Removes Insight and its data (index, media labels, downloaded model).
#   irm https://raw.githubusercontent.com/ehmoratallar8990/insightsapp/main/uninstall.ps1 | iex
Get-Process insight -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'Programs\Insight') -ErrorAction SilentlyContinue
Remove-Item -Force (Join-Path ([Environment]::GetFolderPath('Programs')) 'Insight.lnk') -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'insight') -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force (Join-Path $env:APPDATA 'Insight') -ErrorAction SilentlyContinue
Write-Host 'Removed Insight and its data.'
