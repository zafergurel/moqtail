#!/usr/bin/env bash

# Deletes the iptables MARK rule for the given client IP and mark.

IPTABLES="/usr/sbin/iptables"

PORT="4433"
PROTO="udp"

DEST_ADDRESS="$1"
MARK="$2"
MARK_HEX=$(printf '%x' "$MARK")

if [ -z "$DEST_ADDRESS" ] || [ -z "$MARK" ]; then
  echo "Usage: ./delete_iptable_rule.sh <destination_address> <mark>"
  exit 1
fi

if $IPTABLES -n -L OUTPUT -t mangle | grep -q -E "MARK.+$DEST_ADDRESS.+$PROTO.+$PORT.+MARK set 0x$MARK_HEX"; then
  $IPTABLES -D OUTPUT -t mangle -p $PROTO --sport $PORT -d $DEST_ADDRESS -j MARK --set-mark $MARK
  echo "iptables rule deleted for $PROTO on destination $DEST_ADDRESS port $PORT"
else
  echo "No matching iptables rule found (already deleted?)"
fi
