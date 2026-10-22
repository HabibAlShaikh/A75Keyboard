#!/usr/bin/env bash
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-/usr/local}"

if [[ ! -x "$here/a75ctl" ]]; then
    echo "a75ctl binary not found next to this script" >&2
    exit 1
fi

install -Dm755 "$here/a75ctl" "$prefix/bin/a75ctl"
rm -f /etc/udev/rules.d/99-vgn-a75.rules
install -Dm644 "$here/70-vgn-a75.rules" /etc/udev/rules.d/70-vgn-a75.rules

if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules
    udevadm trigger
fi

echo "Installed $prefix/bin/a75ctl"
echo "Installed /etc/udev/rules.d/70-vgn-a75.rules"
echo "Replug the keyboard, then run: a75ctl"
