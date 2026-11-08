# Installs the wsblast release binary on a Windows runner for the GitHub
# Action in action.yml, after checking its SHA-256 against the release's.
#   pwsh -File install.ps1 -Version v0.2.5 -Destination C:\path\to\bin
# An empty -Version installs the latest release.
param(
    [string]$Version = "",
    [Parameter(Mandatory = $true)][string]$Destination
)
$ErrorActionPreference = "Stop"
$Repo = "jephter-olamiposi/wsblast"
$Asset = "wsblast-x86_64-pc-windows-msvc.zip"

if (-not $Version) {
    # The latest release page redirects to its tag, without the API's rate limit.
    $latest = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" `
        -MaximumRedirection 0 -SkipHttpErrorCheck -UseBasicParsing
    $location = [string]$latest.Headers.Location
    if ($location -notmatch "/releases/tag/(?<tag>[^/]+)$") {
        throw "could not find the latest wsblast release; set the action's version input"
    }
    $Version = $Matches.tag
}

$base = "https://github.com/$Repo/releases/download/$Version/$Asset"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("wsblast-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $zip = Join-Path $work $Asset
    Write-Host "Downloading wsblast $Version for x86_64-pc-windows-msvc..."
    Invoke-WebRequest -Uri $base -OutFile $zip -UseBasicParsing
    Invoke-WebRequest -Uri "$base.sha256" -OutFile "$zip.sha256" -UseBasicParsing
    $expected = ((Get-Content "$zip.sha256" -Raw).Trim() -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
    if (-not $expected -or $expected -ne $actual) {
        throw "SHA-256 mismatch for ${Asset}: expected $expected, got $actual; refusing to install"
    }
    Write-Host "Checksum verified (SHA-256 $actual)."
    Expand-Archive -Path $zip -DestinationPath (Join-Path $work "x")
    $exe = Get-ChildItem -Path (Join-Path $work "x") -Recurse -Filter "wsblast.exe" | Select-Object -First 1
    if (-not $exe) {
        throw "$Asset does not contain wsblast.exe; refusing to install"
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    Copy-Item $exe.FullName (Join-Path $Destination "wsblast.exe") -Force
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
