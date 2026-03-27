# Filesystem Research: Top 10 Open-Source File Systems

Criteria: large storage volumes (PB+), incredibly high file count (billions), NFS suitability, extreme resilience.

---

## Summary Matrix

| Rank | File System | PB+ Scale | Billions of Files | NFS Quality | Resilience/Self-Healing |
|------|-------------|-----------|-------------------|-------------|------------------------|
| 1 | **Ceph** | Excellent | Good | Good (Ganesha) | Excellent |
| 2 | **IBM Storage Scale** | Excellent | Excellent | Excellent (native) | Excellent |
| 3 | **Lustre** | Excellent | Good (w/ multiple MDTs) | Poor (re-export only) | Good |
| 4 | **GlusterFS** | Good | Fair | Good (native) | Fair (slow healing) |
| 5 | **JuiceFS** | Excellent | Excellent | Fair (FUSE-based) | Good (delegated) |
| 6 | **BeeGFS** | Good | Good | Fair (NFSv4 only) | Fair |
| 7 | **SeaweedFS** | Fair | Excellent (by design) | Poor (experimental) | Fair (Enterprise only) |
| 8 | **MooseFS** | Good | Fair | Fair (Ganesha) | Fair |
| 9 | **LizardFS** | Fair | Fair | Fair (Ganesha) | Fair |
| 10 | **3FS** | Good | Unknown | None | Unknown |

---

## 1. Ceph (CephFS)

A unified, software-defined storage platform providing object, block, and file storage from a single cluster. Built on the CRUSH algorithm for intelligent data placement, runs entirely on commodity hardware.

**Strengths:**
- Scales from terabytes to exabytes with linear horizontal scaling
- Autonomous, self-healing OSDs automatically redistribute data when nodes fail
- Supports erasure coding and replication for tunable durability
- NFS export via NFS-Ganesha is well-documented and production-proven
- Strong data integrity through scrubbing, checksums, and deep-scrub cycles

**Weaknesses:**
- Operationally complex to deploy and tune; requires significant expertise
- NFS-Ganesha adds a layer of indirection -- native CephFS clients perform better
- Metadata server (MDS) can become a bottleneck under extreme small-file workloads
- Resource-hungry -- wants plenty of RAM and fast journals/WAL devices

**Used at scale by:** CERN, DigitalOcean, OVH, Wellcome Sanger Institute (20+ PB), Bloomberg, Rackspace.

---

## 2. IBM Storage Scale (GPFS) -- Community Edition

<!-- 
Anything that isn't fully open source is NOT a viable option!
 -->

Originally IBM's General Parallel File System. The community edition (free for clusters up to a certain size) gives access to one of the most battle-tested parallel file systems in existence.

**Strengths:**
- Proven at hundreds of petabytes and billions of files in production
- Native NFS v4 and SMB v3 protocol support built in
- Distributed metadata with policy-based data management and ILM
- Transparent failover, node-level recovery, and multi-site replication
- POSIX-compliant with excellent cache coherence

**Weaknesses:**
- Not fully open-source -- community edition is free but proprietary; full features require licensing
- Heavyweight -- complex to set up and administer
- Vendor lock-in risk; community edition has node/capacity limits

**Used at scale by:** Oak Ridge National Lab (Summit), many Top 500 supercomputers, financial services, life sciences.

---

## 3. Lustre

The dominant parallel file system in HPC, powering seven of the top ten supercomputers. Designed for extreme throughput with tens of thousands of concurrent clients.

**Strengths:**
- Proven at 10-100+ PB scale with throughput exceeding 2 TB/s
- Supports billions of inodes across multiple MDTs (metadata targets)
- High availability features including failover and disaster recovery
- Massive real-world deployment base with deep institutional knowledge

**Weaknesses:**
- NFS is a second-class citizen -- re-export only, performance and cache coherence degrade
- Metadata server is a known bottleneck for small-file/high-file-count workloads
- Complex to administer; requires Lustre-patched kernels on clients
- Self-healing is less autonomous than Ceph -- recovery often requires admin intervention

**Used at scale by:** Oak Ridge, Lawrence Livermore, most national labs, oil/gas, research universities.

---

## 4. GlusterFS

Software-defined, scale-out NAS. Uses a hash-based approach with no centralized metadata server, eliminating a common SPOF.

**Strengths:**
- No metadata server -- SPOF-free architecture via elastic hashing
- Native NFS and SMB protocol support; designed as a network file system from the start
- Scales to hundreds of petabytes; geo-replication built in
- Self-healing with bit-rot detection and automatic repair for replicated volumes
- Simple to deploy relative to Ceph or Lustre

**Weaknesses:**
- Self-healing performance is notoriously slow (27K files taking unacceptable times reported)
- Small-file performance is weak compared to parallel file systems
- Not well suited for billions of files -- metadata operations become sluggish at extreme scale
- Development momentum has slowed since Red Hat shifted focus to Ceph

**Used at scale by:** Red Hat customers, media/entertainment, container persistent storage.

---

## 5. JuiceFS

Modern cloud-native POSIX-compliant distributed file system. Decouples metadata (Redis, MySQL, TiKV, etc.) from data (any S3-compatible object store).

**Strengths:**
- Proven at hundreds of petabytes and hundreds of billions of files in production
- Metadata engine is pluggable -- use TiKV or distributed DB for billion-file scale
- Full POSIX compliance; can be mounted and exported via NFS
- Leverages existing object storage infrastructure for durability
- Very active open-source development (Apache 2.0 license)

