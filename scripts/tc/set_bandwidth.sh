#!/usr/bin/env bash

usage() {
  echo "Usage: ./set_bandwidth.sh <rate_bps> <client_ip> <mark> [delay_ms] [op]"
  echo "  rate_bps : bandwidth cap in bps (0 = no limit)"
  echo "  delay_ms : one-way downstream delay in ms (default 0)"
  echo "  op       : set (default) or del"
  exit 1
}

SCRIPT=$(realpath "$0")
CURRENT_DIR=$(dirname "$SCRIPT")

echo "Current dir: $CURRENT_DIR"

if [ -f "$CURRENT_DIR/.env" ]; then
  source $CURRENT_DIR/.env
fi

if [ -z "$INTERFACE" ]; then
  echo "Please set the INTERFACE variable in .env file"
  exit 1
fi

TC="/sbin/tc"
IPTABLES="/usr/sbin/iptables"

PORT="4433"
PROTO="udp"

RATE="$1"
DEST_ADDRESS="$2"
MARK="$3"
DELAY_MS="${4:-0}"
OP="${5:-set}"

if [ -z "$RATE" ] || [ -z "$DEST_ADDRESS" ] || [ -z "$MARK" ]; then
  usage
fi

if [[ $OP == "set" ]]; then
  echo "Applying tc: rate=${RATE}bps delay=${DELAY_MS}ms"
  $CURRENT_DIR/tc_qdisc.sh $RATE $INTERFACE $DEST_ADDRESS $PORT $PROTO $MARK $DELAY_MS
elif [[ $OP == "del" ]]; then
  echo "Deleting tc rule"
  $CURRENT_DIR/delete_iptable_rule.sh $DEST_ADDRESS $MARK
else
  usage
fi
