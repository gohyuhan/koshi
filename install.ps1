# PowerShell install script for koshi

$ErrorActionPreference = "Stop"

# Version to install (bump before each release)
$Version = "v0.1.0"

Write-Host "Installing koshi version: $Version" -ForegroundColor Cyan

# Detect Architecture
if ($env:PROCESSOR_ARCHITECTURE -eq "AMD64") {
    $Arch = "amd64"
} elseif ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    $Arch = "arm64"
} else {
    Write-Error "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE"
    exit 1
}

Write-Host "Detected Architecture: $Arch" -ForegroundColor Gray

# Construct URL
# Naming convention: koshi-v{version}-windows-{arch}.zip
$VersionNum = $Version -replace "^v", ""
$FileName = "koshi-v$VersionNum-windows-$Arch.zip"
$DownloadUrl = "https://github.com/gohyuhan/koshi/releases/download/$Version/$FileName"

Write-Host "Download URL: $DownloadUrl" -ForegroundColor Gray

# Temp paths
$TempDir = [System.IO.Path]::GetTempPath()
$ZipPath = Join-Path $TempDir $FileName

# Download
Write-Host "Downloading..." -ForegroundColor Cyan
try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath
} catch {
    Write-Error "Failed to download: $_"
    exit 1
}

# Install Directory
$InstallDir = Join-Path $env:LOCALAPPDATA "koshi"
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir | Out-Null
}

# Extract to a scratch directory, so the binary is placed at the install root
# regardless of how the archive nests it — and an upgrade never leaves an old
# root binary shadowing a newly-extracted nested one.
$ExtractDir = Join-Path $TempDir "koshi-extract-$PID"
if (Test-Path $ExtractDir) { Remove-Item $ExtractDir -Recurse -Force }
Write-Host "Extracting..." -ForegroundColor Cyan
Expand-Archive -Path $ZipPath -DestinationPath $ExtractDir -Force

# Cleanup the archive
Remove-Item $ZipPath -ErrorAction SilentlyContinue

# Place the binary at the install root, wherever it landed in the archive.
$Found = Get-ChildItem -Path $ExtractDir -Filter "koshi.exe" -Recurse | Select-Object -First 1
if (-not $Found) {
    Write-Error "Binary 'koshi.exe' not found in extracted files."
    Remove-Item $ExtractDir -Recurse -Force -ErrorAction SilentlyContinue
    exit 1
}
$BinaryPath = Join-Path $InstallDir "koshi.exe"
Move-Item $Found.FullName $BinaryPath -Force
Remove-Item $ExtractDir -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "Installed to: $BinaryPath" -ForegroundColor Green

# Add to PATH
$UserPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
if ($UserPath -notlike "*$InstallDir*") {
    Write-Host "Adding to PATH..." -ForegroundColor Cyan
    $NewPath = "$UserPath;$InstallDir"
    [Environment]::SetEnvironmentVariable("Path", $NewPath, [EnvironmentVariableTarget]::User)
    $env:Path = "$env:Path;$InstallDir" # Update current session
    Write-Host "Added to PATH. You may need to restart your terminal." -ForegroundColor Yellow
} else {
    Write-Host "Already in PATH." -ForegroundColor Gray
}

Write-Host "Installation complete! Run 'koshi --version' to verify." -ForegroundColor Green
