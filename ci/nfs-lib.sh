# An NFS server on the runner, and NFS clients of its export (design
# §14.2). Source after lib.sh; don't run. GitHub ubuntu-24.04 runners only
# (it installs packages and changes the host).
#
#   nfs_server DIR             exports DIR (rw, sync, no_root_squash) as
#                              the NFSv4 root (4.0 to 4.2), with lease and
#                              grace times of $NFS_LEASE seconds (default
#                              10)
#   nfs_netns NAME IP HOSTIP   a network namespace NAME at IP, linked to
#                              the host at HOSTIP (a veth pair)
#   nfs_mount SERVER MNT [OPTS]
#                              mounts the export at MNT from the host's
#                              network namespace, with OPTS (default
#                              $NFS_OPTS)
#   nfs_mount_in NAME SERVER MNT
#                              mounts it from namespace NAME, with a host
#                              name of its own: a second NFSv4 client
#                              (its own client id, lease and locks), still
#                              in the host's mount namespace
#   nfs_cleanup                unmounts (forced, lazy), stops the server
#
# Mount options: NFSv4.2, hard, lookupcache=positive (a name another
# client creates is found at once), as docs/csi.md recommends.

NFS_LEASE=${NFS_LEASE:-10}
NFS_OPTS=${NFS_OPTS:-vers=4.2,hard,lookupcache=positive,timeo=50,retrans=2}
NFS_MOUNTS=()
NFS_NETNS=()

nfs_server() { # dir
  sudo apt-get install -y -qq nfs-kernel-server nfs-common iproute2 >/dev/null
  sudo mkdir -p "$1"
  sudo chmod 755 "$1"
  # Short lease and grace times: locks wait out the server's grace after
  # a restart. 4.0 stays on for the check that refuses it.
  sudo mkdir -p /etc/nfs.conf.d
  sudo tee /etc/nfs.conf.d/agentfs-ci.conf >/dev/null <<EOF
[nfsd]
lease-time=$NFS_LEASE
grace-time=$NFS_LEASE
vers3=n
vers4.0=y
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

nfs_mount() { # server mnt [opts]
  sudo mkdir -p "$2"
  sudo mount -t nfs4 -o "${3:-$NFS_OPTS}" "$1:/" "$2"
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

nfs_cleanup() {
  local ns m
  for m in "${NFS_MOUNTS[@]}"; do
    timeout 20 sudo umount -f -l "$m" 2>/dev/null || true
  done
  for ns in "${NFS_NETNS[@]}"; do sudo ip netns del "$ns" 2>/dev/null || true; done
  timeout 30 sudo systemctl stop nfs-server 2>/dev/null || true
}
