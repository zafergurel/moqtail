#!/usr/bin/env bash

# Sets up HTB bandwidth shaping and/or netem delay on an interface for a
# specific destination IP/port, using iptables packet marking.
#
# Usage:
#   ./tc_qdisc.sh <rate_bps> <interface> <dest_addr> <port> <proto> <mark> [delay_ms]
#
#   rate_bps  : bandwidth cap in bps; 0 = no bandwidth limit (delay-only)
#   delay_ms  : one-way downstream delay in ms (default 0)
#
# At least one of rate_bps > 0 or delay_ms > 0 must be set.

TC="/usr/sbin/tc"
IPTABLES="/usr/sbin/iptables"

RATE="$1"
INTERFACE_1="$2"
DEST_ADDRESS="$3"
PORT="$4"
PROTO="$5"
MARK="$6"
DELAY_MS="${7:-0}"

MARK_HEX=$(printf '%x' "$MARK")

# Delay-only: substitute a 1 Gbps dummy rate so HTB passes everything through
# but the class handle exists as a netem attachment point.
if [ "$RATE" -eq 0 ] 2>/dev/null; then
  if [ "$DELAY_MS" -gt 0 ] 2>/dev/null; then
    RATE=1000000000
  else
    echo "Neither rate nor delay specified — nothing to do."
    exit 0
  fi
fi

CEIL=$(echo "$RATE*1.1" | bc | cut -d. -f1)

if [ -z "$INTERFACE_1" ] || [ -z "$DEST_ADDRESS" ] || [ -z "$PORT" ] || [ -z "$RATE" ] || [ -z "$CEIL" ]; then
  echo "Usage: ./tc_qdisc.sh <rate_bps> <interface> <dest_addr> <port> <proto> <mark> [delay_ms]"
  exit 1
fi

# ── HTB root qdisc ────────────────────────────────────────────────────────────

if $TC qdisc show dev $INTERFACE_1 | grep -q "qdisc htb 1:"; then
  echo "root qdisc already exists"
else
  echo "adding root qdisc"
  $TC qdisc add dev $INTERFACE_1 root handle 1: htb
fi

# ── HTB class ─────────────────────────────────────────────────────────────────

if $TC class show dev $INTERFACE_1 | grep -q "class htb 1:${MARK_HEX}"; then
  echo "class 1:${MARK} (1:0x${MARK_HEX}) already exists, updating rate and ceiling"
  $TC class change dev $INTERFACE_1 parent 1: classid 1:${MARK_HEX} htb rate $RATE ceil $CEIL prio 0 burst 15k quantum 1514
else
  echo "adding class 1:${MARK} (1:0x${MARK_HEX})"
  $TC class add dev $INTERFACE_1 parent 1: classid 1:${MARK_HEX} htb rate $RATE ceil $CEIL prio 0 burst 15k quantum 1514
fi

# ── netem leaf qdisc (delay) ──────────────────────────────────────────────────

# Always delete any existing leaf qdisc first, then re-add if delay > 0.
$TC qdisc del dev $INTERFACE_1 parent 1:${MARK_HEX} 2>/dev/null || true

if [ "$DELAY_MS" -gt 0 ] 2>/dev/null; then
  echo "adding netem delay ${DELAY_MS}ms under class 1:${MARK_HEX}"
  $TC qdisc add dev $INTERFACE_1 parent 1:${MARK_HEX} netem delay ${DELAY_MS}ms
fi

# ── tc filter ─────────────────────────────────────────────────────────────────

if $TC filter show dev $INTERFACE_1 | grep -q "classid 1:${MARK_HEX}"; then
  echo "filter 1:${MARK} (1:0x${MARK_HEX}) already exists"
else
  echo "adding filter 1:${MARK} (1:0x${MARK_HEX})"
  $TC filter add dev $INTERFACE_1 parent 1: prio 0 protocol ip handle $MARK fw flowid 1:$MARK_HEX
fi

# ── iptables MARK rule ────────────────────────────────────────────────────────

if $IPTABLES -n -L OUTPUT -t mangle | grep -q -E "MARK.+$DEST_ADDRESS.+$PROTO.+$PORT.+MARK set 0x${MARK_HEX}"; then
  echo "iptables rule already exists for $PROTO → $DEST_ADDRESS:$PORT mark $MARK (0x${MARK_HEX})"
else
  echo "adding iptables rule for $PROTO → $DEST_ADDRESS:$PORT mark $MARK"
  $IPTABLES -A OUTPUT -t mangle -p $PROTO --sport $PORT -d $DEST_ADDRESS -j MARK --set-mark $MARK
fi
