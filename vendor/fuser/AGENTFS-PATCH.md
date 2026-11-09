This is the crates.io fuser 0.18.0 source, licensed under its included MIT
LICENSE.md. The Linux FUSE_TMPFILE request decoding and dispatch are backported
from upstream commit c55df5dd56ad74c1d65dd5ef3f9176cabfed5b45
(https://github.com/cberner/fuser/commit/c55df5dd56ad74c1d65dd5ef3f9176cabfed5b45).
The callback omits kill_suid_gid because the 0.18.0 API does not request
FUSE_HANDLE_KILLPRIV_V2 by default or expose that field. The default remains ENOSYS.
Upstream's commit follows incompatible changes to existing callback signatures.
Trailing whitespace was also removed from CHANGELOG.md, deny.toml and src/lib.rs.
