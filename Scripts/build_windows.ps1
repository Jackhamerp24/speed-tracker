param(
    [ValidateSet('win-x64', 'win-arm64')][string]$Runtime = 'win-x64',
    [string]$Output = ''
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
if (-not $Output) { $Output = Join-Path $Root "dist/SpeedTracker-$Runtime" }
$Output = [System.IO.Path]::GetFullPath($Output)
# Self-contained folders include runtime, SQLite and the explicitly optional collector.
dotnet publish (Join-Path $Root 'Windows/SpeedTracker.Windows/SpeedTracker.Windows.csproj') -c Release -r $Runtime --self-contained true -o $Output
if ($LASTEXITCODE -ne 0) { throw 'Windows desktop build failed' }
dotnet publish (Join-Path $Root 'Windows/SpeedTracker.Collector/SpeedTracker.Collector.csproj') -c Release -r $Runtime --self-contained true -o (Join-Path $Output 'collector')
if ($LASTEXITCODE -ne 0) { throw 'Optional network collector build failed' }
$Archive = "$Output.zip"
Compress-Archive -Path (Join-Path $Output '*') -DestinationPath $Archive -Force
Write-Output "Built $Archive"
