# PowerShell install script for koshi

# Every error stops the script, Write-Error included.
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
}

Write-Host "Detected architecture: $architecture_slug" -ForegroundColor Gray

# Construct download URL
# Naming convention: koshi-v{version}-windows-{arch}.zip
$release_version_number = $release_version -replace "^v", ""
$archive_file_name = "koshi-v$release_version_number-windows-$architecture_slug.zip"
$release_archive_url = "https://github.com/gohyuhan/koshi/releases/download/$release_version/$archive_file_name"

Write-Host "Download URL: $release_archive_url" -ForegroundColor Gray

# Staging paths. Both names hold the process id of this run. The finally block
# at the end removes both when the install succeeds, fails, or is stopped.
$staging_directory = [System.IO.Path]::GetTempPath()
$archive_path = Join-Path $staging_directory "koshi-$PID-$archive_file_name"
$extraction_directory = Join-Path $staging_directory "koshi-extract-$PID"

# Installation paths. The new koshi.exe waits beside the installed one as
# koshi-update-<process id of this run>.exe until it replaces it. The finally
# block at the end removes that copy when the install stops before then.
$installation_directory = Join-Path $env:LOCALAPPDATA "koshi"
$binary_path = Join-Path $installation_directory "koshi.exe"
$staged_binary_path = Join-Path $installation_directory "koshi-update-$PID.exe"
$install_lock_path = Join-Path $installation_directory "koshi.lock"

$install_lock = $null
$backup_path = $null
try {
    # Download
    Write-Host "Downloading..." -ForegroundColor Cyan
    try {
        Invoke-WebRequest -Uri $release_archive_url -OutFile $archive_path
    } catch {
        Write-Error "Failed to download: $_"
    }

    if (-not (Test-Path $installation_directory)) {
        New-Item -ItemType Directory -Path $installation_directory | Out-Null
    }

    # Extract to a staging directory. Only koshi.exe moves from there to the
    # install root, wherever the archive nests it.
    if (Test-Path $extraction_directory) { Remove-Item $extraction_directory -Recurse -Force }
    Write-Host "Extracting..." -ForegroundColor Cyan
    Expand-Archive -Path $archive_path -DestinationPath $extraction_directory -Force

    # Place the binary at the install root, wherever it landed in the archive.
    $binary_file = Get-ChildItem -Path $extraction_directory -Filter "koshi.exe" -Recurse | Select-Object -First 1
    if (-not $binary_file) {
        Write-Error "Binary 'koshi.exe' not found in extracted files."
    }

    # Move the new koshi.exe to $staged_binary_path, replacing a copy that an
    # earlier run with the same process id left there, and run it with
    # --version. A first output line other than "koshi <release version
    # number>" stops the install, and the installed koshi.exe stays as it was.
    Move-Item -Force $binary_file.FullName $staged_binary_path
    $new_version_line = & $staged_binary_path --version | Select-Object -First 1
    if ("$new_version_line" -ne "koshi $release_version_number") {
        Write-Error "The new koshi printed '$new_version_line' for --version, not 'koshi $release_version_number'"
    }

    # Take the install lock: an exclusive lock on byte 0 of koshi.lock beside
    # koshi.exe. koshi update and the backup removal at each launch of a koshi
    # newer than 0.5.0 lock the same file. While another process holds the
    # lock, the lock is tried again every 200 ms. The finally block at the end
    # releases it.
    try {
        $install_lock = [System.IO.File]::Open($install_lock_path, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
    } catch {
        Write-Error "Failed to open ${install_lock_path}: $_"
    }
    $is_wait_announced = $false
    while ($true) {
        try {
            $install_lock.Lock(0, 1)
            break
        } catch [System.IO.IOException] {
            # HResult -2147024863 is 0x80070021, ERROR_LOCK_VIOLATION: another
            # process holds the lock. Every other failure stops the install.
            if ($_.Exception.GetBaseException().HResult -ne -2147024863) {
                Write-Error "Failed to lock ${install_lock_path}: $_"
            }
            if (-not $is_wait_announced) {
                Write-Host "Waiting while another koshi install holds $install_lock_path" -ForegroundColor Yellow
                $is_wait_announced = $true
            }
            Start-Sleep -Milliseconds 200
        }
    }

    # Rename the installed koshi.exe to the first backup name where no file is
    # left: koshi.old, then koshi.1.old, koshi.2.old, and so on. A file at a
    # backup name is removed first. A file that cannot be removed, such as a
    # backup that a running koshi runs from, passes the search to the next
    # name. Then the new koshi.exe moves into place.
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
        Move-Item $staged_binary_path $binary_path
    } catch {
        Write-Error "Failed to install koshi.exe: $_"
    }

    # Remove every backup that no running koshi runs from.
    Get-ChildItem -Path $installation_directory -File |
        Where-Object { $_.Name -cmatch '^koshi(\.[0-9]+)?\.old$' } |
        Remove-Item -Force -ErrorAction SilentlyContinue

    Write-Host "Installed to: $binary_path" -ForegroundColor Green

    # Add to PATH, unless an entry of the user PATH names the installation
    # directory, with or without a trailing backslash.
    $user_path = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
    $user_path_entries = "$user_path" -split ";"
    if (($user_path_entries -notcontains $installation_directory) -and ($user_path_entries -notcontains "$installation_directory\")) {
        Write-Host "Adding to PATH..." -ForegroundColor Cyan
        $updated_user_path = "$user_path;$installation_directory"
        [Environment]::SetEnvironmentVariable("Path", $updated_user_path, [EnvironmentVariableTarget]::User)
        $env:Path = "$env:Path;$installation_directory" # Update current session
        Write-Host "Added to PATH. You may need to restart your terminal." -ForegroundColor Yellow
    } else {
        Write-Host "Already in PATH." -ForegroundColor Gray
    }
} finally {
    # A koshi.exe that was renamed to a backup, with no new koshi.exe in its
    # place, is renamed back.
    if ($backup_path -and -not (Test-Path $binary_path) -and (Test-Path $backup_path)) {
        Move-Item $backup_path $binary_path -ErrorAction SilentlyContinue
    }
    # Closing koshi.lock releases the install lock.
    if ($install_lock) {
        $install_lock.Dispose()
    }
    Remove-Item $staged_binary_path -ErrorAction SilentlyContinue
    Remove-Item $archive_path -ErrorAction SilentlyContinue
    Remove-Item $extraction_directory -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Installation complete! Run 'koshi --version' to verify." -ForegroundColor Green
