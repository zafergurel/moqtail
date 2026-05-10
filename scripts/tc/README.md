# Traffic Control Scripts

All scripts shape outbound QUIC (UDP port 4433) traffic toward a specific client IP.
The network interface is read from `scripts/tc/.env`:

```bash
echo 'INTERFACE=eth0' > scripts/tc/.env   # replace eth0 with your NIC
```

## Setting a bandwidth limit

```bash
./set_bandwidth.sh <rate_bps> <client_ip> <mark>
```

Example — limit to 2 Mbps toward 192.168.1.100, mark 1:

```bash
./set_bandwidth.sh 2000000 192.168.1.100 1
```

NOTE: `<mark>` is an arbitrary integer (1–255) used as a flow identifier.
Use a consistent value per client across set/del calls.

## Removing a bandwidth limit

```bash
./set_bandwidth.sh 0 <client_ip> <mark> del
```

Example:

```bash
./set_bandwidth.sh 0 192.168.1.100 1 del
```

## Clearing all rules on an interface

```bash
./clear_all.sh eth0
```

If no interface is given, it falls back to `INTERFACE` from `.env`.

## Useful diagnostic commands

```bash
# Watch per-class traffic in real time
watch /sbin/tc -s -d class show dev eth0

# Inspect active rules
tc filter show dev eth0
tc class show dev eth0
tc qdisc show dev eth0
iptables -L OUTPUT -t mangle -n -v
```

To analyse traffic on interfaces, `iptraf` can be used.
