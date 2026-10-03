#!/bin/sh
# Codenotch on Linux.
#
# Desktop compatibility:
#   * Wayland does not let a client place its own windows, and the notch has to
#     sit on a screen edge — so it runs as an X11 client under XWayland.
#   * A shell started from a snap (VS Code, for one) exports that snap's library
#     paths, and they break a binary built against the system glibc.
#   * WebKit's DMA-BUF renderer can fail to allocate GBM buffers under XWayland
#     with some graphics drivers. Use shared-memory frames without disabling the
#     compositor; an explicit 0 opts into hardware buffer transport.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target=${CARGO_TARGET_DIR:-$root/target}
bin=${CODENOTCH_BIN:-$target/release/codenotch}
if [ -z "${CODENOTCH_BIN:-}" ] && [ ! -x "$bin" ]; then
  bin=$target/debug/codenotch
fi

if [ ! -x "$bin" ]; then
  echo "No executable at $bin. From the repository root, run: make build" >&2
  exit 1
fi

exec env -u LD_LIBRARY_PATH -u GTK_PATH -u GIO_MODULE_DIR -u GSETTINGS_SCHEMA_DIR \
         -u LOCPATH -u GDK_PIXBUF_MODULE_FILE -u GDK_PIXBUF_MODULEDIR \
         GDK_BACKEND=x11 \
         WEBKIT_DMABUF_RENDERER_FORCE_SHM="${WEBKIT_DMABUF_RENDERER_FORCE_SHM:-1}" \
    "$bin" "$@"
