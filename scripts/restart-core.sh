#!/bin/sh
# Restarts the core with only the login's own variables instead of the calling shell's.
# A running core keeps the environment it started with; one this starts gets only
# these variables, so a build shell's (an agent's, a CI job's) never reach terminals.
# The SSH markers only tell the core it was started over ssh; programs never see them.
set -eu
exe="$1"
shift
exec env -i \
  ${PATH:+"PATH=$PATH"} \
  ${HOME:+"HOME=$HOME"} \
  ${USER:+"USER=$USER"} \
  ${LOGNAME:+"LOGNAME=$LOGNAME"} \
  ${SHELL:+"SHELL=$SHELL"} \
  ${TMPDIR:+"TMPDIR=$TMPDIR"} \
  ${LANG:+"LANG=$LANG"} \
  ${LC_ALL:+"LC_ALL=$LC_ALL"} \
  ${LC_CTYPE:+"LC_CTYPE=$LC_CTYPE"} \
  ${SSH_AUTH_SOCK:+"SSH_AUTH_SOCK=$SSH_AUTH_SOCK"} \
  ${SSH_CONNECTION:+"SSH_CONNECTION=$SSH_CONNECTION"} \
  ${SSH_CLIENT:+"SSH_CLIENT=$SSH_CLIENT"} \
  ${SSH_TTY:+"SSH_TTY=$SSH_TTY"} \
  ${DISPLAY:+"DISPLAY=$DISPLAY"} \
  ${WAYLAND_DISPLAY:+"WAYLAND_DISPLAY=$WAYLAND_DISPLAY"} \
  ${XAUTHORITY:+"XAUTHORITY=$XAUTHORITY"} \
  ${DBUS_SESSION_BUS_ADDRESS:+"DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"} \
  ${XDG_CONFIG_HOME:+"XDG_CONFIG_HOME=$XDG_CONFIG_HOME"} \
  ${XDG_DATA_HOME:+"XDG_DATA_HOME=$XDG_DATA_HOME"} \
  ${XDG_STATE_HOME:+"XDG_STATE_HOME=$XDG_STATE_HOME"} \
  ${XDG_RUNTIME_DIR:+"XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"} \
  ${HC_SESSION:+"HC_SESSION=$HC_SESSION"} \
  ${HC_SOCKET:+"HC_SOCKET=$HC_SOCKET"} \
  "$exe" server restart "$@"