**Weaknesses:**
- Resilience depends on underlying object store and metadata engine (delegation, not built-in)
- NFS export works but FUSE mount is the primary access pattern
- Relatively young compared to Ceph/Lustre
- Performance depends heavily on metadata backend choice

**Used at scale by:** Shopee, Xiaomi, Li Auto, INTSIG (PB-scale AI training).

---

## 6. BeeGFS

High-performance parallel cluster file system from Fraunhofer Institute. Distributed metadata architecture from the start.

**Strengths:**
- Distributed metadata -- no single MDS bottleneck; scales linearly
- Deployments up to 30+ PB with no theoretical capacity limits
- Easy to install and manage compared to Lustre or GPFS
- Buddy mirroring for HA of both data and metadata

**Weaknesses:**
- NFS not natively supported -- requires NFS-Ganesha; only NFSv4 works
- Not truly self-healing -- buddy mirroring provides HA, but automatic repair is limited
- Open-source version has fewer enterprise features; controlled by NetApp
- Smaller community; fewer production references at billions-of-files scale

**Used at scale by:** Lawrence Livermore (AI workloads), HPC sites, Fraunhofer.

---

## 7. SeaweedFS

Distributed storage designed for billions of files with O(1) disk access. Inspired by Facebook's Haystack paper -- packs small files into volumes.

**Strengths:**
- Engineered for billions of files with O(1) disk access -- avoids inode overhead
- Erasure coding for space-efficient durability
- Rack and data center aware replication; automatic master failover
- S3-compatible API; supports FUSE mount
- Lightweight and easy to deploy

**Weaknesses:**
- NFS support is experimental / in development
- Self-healing only in Enterprise (paid) edition
- Relatively small community
- Less proven at true petabyte scale
- Not POSIX-native -- filer layer adds overhead

**Used at scale by:** Various smaller organizations; no major public PB-scale references.

---

## 8. MooseFS

Open-source, POSIX-compliant distributed file system. Centralized metadata server with chunk servers for data.

**Strengths:**
- Simple architecture; easy to deploy and manage
- Scales up to 16 EB theoretical, 2+ billion files
- Configurable per-file/directory replication goals
- Snapshots and trash bin (deleted file recovery) built in
- In production since 2005 at multi-PB scale

**Weaknesses:**
- Single master server is SPOF (Pro version adds HA, open-source does not)
- Performance degrades past hundreds of millions of files in practice
- NFS via Ganesha only
- No checksumming or bit-rot detection in open-source version
- Small development team

**Used at scale by:** Various European organizations; documented at 5.5+ PB.

---

## 9. LizardFS

Fork of MooseFS (2013) adding erasure coding and improved metadata HA.

**Strengths:**
- Erasure coding in the open-source version (unlike MooseFS)
- NFS support via NFS-Ganesha
- Metadata HA with shadow masters
- POSIX-compliant

**Weaknesses:**
- Development has largely stalled -- very few recent commits; future uncertain
- Same metadata architecture limitations as MooseFS for billion-file workloads
- Smaller community than MooseFS
- Limited petabyte-scale production references
- Self-healing is basic (re-replication only)

**Used at scale by:** Limited; some European HPC and media companies.

---

## 10. DeepSeek 3FS (Fire-Flyer File System)

Very new (open-sourced Feb 2025) high-performance distributed file system from DeepSeek for AI training/inference. MIT licensed.

**Strengths:**
- Extraordinary read throughput (7.3 TB/s on 180 nodes)
- Petabyte-scale by design
- CRAQ (Chain Replication with Apportioned Queries) for strong consistency
- Fully open-source under MIT license
- Disaggregated architecture pools thousands of SSDs

**Weaknesses:**
- Extremely new -- limited production track record outside DeepSeek
- No NFS support; designed for RDMA/native access only
- Deliberately ignores read caching (by design for AI)
- Not a general-purpose file system
- Self-healing characteristics not well documented

**Used at scale by:** DeepSeek (internally since ~2019).

---

## Key Takeaway

No single open-source file system is truly excellent at all four criteria simultaneously. **Ceph comes closest** but is operationally demanding. IBM Storage Scale arguably does it best, but calling it "open source" is a stretch. If NFS is a hard requirement with self-healing at petabyte scale, **Ceph is the best bet**, potentially paired with careful MDS scaling for billions of files. If NFS is negotiable, Lustre or JuiceFS may serve better depending on workload profile.

---

## Sources

- [Billion-files File Systems (BfFS): A Comparison](https://arxiv.org/html/2408.01805v1)
- [The 13 Best Distributed File Systems & Object Storage Solutions](https://solutionsreview.com/data-storage/the-best-distributed-file-systems/)
- [SeaweedFS GitHub](https://github.com/seaweedfs/seaweedfs)
- [Ceph Architecture Documentation](https://docs.ceph.com/en/latest/architecture/)
- [Lustre Wiki - NFS vs. Lustre](https://wiki.lustre.org/NFS_vs._Lustre)
- [GlusterFS - Wikipedia](https://en.wikipedia.org/wiki/Gluster)
- [BeeGFS NFS Export Documentation](https://doc.beegfs.io/latest/advanced_topics/nfs_export.html)
- [IBM Spectrum Scale - Advanced HPC](https://www.advancedhpc.com/pages/ibm-spectrum-scale)
- [JuiceFS Introduction](https://juicefs.com/docs/community/introduction/)
- [MooseFS GitHub](https://github.com/moosefs/moosefs)
- [DeepSeek 3FS GitHub](https://github.com/deepseek-ai/3FS)
- [Comparison of Distributed File Systems - Wikipedia](https://en.wikipedia.org/wiki/Comparison_of_distributed_file_systems)
