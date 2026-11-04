#!/bin/bash
# Usage: ./profile.sh <profile>
# Prints the relay config sections of a MUMPEER_PROFILE (see README.md), to be appended after the
# top-level keys. up.sh uses it, and tests/testenv_profiles.rs checks that every profile parses.
set -euo pipefail
case "${1:-}" in
  all) ;;
  no-files) printf '[uses]\nfiles = false\n' ;;
  no-screen) printf '[uses]\nscreen_share = false\n' ;;
  no-images) printf '[uses]\nimages = false\n' ;;
  no-camera) printf '[uses]\ncamera = false\n' ;;
  no-broadcast) printf '[limits]\nmax_broadcasts = 0\nmax_camera_broadcasts = 0\n' ;;
  old) ;; # a previous release may not know [uses] or the broadcast limits
  *) echo "unknown MUMPEER_PROFILE '${1:-}' (all, no-files, no-screen, no-images, no-camera, no-broadcast, old)" >&2; exit 1 ;;
esac
