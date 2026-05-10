#!/usr/bin/env bash

# This script uses tc to set up a qdisc for a given interface and port.
# It also sets up iptables rules to mark packets for the qdisc.
# The qdisc is set up with a rate and a ceiling.
# The rate is the guaranteed bandwidth for the qdisc.
# The ceiling is the maximum bandwidth for the qdisc.
# The qdisc is set up with a flow id.

# To configure iptables to count incoming and outgoing traffic from/to given IP
# iptables -I INPUT 1 -s <IP> -j ACCEPT
# iptables -I OUTPUT 1 -d <IP> -j ACCEPT

TC="/usr/sbin/tc"
IPTABLES="/usr/sbin/iptables"

RATE="$1" # bps
INTERFACE_1="$2"
DEST_ADDRESS="$3"
PORT="$4"
PROTO="$5"   # udp or tcp
MARK="$6"    # eg. 10
MARK_HEX=$(printf '%x' "$MARK") # converting to hex values since tc having problems with big decimal values
# FLOW_ID is always 1:MARK

CEIL=$(echo "$RATE*1.1" | bc) # bps

if [ -z $INTERFACE_1 ] || [ -z $DEST_ADDRESS ] || [ -z $PORT ] || [ -z $RATE ] || [ -z $CEIL ]; then
  echo "Usage: ./tc_qdisc.sh <rate (bps)> <interface name> <dest address> <port> <protocol> <mark>"
  exit 1
fi

# create a new root queuing discipline
if $TC qdisc show dev $INTERFACE_1 | grep -q "qdisc htb 1:"; then
  echo "root qdisc already exists"
else
  echo "adding root qdisc"
  echo "$TC qdisc add dev $INTERFACE_1 root handle 1: htb"
  $TC qdisc add dev $INTERFACE_1 root handle 1: htb
fi

# create a class for the given flow id (1:MARK)
if $TC class show dev $INTERFACE_1 | grep -q "class htb 1:${MARK_HEX}"; then
  echo "class 1:${MARK} (1:0x${MARK_HEX}) already exists, updating rate and ceiling"
  echo "$TC class change dev $INTERFACE_1 parent 1: classid 1:${MARK_HEX} htb rate $RATE ceil $CEIL prio 0 burst 15k quantum 1514"
  $TC class change dev $INTERFACE_1 parent 1: classid 1:${MARK_HEX} htb rate $RATE ceil $CEIL prio 0 burst 15k quantum 1514
else
  echo "adding class 1:${MARK} (1:0x${MARK_HEX})"
  echo "$TC class add dev $INTERFACE_1 parent 1: classid 1:${MARK_HEX} htb rate $RATE ceil $CEIL prio 0 burst 15k quantum 1514"
  $TC class add dev $INTERFACE_1 parent 1: classid 1:${MARK_HEX} htb rate $RATE ceil $CEIL prio 0 burst 15k quantum 1514
  # for stochastic fair queueing the following can be created as well
  # echo $TC qdisc add dev $INTERFACE_1 parent $FLOW_ID handle 10: sfq perturb 10
  # $TC qdisc add dev $INTERFACE_1 parent $FLOW_ID handle 10: sfq perturb 10
fi

# create a filter for the given flow id (1:MARK)
if $TC filter show dev $INTERFACE_1 | grep -q "classid 1:${MARK_HEX}"; then
  echo "filter 1:${MARK} (1:0x${MARK_HEX}) already exists"
else
  echo "adding filter 1:${MARK} (1:0x${MARK_HEX})"
  echo "$TC filter add dev $INTERFACE_1 parent 1: prio 0 protocol ip handle $MARK fw flowid 1:${MARK_HEX}"
  $TC filter add dev $INTERFACE_1 parent 1: prio 0 protocol ip handle $MARK fw flowid 1:$MARK_HEX

fi

# -n for numeric output, -L for list, -t for table, -p for protocol,
#-sport for source port, -j for jump, -A for append, -D for delete
if $IPTABLES -n -L OUTPUT -t mangle | grep -q -E "MARK.+$DEST_ADDRESS.+$PROTO.+$PORT.+MARK set 0x${MARK_HEX}"; then
  echo "iptables rule already exists for $PROTO on destination $DEST_ADDRESS and port $PORT with mark $MARK (0x${MARK_HEX})"
else
  echo "adding iptables rule for $PROTO on destination $DEST_ADDRESS and port $PORT with mark $MARK"
  echo "$IPTABLES -A OUTPUT -t mangle -p $PROTO --sport $PORT -d $DEST_ADDRESS -j MARK --set-mark $MARK"
  $IPTABLES -A OUTPUT -t mangle -p $PROTO --sport $PORT -d $DEST_ADDRESS -j MARK --set-mark $MARK
fi