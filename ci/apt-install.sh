#!/usr/bin/env bash
# Installs Debian packages on a CI runner from the Ubuntu archive alone:
# `apt-get update` and `apt-get install` read only the runner's Ubuntu
# sources (/etc/apt/sources.list.d/ubuntu.sources), never the third-party
# sources its image preinstalls, which CI does not use (packages.microsoft.com
# answered 403 and failed `apt-get update` in run 45862721274). Every apt-get
# in the workflows and ci/ goes through here (agentfs-repo's
# `ci_installs_outside_tools_through_their_homes`; docs/development.md,
# "Runners, images and runbooks").
#
# Usage: ci/apt-install.sh <package>...
set -euo pipefail
[ "$#" -gt 0 ] || { echo "usage: ci/apt-install.sh <package>..." >&2; exit 2; }
sources=/etc/apt/sources.list.d/ubuntu.sources
[ -f "$sources" ] || { echo "no $sources: is this an Ubuntu 24.04 runner?" >&2; exit 1; }
only_ubuntu=(-o "Dir::Etc::sourcelist=$sources" -o Dir::Etc::sourceparts=-)
sudo apt-get "${only_ubuntu[@]}" update
sudo apt-get "${only_ubuntu[@]}" install -y "$@"
