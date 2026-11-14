#!/bin/sh
# Install jiminit from out/jiminit into a root filesystem (or a staging dir).
#
#   ./scripts/install.sh                     # /sbin/jiminit, /etc/jiminit.toml
#   ./scripts/install.sh --prefix /usr       # /usr/sbin/jiminit, /etc/...
#   ./scripts/install.sh --destdir out/pkg   # for packaging (DESTDIR)
#
# An existing /etc/jiminit.toml is never overwritten.
set -eu
cd "$(dirname "$0")/.."

PREFIX=
DESTDIR=
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX=${2:?--prefix needs a path}; shift 2 ;;
        --destdir) DESTDIR=${2:?--destdir needs a path}; shift 2 ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

BIN=out/jiminit
if [ ! -f "$BIN" ]; then
    echo "missing $BIN; run scripts/build.sh first" >&2
    exit 1
fi

BINDIR=$DESTDIR$PREFIX/sbin
# Configuration and docs always use the FHS locations; PREFIX only moves the
# binary (useful for /usr-merged systems).
CONFDIR=$DESTDIR/etc
DOCDIR=$DESTDIR/usr/share/doc/jiminit

install -d "$BINDIR" "$CONFDIR/jiminit/services.d" "$DOCDIR"
install -m755 "$BIN" "$BINDIR/jiminit"
install -m644 README.md "$DOCDIR/README.md"
install -d "$DOCDIR/docs"
cp -R docs/. "$DOCDIR/docs/"

if [ -e "$CONFDIR/jiminit.toml" ]; then
    echo "keeping existing $CONFDIR/jiminit.toml"
else
    install -m644 contrib/jiminit.toml "$CONFDIR/jiminit.toml"
fi

cat > "$CONFDIR/jiminit/services.d/example.toml.disabled" <<'EXAMPLE'
# Service definitions are one TOML file per service; the file name is the
# service name. Remove the ".disabled" suffix to enable this example.
exec = "/bin/sh"
args = ["-c", "while true; do echo hello from example; sleep 60; done"]
restart = "on-failure"
EXAMPLE

echo "installed $BINDIR/jiminit"
echo "config:  $CONFDIR/jiminit.toml"
echo "services:$CONFDIR/jiminit/services.d"
