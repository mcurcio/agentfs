#!/usr/bin/env bash
# Mounts the runner's NFS export at the CSI driver's basePath inside a kind
# node container, now and at every boot of the container (a systemd unit
# ordered before the kubelet), so `docker restart` of the node brings it
# back (design §14.2, docs/csi.md §11). Each node container has its own
# network and UTS namespaces, so it is a separate NFSv4 client.
#
# Usage: ci/kind/nfs-node-mount.sh <node container> <server ip>
#
# The kernel's NFS client is used directly (mount -i, no mount.nfs helper
# in kindest/node); if that fails, nfs-common is installed in the node.

set -euo pipefail
node=${1:?node container}
server=${2:?server ip}
base=/var/lib/agentfs-csi/volumes
opts="vers=4.2,hard,lookupcache=positive,timeo=50,retrans=2,addr=$server"

docker exec -i "$node" sh -c "cat >/usr/local/bin/agentfs-nfs-mount" <<SCRIPT
#!/bin/sh
# Written by ci/kind/nfs-node-mount.sh.
mkdir -p $base
mountpoint -q $base && exit 0
for i in \$(seq 1 60); do
  if command -v mount.nfs4 >/dev/null 2>&1; then
    mount -t nfs4 -o $opts $server:/ $base && exit 0
  else
    mount -i -t nfs4 -o $opts $server:/ $base && exit 0
  fi
  sleep 1
done
exit 1
SCRIPT
docker exec "$node" chmod 0755 /usr/local/bin/agentfs-nfs-mount
docker exec -i "$node" sh -c "cat >/etc/systemd/system/agentfs-nfs.service" <<'UNIT'
[Unit]
Description=The CSI driver's basePath on the runner's NFS export (CI)
After=network-online.target
Before=kubelet.service containerd.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/agentfs-nfs-mount

[Install]
WantedBy=multi-user.target
RequiredBy=kubelet.service
UNIT
docker exec "$node" systemctl daemon-reload
docker exec "$node" systemctl enable agentfs-nfs.service >/dev/null 2>&1
if ! docker exec "$node" timeout 30 /usr/local/bin/agentfs-nfs-mount; then
  echo "kernel NFS mount failed in $node; installing nfs-common there" >&2
  docker exec "$node" sh -c 'apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nfs-common >/dev/null'
  docker exec "$node" timeout 60 /usr/local/bin/agentfs-nfs-mount
fi
docker exec "$node" findmnt -no SOURCE,FSTYPE,OPTIONS "$base"
