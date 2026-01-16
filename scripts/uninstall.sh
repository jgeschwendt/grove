#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${HOME}/.grove"

echo "grove Uninstaller"
echo ""
echo "This will remove:"
echo "  • ${INSTALL_DIR}/"
echo "  • PATH entry from shell config"
echo ""
echo "WARNING: All cloned repositories in ${INSTALL_DIR}/clones/ will be deleted!"
echo ""
read -p "Continue? [y/N] " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
  echo "Uninstall cancelled."
  exit 0
fi

# Remove installation directory
if [ -d "$INSTALL_DIR" ]; then
  echo "Removing ${INSTALL_DIR}..."
  rm -rf "$INSTALL_DIR"
  echo "✓ Removed installation directory"
else
  echo "⊘ Installation directory not found"
fi

# Remove PATH from shell config files
remove_path_from_file() {
  local file="$1"
  if [ -f "$file" ]; then
    if grep -q ".grove/bin" "$file" 2>/dev/null; then
      # Remove the PATH line and the comment before it
      sed -i.bak '/# grove/d' "$file"
      sed -i.bak '/\.grove\/bin/d' "$file"
      # Remove empty lines that were left behind
      sed -i.bak '/^$/N;/^\n$/d' "$file"
      rm -f "${file}.bak"
      echo "✓ Removed PATH from $file"
    fi
  fi
}

# Check all common shell config files
remove_path_from_file "${HOME}/.zshenv"
remove_path_from_file "${HOME}/.zshrc"
remove_path_from_file "${HOME}/.bash_profile"
remove_path_from_file "${HOME}/.bashrc"

echo ""
echo "✓ Uninstall complete!"
echo ""
echo "Reload your shell or run:"
echo "  source ~/.zshrc  # or ~/.bashrc"
