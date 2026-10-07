# PowerShell install script for koshi

$ErrorActionPreference = "Stop"

# Release version to install
$release_version = "v0.5.0"

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

# Extract to a staging directory. Only koshi.exe moves from there to the
# install root, wherever the archive nests it.
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

# Rename the installed koshi.exe to the first backup name where no file is
# left: koshi.old, then koshi.1.old, koshi.2.old, and so on. A file at a backup
# name is removed first. A file that cannot be removed, such as a backup that a
# running koshi runs from, passes the search to the next name. Then the new
# koshi.exe moves into place. If that move fails, the backup is renamed back.
$backup_path = $null
try {
    if (Test-Path $binary_path) {
        $backup_index = 0
        while ($true) {
            if ($backup_index -eq 0) {
                $backup_name = "koshi.old"
            } else {
                $backup_name = "koshi.$backup_index.old"
            }
            $backup_path = Join-Path $installation_directory $backup_name
            if (Test-Path $backup_path -PathType Leaf) {
                Remove-Item $backup_path -Force -ErrorAction SilentlyContinue
            }
            if (-not (Test-Path $backup_path)) {
                break
            }
            $backup_index++
        }
        Move-Item $binary_path $backup_path
    }
    Move-Item $binary_file.FullName $binary_path
} catch {
    if ($backup_path -and (Test-Path $backup_path) -and -not (Test-Path $binary_path)) {
        Move-Item $backup_path $binary_path -ErrorAction SilentlyContinue
    }
    Remove-Item $extraction_directory -Recurse -Force -ErrorAction SilentlyContinue
    Write-Error "Failed to install koshi.exe: $_"
    exit 1
}
Remove-Item $extraction_directory -Recurse -Force -ErrorAction SilentlyContinue

# Remove every backup that no running koshi runs from.
Get-ChildItem -Path $installation_directory -File |
    Where-Object { $_.Name -cmatch '^koshi(\.[0-9]+)?\.old$' } |
    Remove-Item -Force -ErrorAction SilentlyContinue

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
