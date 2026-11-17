#!/bin/sh
# Caller owns WifiLock, released discovery, and supplies <=70 seconds remaining.
# Coordinate with /opt/mu300/bin/wcn-reopen without restarting USB or cellular.
set -u
budget=${1:-70}
case "$budget" in ''|*[!0-9]*) exit 1;; esac
[ "$budget" -gt 0 ] && [ "$budget" -le 70 ] || exit 1
now() { cut -d. -f1 /proc/uptime; }
deadline=$(( $(now) + budget ))
remaining() { echo $((deadline - $(now))); }
ap_up() {
	left=$(remaining)
	[ "$left" -gt 0 ] || return 1
	[ "$left" -le 5 ] || left=5
	ubus -t "$left" call hostapd.wlan0 get_status 2>/dev/null | grep -q '"status": "ENABLED"'
}
ap_up && exit 0
[ "$(uci -q get wireless.ap0.disabled)" = 0 ] || exit 1
lock=/run/mu300-wcn-reopen.lock
candidate="$lock.scenario.$$"
mkdir "$candidate" || exit 1
echo "$$" > "$candidate/scenario-pid"
# Older platform handlers read pid; installed handlers also honor mkdir failure.
echo "$$" > "$candidate/pid"
down=0
verified=0
cleanup() {
	if [ "$down" = 1 ] && [ "$verified" = 0 ] && [ "$(uci -q get wireless.ap0.disabled)" = 0 ]; then
		left=$(remaining)
		if [ "$left" -gt 0 ]; then
			[ "$left" -le 3 ] || left=3
			ubus -t "$left" call network.wireless up '{"device":"radio0"}' >/dev/null 2>&1 || :
		fi
	fi
	[ "$(readlink "$lock" 2>/dev/null)" != "$candidate" ] || rm -f "$lock"
	rm -f "$candidate/scenario-pid" "$candidate/pid"
	rmdir "$candidate" 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
if ! ln -sT "$candidate" "$lock" 2>/dev/null; then
	holder=$(cat "$lock/scenario-pid" 2>/dev/null)
	target=$(readlink "$lock" 2>/dev/null)
	case "$target" in
		"$lock".scenario.*)
			case "$holder" in ''|*[!0-9]*) exit 1;; esac
			# WifiLock serializes scenario helpers, including stale-lock takeover.
			if ! kill -0 "$holder" 2>/dev/null; then
				rm -f "$lock" "$target/scenario-pid" "$target/pid"
				rmdir "$target" 2>/dev/null || :
				ln -sT "$candidate" "$lock" 2>/dev/null || exit 1
			else
				exit 1
			fi
			;;
		*) logger -t scenario 'native WCN reconciliation is active'; exit 1;;
	esac
fi
logger -t scenario 'AP verification failed; reconciling radio0 without a manual WCN reset'
[ "$(remaining)" -gt 0 ] || exit 1
down=1
ubus -t 5 call network.wireless down '{"device":"radio0"}' >/dev/null 2>&1 || :
[ "$(uci -q get wireless.ap0.disabled)" = 0 ] || exit 1
[ "$(remaining)" -gt 0 ] || exit 1
ubus -t 5 call network.wireless up '{"device":"radio0"}' >/dev/null 2>&1 || exit 1
while [ "$(remaining)" -gt 0 ]; do
	if ap_up; then
		verified=1
		logger -t scenario 'radio0 recovery verified AP ENABLED'
		exit 0
	fi
	sleep 1
done
logger -t scenario 'radio0 recovery did not restore AP within action budget'
exit 1
