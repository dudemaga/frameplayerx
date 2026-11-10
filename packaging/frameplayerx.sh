#!/bin/bash
# FramePlayerX launcher: what Steam (or Frame Control/FrameDrop) runs.
#
# Uses the FFmpeg libraries shipped in lib/.
here="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Optional environment for runs started from Steam, which can't pass any:
# KEY=value lines, e.g. FRAMEPLAYERX_DEBUG_INPUT=1.
envfile=${XDG_CONFIG_HOME:-$HOME/.config}/frameplayerx/env
if [[ -f $envfile ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$envfile"
  set +a
fi

exec "$here/frameplayerx" "$@"
