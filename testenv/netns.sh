#!/bin/bash
# Network namespaces for two Mumcord clients A and B (spec 04 §10).
#
#   sudo testenv/netns.sh up [direct|nodirect|noudp]   create (or re-profile) the namespaces
#   sudo testenv/netns.sh profile direct|nodirect|noudp
#   sudo -E testenv/netns.sh run a|b <command…>        run a command as you inside A or B
#   sudo testenv/netns.sh status
#   sudo testenv/netns.sh down
#
# Topology: A (10.77.0.2) and B (10.77.0.3) share a LAN bridge inside a router namespace, so a
# direct connection between them works like on a home network. The router masquerades them
# towards the host, which they reach at 10.77.255.1; the test server and the relay listen there
# (up.sh announces that address when the namespaces exist). Nothing reaches the internet.
# Docker's FORWARD rules live in the host namespace only, so they never touch this traffic.
#
# Profiles (nftables inside A and B):
#   direct    no restrictions
#   nodirect  drops all traffic between A and B, so direct peer-to-peer fails
#   noudp     nodirect, plus all UDP leaving A and B is dropped (also Mumble voice, which then
#             tunnels over TCP), so the relay must use TCP
set -euo pipefail

NS_A=mumpeer-a
NS_B=mumpeer-b
NS_R=mumpeer-r
HOST_IF=mp-up
HOST_ADDR=10.77.255.1
ROUTER_UP=10.77.255.2
LAN_GW=10.77.0.1
ADDR_A=10.77.0.2
ADDR_B=10.77.0.3

die() { echo "$*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "run with sudo"
command -v nft >/dev/null || die "nft (nftables) is required"

in_ns() { ip netns exec "$1" "${@:2}"; }

no_ipv6() {
  # Link-local IPv6 between A and B would be a direct path the profiles do not block.
  in_ns "$1" sysctl -qw net.ipv6.conf.all.disable_ipv6=1 net.ipv6.conf.default.disable_ipv6=1
}

create() {
  ip netns add "$NS_R"
  ip netns add "$NS_A"
  ip netns add "$NS_B"
  for ns in "$NS_R" "$NS_A" "$NS_B"; do
    in_ns "$ns" ip link set lo up
    no_ipv6 "$ns"
  done

  ip link add "$HOST_IF" type veth peer name uplink netns "$NS_R"
  ip addr add "$HOST_ADDR/30" dev "$HOST_IF"
  ip link set "$HOST_IF" up
  in_ns "$NS_R" ip addr add "$ROUTER_UP/30" dev uplink
  in_ns "$NS_R" ip link set uplink up
  in_ns "$NS_R" ip link add br0 type bridge
  in_ns "$NS_R" ip addr add "$LAN_GW/24" dev br0
  in_ns "$NS_R" ip link set br0 up
  in_ns "$NS_R" sysctl -qw net.ipv4.ip_forward=1
  in_ns "$NS_R" nft -f - <<NFT
table ip mumpeer {
  chain post {
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr 10.77.0.0/24 oifname "uplink" masquerade
  }
}
NFT

  for side in a b; do
    local ns addr
    if [ "$side" = a ]; then ns=$NS_A addr=$ADDR_A; else ns=$NS_B addr=$ADDR_B; fi
    in_ns "$NS_R" ip link add "lan-$side" type veth peer name eth0 netns "$ns"
    in_ns "$NS_R" ip link set "lan-$side" master br0 up
    in_ns "$ns" ip addr add "$addr/24" dev eth0
    in_ns "$ns" ip link set eth0 up
    in_ns "$ns" ip route add default via "$LAN_GW"
  done
}

apply_profile() {
  local profile="$1" ns other
  case "$profile" in direct|nodirect|noudp) ;; *) die "unknown profile: $profile" ;; esac
  for side in a b; do
    if [ "$side" = a ]; then ns=$NS_A other=$ADDR_B; else ns=$NS_B other=$ADDR_A; fi
    in_ns "$ns" nft delete table inet mumpeer 2>/dev/null || true
    [ "$profile" = direct ] && continue
    local udp_rule=""
    [ "$profile" = noudp ] && udp_rule='meta l4proto udp oifname != "lo" drop'
    in_ns "$ns" nft -f - <<NFT
table inet mumpeer {
  chain out {
    type filter hook output priority filter; policy accept;
    ip daddr $other drop
    $udp_rule
  }
  chain in {
    type filter hook input priority filter; policy accept;
    ip saddr $other drop
  }
}
NFT
  done
  echo "$profile" > "/run/mumpeer-netns-profile"
  echo "profile $profile active (A $ADDR_A, B $ADDR_B, host $HOST_ADDR)"
}

cmd="${1:-}"
case "$cmd" in
  up)
    ip netns list | grep -qw "$NS_R" || create
    apply_profile "${2:-direct}"
    ;;
  profile)
    ip netns list | grep -qw "$NS_R" || die "namespaces missing: run '$0 up' first"
    apply_profile "${2:?profile missing}"
    ;;
  run)
    side="${2:?a or b}"; shift 2
    case "$side" in a) ns=$NS_A ;; b) ns=$NS_B ;; *) die "side must be a or b" ;; esac
    user="${SUDO_USER:?run with sudo -E from your own account}"
    home="$(getent passwd "$user" | cut -d: -f6)"
    # Keeps the caller's environment (display, audio and D-Bus sockets are files, not network).
    exec ip netns exec "$ns" setpriv --reuid="$SUDO_UID" --regid="$SUDO_GID" --init-groups \
      env HOME="$home" USER="$user" "$@"
    ;;
  status)
    ip netns list | grep -E "mumpeer-(a|b|r)" || echo "no namespaces"
    [ -f /run/mumpeer-netns-profile ] && echo "profile: $(cat /run/mumpeer-netns-profile)"
    ;;
  down)
    for ns in "$NS_A" "$NS_B" "$NS_R"; do ip netns delete "$ns" 2>/dev/null || true; done
    ip link delete "$HOST_IF" 2>/dev/null || true
    rm -f /run/mumpeer-netns-profile
    echo "namespaces removed"
    ;;
  *)
    sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac
