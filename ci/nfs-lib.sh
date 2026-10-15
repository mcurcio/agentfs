# An NFS server on the runner, and NFS clients of its export (design
# §14.2). Source after lib.sh; don't run. GitHub ubuntu-24.04 runners only
# (it installs packages and changes the host).
#
#   nfs_server DIR             exports DIR (rw, sync, no_root_squash) as
#                              the NFSv4 root, with lease and grace times
#                              of $NFS_LEASE seconds (default 15)
#   nfs_netns NAME IP HOSTIP   a network namespace NAME at IP, linked to
#                              the host at HOSTIP (a veth pair)
#   nfs_mount SERVER MNT       mounts the export at MNT from the host's
#                              network namespace
#   nfs_mount_in NAME SERVER MNT
#                              mounts it from namespace NAME, with a host
#                              name of its own: a second NFSv4 client
#                              (its own client id, lease and locks), still
#                              in the host's mount namespace
#   nfs_partition NAME / nfs_heal NAME
#                              drops / restores all traffic between the
#                              namespace and the host
#   nfs_cleanup                unmounts (forced, lazy), stops the server
#
# Mount options: NFSv4.2, hard, lookupcache=positive (a name another
# client creates is found at once), as docs/csi.md recommends.

NFS_LEASE=${NFS_LEASE:-15}
NFS_OPTS=${NFS_OPTS:-vers=4.2,hard,lookupcache=positive,timeo=50,retrans=2}
NFS_MOUNTS=()
NFS_NETNS=()

nfs_server() { # dir
  sudo apt-get install -y -qq nfs-kernel-server nfs-common iproute2 iptables >/dev/null
  sudo mkdir -p "$1"
  sudo chmod 755 "$1"
  # Short lease and grace times, so a lost client's locks expire within
  # the test (design §14.2): the server's lease must be at least twice
  # the services' --lock-fence.
  sudo mkdir -p /etc/nfs.conf.d
  sudo tee /etc/nfs.conf.d/agentfs-ci.conf >/dev/null <<EOF
[nfsd]
lease-time=$NFS_LEASE
grace-time=$NFS_LEASE
vers3=n
EOF
  echo "$1 *(rw,sync,no_subtree_check,no_root_squash,fsid=0,insecure)" |
    sudo tee /etc/exports >/dev/null
  sudo systemctl restart nfs-server
  sudo exportfs -ra
  local lease
  lease=$(sudo cat /proc/fs/nfsd/nfsv4leasetime 2>/dev/null || echo unknown)
  echo "nfsd: v4 lease time $lease s, exports:"
  sudo exportfs -v
  [ "$lease" = "$NFS_LEASE" ] || {
    # Older nfs-utils ignore nfs.conf's lease-time: set it with nfsd
    # stopped.
    sudo systemctl stop nfs-server
    echo "$NFS_LEASE" | sudo tee /proc/fs/nfsd/nfsv4leasetime >/dev/null
    echo "$NFS_LEASE" | sudo tee /proc/fs/nfsd/nfsv4gracetime >/dev/null || true
    sudo systemctl start nfs-server
    lease=$(sudo cat /proc/fs/nfsd/nfsv4leasetime)
    echo "nfsd: v4 lease time now $lease s"
  }
}

nfs_netns() { # name ip hostip
  sudo ip netns add "$1"
  NFS_NETNS+=("$1")
  sudo ip link add "veth-$1" type veth peer name eth0 netns "$1"
  sudo ip addr add "$3/24" dev "veth-$1"
  sudo ip link set "veth-$1" up
  sudo ip netns exec "$1" ip addr add "$2/24" dev eth0
  sudo ip netns exec "$1" ip link set eth0 up
  sudo ip netns exec "$1" ip link set lo up
  sudo ip netns exec "$1" ping -c1 -W2 "$3" >/dev/null || fail "namespace $1 cannot reach $3"
}

nfs_mount() { # server mnt
  sudo mkdir -p "$2"
  sudo mount -t nfs4 -o "$NFS_OPTS" "$1:/" "$2"
  NFS_MOUNTS+=("$2")
  findmnt -no SOURCE,FSTYPE,OPTIONS "$2"
}

nfs_mount_in() { # netns server mnt
  sudo mkdir -p "$3"
  # The NFSv4 client id is built from the host name: a UTS namespace of
  # its own makes this a distinct client to the server.
  sudo nsenter --net="/run/netns/$1" unshare --uts sh -c \
    "hostname nfs-client-$1 && mount -t nfs4 -o '$NFS_OPTS' '$2:/' '$3'"
  NFS_MOUNTS+=("$3")
  findmnt -no SOURCE,FSTYPE,OPTIONS "$3"
}

nfs_partition() { # netns
  sudo ip netns exec "$1" iptables -I INPUT -i eth0 -j DROP
  sudo ip netns exec "$1" iptables -I OUTPUT -o eth0 -j DROP
}

nfs_heal() { # netns
  sudo ip netns exec "$1" iptables -D INPUT -i eth0 -j DROP || true
  sudo ip netns exec "$1" iptables -D OUTPUT -o eth0 -j DROP || true
}

nfs_cleanup() {
  local ns m
  for ns in "${NFS_NETNS[@]}"; do nfs_heal "$ns" 2>/dev/null || true; done
  for m in "${NFS_MOUNTS[@]}"; do
    timeout 20 sudo umount -f -l "$m" 2>/dev/null || true
  done
  for ns in "${NFS_NETNS[@]}"; do sudo ip netns del "$ns" 2>/dev/null || true; done
  timeout 30 sudo systemctl stop nfs-server 2>/dev/null || true
}
