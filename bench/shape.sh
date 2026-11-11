#!/usr/bin/env bash
# Shapes the loopback UDP traffic of one host port like a slow link, in the kernel.
#   sudo bench/shape.sh up <port> <mbps> <rtt_ms> [loss_pct] [queue_pkts]
#   sudo bench/shape.sh down
# Host -> viewer (from <port>): <mbps>, <rtt_ms>/2 delay, <loss_pct> random loss, a
# <queue_pkts> drop-tail queue (default 100). Viewer -> host (to <port>): <rtt_ms>/2 delay.
# Only that port's UDP on loopback is touched; everything else passes as before.
# macOS: a pf anchor (com.glass.bench) and dummynet pipes. Linux: tc netem on lo.
set -euo pipefail

ANCHOR=com.glass.bench
PIPE_DOWN=47101
PIPE_UP=47102
STATE=/tmp/glass-bench-shape.state

usage() { sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
[ $# -ge 1 ] || usage
[ "$(id -u)" = 0 ] || { echo "shape.sh: needs root (sudo)" >&2; exit 1; }

up_macos() {
  local port=$1 mbps=$2 rtt=$3 loss=$4 queue=$5
  local half plr
  half=$(awk "BEGIN { printf \"%d\", $rtt / 2 }") # dummynet delays are whole ms
  plr=$(awk "BEGIN { print $loss / 100 }")
  dnctl pipe $PIPE_DOWN config bw "${mbps}Mbit/s" delay "$half" plr "$plr" queue "$queue"
  dnctl pipe $PIPE_UP config delay "$half"
  # The main ruleset only hands packets to anchors it names: load /etc/pf.conf plus ours.
  { cat /etc/pf.conf; echo "dummynet-anchor \"$ANCHOR\""; echo "anchor \"$ANCHOR\""; } | pfctl -q -f - 2>/dev/null
  # Loopback packets go out and come back in on lo0: shape them once, on the way out.
  printf '%s\n' \
    "dummynet out quick on lo0 proto udp from any port $port to any pipe $PIPE_DOWN" \
    "dummynet out quick on lo0 proto udp from any to any port $port pipe $PIPE_UP" \
    | pfctl -q -a "$ANCHOR" -f -
  # pfctl -E counts a reference (and turns pf on if it was off); down gives it back.
  pfctl -E 2>&1 | sed -n 's/^Token : //p' > "$STATE"
}

down_macos() {
  pfctl -q -a "$ANCHOR" -F all 2>/dev/null || true
  dnctl -q pipe delete $PIPE_DOWN 2>/dev/null || true
  dnctl -q pipe delete $PIPE_UP 2>/dev/null || true
  pfctl -q -f /etc/pf.conf 2>/dev/null || true
  if [ -s "$STATE" ]; then pfctl -q -X "$(cat "$STATE")" 2>/dev/null || true; fi
  rm -f "$STATE"
}

# Five bands: the default priomap only uses 1:1-1:3, so 1:4 and 1:5 carry nothing else.
up_linux() {
  local port=$1 mbps=$2 rtt=$3 loss=$4 queue=$5
  local half
  half=$(awk "BEGIN { print $rtt / 2 }")
  tc qdisc add dev lo root handle 1: prio bands 5
  tc qdisc add dev lo parent 1:4 handle 40: netem delay "${half}ms" rate "${mbps}mbit" loss "${loss}%" limit "$queue"
  tc qdisc add dev lo parent 1:5 handle 50: netem delay "${half}ms"
  tc filter add dev lo parent 1: protocol ip prio 1 u32 match ip protocol 17 0xff match ip sport "$port" 0xffff flowid 1:4
  tc filter add dev lo parent 1: protocol ip prio 1 u32 match ip protocol 17 0xff match ip dport "$port" 0xffff flowid 1:5
  tc filter add dev lo parent 1: protocol ipv6 prio 2 u32 match ip6 protocol 17 0xff match ip6 sport "$port" 0xffff flowid 1:4
  tc filter add dev lo parent 1: protocol ipv6 prio 2 u32 match ip6 protocol 17 0xff match ip6 dport "$port" 0xffff flowid 1:5
  echo linux > "$STATE"
}

down_linux() {
  tc qdisc del dev lo root 2>/dev/null || true
  rm -f "$STATE"
}

os=$(uname -s)
case "$1" in
  up)
    [ $# -ge 4 ] || usage
    port=$2 mbps=$3 rtt=$4 loss=${5:-0} queue=${6:-100}
    case $os in
      Darwin) down_macos; up_macos "$port" "$mbps" "$rtt" "$loss" "$queue" ;;
      Linux) down_linux; up_linux "$port" "$mbps" "$rtt" "$loss" "$queue" ;;
      *) echo "shape.sh: $os isn't supported" >&2; exit 1 ;;
    esac
    echo "shaping udp port $port: ${mbps} Mbps, ${rtt} ms rtt, ${loss}% loss, ${queue} pkt queue"
    ;;
  down)
    case $os in
      Darwin) down_macos ;;
      Linux) down_linux ;;
    esac
    ;;
  *) usage ;;
esac
