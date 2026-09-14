# PowerShell install script for koshi

$ErrorActionPreference = "Stop"

# Release version to install
$release_version = "v0.4.0"

Write-Host "Installing koshi version: $release_version" -ForegroundColor Cyan

# Detect architecture
if ($env:PROCESSOR_ARCHITECTURE -eq "AMD64") {
    $architecture_slug = "amd64"
} elseif ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    $architecture_slug = "arm64"
} else {
    Write-Error "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE"
    exit 1
}

Write-Host "Detected architecture: $architecture_slug" -ForegroundColor Gray

# Construct download URL
# Naming convention: koshi-v{version}-windows-{arch}.zip
$release_version_number = $release_version -replace "^v", ""
$archive_file_name = "koshi-v$release_version_number-windows-$architecture_slug.zip"
$release_archive_url = "https://github.com/gohyuhan/koshi/releases/download/$release_version/$archive_file_name"

Write-Host "Download URL: $release_archive_url" -ForegroundColor Gray

# Staging paths
$staging_directory = [System.IO.Path]::GetTempPath()
$archive_path = Join-Path $staging_directory $archive_file_name

# Download
Write-Host "Downloading..." -ForegroundColor Cyan
try {
    Invoke-WebRequest -Uri $release_archive_url -OutFile $archive_path
} catch {
    Write-Error "Failed to download: $_"
    exit 1
}

# Installation directory
$installation_directory = Join-Path $env:LOCALAPPDATA "koshi"
if (-not (Test-Path $installation_directory)) {
    New-Item -ItemType Directory -Path $installation_directory | Out-Null
}

# Extract to a staging directory, so the binary is placed at the install root
# regardless of how the archive nests it — and an upgrade never leaves an old
# root binary shadowing a newly-extracted nested one.
$extraction_directory = Join-Path $staging_directory "koshi-extract-$PID"
if (Test-Path $extraction_directory) { Remove-Item $extraction_directory -Recurse -Force }
Write-Host "Extracting..." -ForegroundColor Cyan
Expand-Archive -Path $archive_path -DestinationPath $extraction_directory -Force

# Cleanup the archive
Remove-Item $archive_path -ErrorAction SilentlyContinue

# Place the binary at the install root, wherever it landed in the archive.
$binary_file = Get-ChildItem -Path $extraction_directory -Filter "koshi.exe" -Recurse | Select-Object -First 1
if (-not $binary_file) {
    Write-Error "Binary 'koshi.exe' not found in extracted files."
    Remove-Item $extraction_directory -Recurse -Force -ErrorAction SilentlyContinue
    exit 1
}
$binary_path = Join-Path $installation_directory "koshi.exe"
Move-Item $binary_file.FullName $binary_path -Force
Remove-Item $extraction_directory -Recurse -Force -ErrorAction SilentlyContinue

Write-Host "Installed to: $binary_path" -ForegroundColor Green

# Add to PATH
$user_path = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
if ($user_path -notlike "*$installation_directory*") {
    Write-Host "Adding to PATH..." -ForegroundColor Cyan
    $updated_user_path = "$user_path;$installation_directory"
    [Environment]::SetEnvironmentVariable("Path", $updated_user_path, [EnvironmentVariableTarget]::User)
    $env:Path = "$env:Path;$installation_directory" # Update current session
    Write-Host "Added to PATH. You may need to restart your terminal." -ForegroundColor Yellow
} else {
    Write-Host "Already in PATH." -ForegroundColor Gray
}

Write-Host "Installation complete! Run 'koshi --version' to verify." -ForegroundColor Green
