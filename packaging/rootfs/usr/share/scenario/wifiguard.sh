#!/bin/sh
# wifiguard — Wi-Fi watchdog for the scenario engine.
# Port of luya-z-manager's sentinel Wi-Fi guard; the contract with the engine
# (crates/luyaz-scenario, unchanged from the original):
#
#   /tmp/scenario.heartbeat — the engine writes whole seconds of /proc/uptime
#     before bootsafe and on every loop, whether or not it is switching.
#   /tmp/scenario.took-over — WE write it, ONCE per takeover, with content
#     unique to that takeover (our /proc/uptime at the moment). While it exists
#     the engine does no detection, ignores the pin, and goes back to (and stays
#     in) away. The engine itself removes the marker after 10 minutes of its
#     own continuous running — rewriting the marker would restart that hold
#     forever, so we never rewrite an existing one.
#
# The engine's own watchdog (main.rs) exits past WEDGED=900 s so procd restarts
# it; we only step in when it is dead *and* the AP is down, because that is the
# state in which the device is someone's paperweight.

STALE=900
HB=/tmp/scenario.heartbeat
TAKE=/tmp/scenario.took-over

ap_up() {
	ubus call hostapd.wlan0 get_status 2>/dev/null | grep -q '"status": "ENABLED"'
}

force_on() {
	uci set wireless.ap0.disabled='0'
	uci commit wireless
	ubus call network reload >/dev/null 2>&1
	i=0
	while [ "$i" -lt 45 ]; do
		ap_up && return 0
		sleep 1
		i=$((i + 1))
	done
	return 1
}

while :; do
	sleep 30
	[ -f "$HB" ] || continue          # never saw a heartbeat: not ours to judge yet
	hb=$(cat "$HB" 2>/dev/null)
	up=$(cut -d. -f1 /proc/uptime 2>/dev/null)
	case "$hb$up" in '' | *[!0-9]*) continue ;; esac
	[ $((up - hb)) -gt "$STALE" ] || continue
	ap_up && continue                  # wedged but Wi-Fi is up: procd's respawn is enough
	force_on
	if [ ! -f "$TAKE" ]; then
		echo "$up" > "$TAKE"
		logger -t wifiguard "engine heartbeat stale ${STALE}s and AP down: forced Wi-Fi on, takeover marked"
	else
		logger -t wifiguard "engine heartbeat stale ${STALE}s and AP down: forced Wi-Fi on (takeover already marked)"
	fi
done
