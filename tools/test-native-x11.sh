#!/usr/bin/env bash
set -euo pipefail
# Uses a new private X server; never sends automation to the caller's desktop.
exec xvfb-run -a -s '-screen 0 1280x800x24 -nolisten tcp' \
  cargo test -p gm-platform -- --ignored --exact linux::x11::native_tests::real_capture_focus_keyboard_pointer_and_cancellation
