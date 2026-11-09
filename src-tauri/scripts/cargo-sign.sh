#!/bin/sh
# Tauri `build.runner`: runs cargo, then signs dev builds with the stable
# "NCSync Dev" cert so Keychain "Always Allow" survives rebuilds.
# (Release bundles are signed by the Tauri bundler via bundle.macOS.signingIdentity.)
cargo "$@" || exit $?
[ "$1" = build ] || exit 0
case " $* " in *" --release "*) exit 0 ;; esac
dir="$(cd "$(dirname "$0")/.." && pwd)/target"
target=$(echo " $* " | sed -n 's/.* --target \([^ ]*\) .*/\1/p')
bin="$dir/${target:+$target/}debug/ncsync"
[ -f "$bin" ] && codesign --force -s "NCSync Dev" --identifier com.bryan.ncsync "$bin" 2>&1 | grep -v "replacing existing signature"
exit 0
