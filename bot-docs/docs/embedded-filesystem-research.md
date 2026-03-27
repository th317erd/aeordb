# Embedded Filesystem Research

Can any existing open-source filesystem run as an embedded library in a single process on a single file AND scale to a distributed cluster?

**Answer: No.** This is a gap in the ecosystem.

---

## Findings

| Project | Embedded Single-File | Distributed Cluster | Same Code Path | COW/Snapshots |
|---|---|---|---|---|
| Ceph BlueStore | Technically possible (ObjectStore API) | Yes | Partially | No (not COW) |
| OpenEBS uZFS (cStor) | Yes (userspace ZFS DMU) | Yes (via replication) | Partially | Yes |
| FoundationDB | Simulation only | Yes | Yes (same binary) | No |
| ZFS + file-backed vdevs | Yes (single file pool) | No (not distributed) | N/A | Yes |
| redb | Yes | No | N/A | Yes (savepoints) |

### Ceph
- Requires minimum 3 separate daemon processes (MON + MGR + OSD)
- librados is a network client, NOT an embedded engine
- MicroCeph simplifies single-node deployment but still runs multiple daemons
- BlueStore CAN be instantiated directly (Ceph test suite does this) but drags in enormous dependencies

### GlusterFS, JuiceFS, SeaweedFS, MooseFS, CubeFS
- All client-server architectures
- None can run embedded in a single process
- All require separate daemon infrastructure

### ZFS
- Requires kernel module + root privileges
- uZFS (OpenEBS cStor) runs DMU in userspace but is Kubernetes-focused
- CDDL license is incompatible with GPL

### Btrfs, Bcachefs
- Kernel-only, no userspace/embedded mode

---

## Conclusion

The solution for aeordb:
- **redb** for embedded single-file storage (COW, checksums, ACID, snapshots)
- **openraft** for distributed consensus and replication
- **Ceph via StorageBackend trait** as an optional distributed storage plugin
- Custom append-only log for the Raft log (pure Rust, no dependencies)

Same binary, same code path, single-node to cluster.
