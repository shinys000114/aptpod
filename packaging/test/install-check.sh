#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
# Runs as root inside a fresh Debian/Ubuntu container with the .deb files
# in /dist: installs both packages, checks what postinst set up, runs the
# server as its service user, publishes a package with the CLI and
# installs it with apt from the published tree.
set -eu
export DEBIAN_FRONTEND=noninteractive
fail() { echo "FAIL: $*" >&2; [ -f /tmp/server.log ] && cat /tmp/server.log >&2; exit 1; }

arch=$(dpkg --print-architecture)
apt-get update -qq
apt-get install -y -qq /dist/aptpod-server_*_"$arch".deb /dist/aptpod_*_"$arch".deb >/dev/null

getent passwd aptpod >/dev/null || fail "no aptpod user"
[ "$(stat -c '%U:%G %a' /var/lib/aptpod)" = "aptpod:aptpod 750" ] || fail "data dir: $(stat -c '%U:%G %a' /var/lib/aptpod)"
[ "$(stat -c '%G %a' /etc/aptpod/server.toml)" = "aptpod 640" ] || fail "config: $(stat -c '%G %a' /etc/aptpod/server.toml)"
aptpod --version
aptpod-server --version

# Root must not create files the service user cannot read.
if echo 'admin-password' | aptpod-server user add admin --password-stdin 2>/tmp/err; then
    fail "user add as root was accepted"
fi
grep -q 'sudo -u aptpod' /tmp/err || fail "no hint: $(cat /tmp/err)"

sed -i 's|^publish_roots = \[\]|publish_roots = ["/srv/apt"]|' /etc/aptpod/server.toml
# Schedules in a named time zone must work on a bare image too.
echo 'timezone = "Asia/Seoul"' >>/etc/aptpod/server.toml
install -d -o aptpod -g aptpod /srv/apt
echo 'admin-password' | runuser -u aptpod -- aptpod-server user add admin --role admin --password-stdin
# setpriv execs the server itself (runuser would stay as its parent), so
# $! is the server and its exit status is the server's.
setpriv --reuid=aptpod --regid=aptpod --init-groups aptpod-server >/tmp/server.log 2>&1 &
server=$!

export APTPOD_SERVER=http://127.0.0.1:8090 APTPOD_CREDENTIALS=/tmp/creds.toml
up=
for _ in $(seq 50); do
    st=0; aptpod whoami >/dev/null 2>&1 || st=$?
    if [ "$st" = 7 ]; then up=1; break; fi   # answering, not logged in
    sleep 0.2
done
[ -n "$up" ] || fail "server did not come up"

echo 'admin-password' | aptpod login --user admin --password-stdin
aptpod publish create t --path /srv/apt/t --dist stable --component main --arch "$arch" --no-sign -y

mkdir -p /tmp/pkg/DEBIAN /tmp/pkg/usr/share/hello-aptpod
printf 'Package: hello-aptpod\nVersion: 1.0\nArchitecture: all\nMaintainer: Test <test@example.invalid>\nDescription: install test\n' >/tmp/pkg/DEBIAN/control
echo hello >/tmp/pkg/usr/share/hello-aptpod/marker
dpkg-deb --root-owner-group --build /tmp/pkg /tmp/hello-aptpod_1.0_all.deb >/dev/null
aptpod add t main /tmp/hello-aptpod_1.0_all.deb -d stable -y
aptpod ls t | grep -q 'hello-aptpod  1.0' || fail "not listed"

# apt (as its sandbox user _apt) reads the tree the service wrote.
echo 'deb [trusted=yes] file:/srv/apt/t stable main' >/etc/apt/sources.list.d/t.list
apt-get update -qq -o Dir::Etc::sourcelist=/dev/null -o Dir::Etc::sourceparts=/etc/apt/sources.list.d
apt-get install -y -qq hello-aptpod >/dev/null
[ "$(cat /usr/share/hello-aptpod/marker)" = hello ] || fail "package content"

kill -TERM "$server"
wait "$server" || fail "server exit status $?"
apt-get purge -y -qq aptpod-server aptpod >/dev/null
echo "install check passed ($(. /etc/os-release; echo "$PRETTY_NAME"), $arch)"
