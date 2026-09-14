#!/bin/bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NO_COLOR='\033[0m' # No Color

write_installation_message() {
  echo -e "${BLUE}[INFO]${NO_COLOR} $1"
}

log_success() {
  echo -e "${GREEN}[SUCCESS]${NO_COLOR} $1"
}

log_error() {
  echo -e "${RED}[ERROR]${NO_COLOR} $1"
}

# Detect OS
operating_system_name="$(uname -s)"
case "${operating_system_name}" in
Linux*) operating_system_slug=linux ;;
Darwin*) operating_system_slug=darwin ;;
*)
  log_error "Unsupported operating system: ${operating_system_name}"
  exit 1
  ;;
esac

# Detect Architecture
architecture_name="$(uname -m)"
case "${architecture_name}" in
x86_64) architecture_slug=amd64 ;;
arm64) architecture_slug=arm64 ;;
aarch64) architecture_slug=arm64 ;;
*)
  log_error "Unsupported architecture: ${architecture_name}"
  exit 1
  ;;
esac

write_installation_message "Detected OS: ${operating_system_slug}, Architecture: ${architecture_slug}"

# Version to install (bump before each release)
release_version="v0.4.0"

write_installation_message "Installing koshi version: ${release_version}"

# Construct download URL
# Naming convention: koshi-v{version}-{os}-{arch}.tar.gz
release_version_number="${release_version#v}"
archive_file_name="koshi-v${release_version_number}-${operating_system_slug}-${architecture_slug}.tar.gz"
release_archive_url="https://github.com/gohyuhan/koshi/releases/download/${release_version}/${archive_file_name}"

write_installation_message "Download URL: ${release_archive_url}"

# Create temp directory
staging_directory=$(mktemp -d)
trap 'rm -rf "${staging_directory}"' EXIT

# Download
write_installation_message "Downloading ${archive_file_name}..."
curl -sL "${release_archive_url}" -o "${staging_directory}/${archive_file_name}"

# Extract
write_installation_message "Extracting..."
tar -xzf "${staging_directory}/${archive_file_name}" -C "${staging_directory}"

# Find binary
binary_path="${staging_directory}/koshi"
if [ ! -f "${binary_path}" ]; then
  binary_path=$(find "${staging_directory}" -type f -name "koshi" | head -n 1)
fi

if [ ! -f "${binary_path}" ]; then
  log_error "Binary 'koshi' not found in extracted archive."
  exit 1
fi

# Install
installation_directory="/usr/local/bin"
installed_binary_path="${installation_directory}/koshi"

write_installation_message "Installing to ${installed_binary_path}..."

# Ensure the install directory exists (sudo if we cannot create it ourselves),
# so a fresh machine without /usr/local/bin does not fail the move below.
if [ ! -d "${installation_directory}" ]; then
  mkdir -p "${installation_directory}" 2>/dev/null || sudo mkdir -p "${installation_directory}"
fi

if [ -w "${installation_directory}" ]; then
  mv "${binary_path}" "${installed_binary_path}"
  chmod +x "${installed_binary_path}"
else
  write_installation_message "Requires sudo to install to ${installation_directory}"
  sudo mv "${binary_path}" "${installed_binary_path}"
  sudo chmod +x "${installed_binary_path}"
fi

log_success "koshi installed successfully!"
write_installation_message "Run 'koshi --version' to verify."
