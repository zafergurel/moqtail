#!/usr/bin/env bash

usage() {
  echo "Usage: ./set_bandwidth.sh <rate (bps)> <client_ip> <mark> [op]"
  echo "  rate: bandwidth in bps (e.g. 2000000 for 2 Mbps)"
  echo "  op:   set (default) or del"
  exit 1
}

SCRIPT=$(realpath "$0")
CURRENT_DIR=$(dirname "$SCRIPT")

echo "Current dir: $CURRENT_DIR"

# load environment variables like INTERFACE
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
OP=${4:-"set"}

if [ -z "$RATE" ] || [ -z "$DEST_ADDRESS" ] || [ -z "$MARK" ]; then
  usage
fi

if [[ $OP == "set" ]]; then
  echo "Setting bandwidth limit"
  $CURRENT_DIR/tc_qdisc.sh $RATE $INTERFACE $DEST_ADDRESS $PORT $PROTO $MARK
elif [[ $OP == "del" ]]; then
  echo "Deleting bandwidth limit"
  $CURRENT_DIR/delete_iptable_rule.sh $DEST_ADDRESS $MARK
else
  usage
fi
