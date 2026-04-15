# AeorDB Replication Design — Democratic Debate

## Context

We are designing multi-node replication for AeorDB using openraft. The full design spec is at `/home/wyatt/Projects/aeordb-workspace/aeordb/bot-docs/plan/replication-design.md`. Key decisions already made:

- Replicate **semantic operations** (StoreFile, DeleteFile, etc.), not raw KV ops
- Separate consensus plane (Raft log with metadata) from data plane (chunk transfer)
- Static cluster config stored in the database's system tables
- HTTP-based inter-node communication on /raft/* endpoints
- Cluster secret for inter-node auth

## Open Questions to Resolve

1. **System table replication** — User/group/permission/API key changes are stored in system tables via engine.store_entry(). These MUST be replicated. Since we're replicating semantic operations, do we add RaftRequest variants for every system table mutation (CreateUser, UpdateGroup, CreateApiKey, etc.)?

2. **Auth in cluster mode** — With --auth self, each node generates its own JWT signing key. In a cluster, all nodes must share the same key. How do we bootstrap this? The signing key must exist before replication is set up.

3. **GC coordination** — GC reclaims unreachable entries locally. In a cluster, a chunk might be "unreachable" on one node but needed by another catching up. Do we disable GC in cluster mode? Coordinate across nodes? Only run on leader?

4. **Event bus in cluster** — Events (file created, etc.) are currently local. Should events from other nodes be forwarded? SSE subscribers want cluster-wide visibility.

5. **Task/cron coordination** — Background tasks (reindex, GC) should probably only run on leader. Or each node runs independently?

6. **Hot file recovery vs Raft recovery** — The hot file replays uncommitted WAL entries on crash. In cluster mode, Raft is the source of truth. Do we still need hot file recovery?

7. **Chunk transfer reliability** — If leader crashes after committing a StoreChunkRef but before followers fetch the chunk, where do followers get it?

8. **Large snapshot transfer** — Multi-GB Raft snapshots for new nodes. How do we handle this without blocking the cluster?

9. **Standalone → cluster migration** — User starts standalone, adds replication later. How does existing data become the initial Raft state?

10. **The big file problem** — A 1GB file upload. The semantic operation approach means RaftRequest::StoreFile contains 1GB of data. Even with chunk-ref optimization, how do we handle the Raft log entry size? Should we split large files into multiple Raft operations?

## Rules of Debate

- Each participant speaks in turn, in character
- Address the previous speaker's points before making new ones
- After 2 full rounds, any participant can call for consensus
- To reach consensus: propose a position, next speakers either agree or counter
- When all 5 agree on a consolidated position for all 10 questions, write "CONSENSUS REACHED" and a final summary

---

## Round 1

### Engineer

I've reviewed the design spec and audited the existing code. Let me ground these 10 questions in what actually exists and what the implementation constraints are.

**Q1 (System table replication):** The cleanest path is intercepting at the `engine.store_entry()` level, not adding RaftRequest variants per system table mutation. Looking at the code, all system table writes (users, groups, API keys, permissions) flow through `StorageEngine::store_entry()`. If we intercept there, system tables get replicated automatically. Adding CreateUser/UpdateGroup/etc. as separate RaftRequest variants creates a maintenance nightmare — every new system table feature would need a new variant. The KV-level approach from the design spec already handles this.

**Q2 (Auth bootstrap):** This is a real chicken-and-egg problem. The JWT signing key is stored in system tables via `store_entry()`. My proposal: during `--join`, the joining node does NOT generate its own signing key. Instead, step 1 of the join handshake is fetching the cluster's signing key from the leader via a dedicated `/raft/join-handshake` endpoint authenticated by the cluster secret. The signing key must exist before the node processes any client requests.

**Q3 (GC coordination):** GC should ONLY run on the leader, and it should go through Raft like any other mutation. GC produces `MarkDeleted` operations — those are already in our RaftRequest enum. Running GC on followers is dangerous because they could delete chunks that the leader or other followers still reference. Leader-only GC through Raft guarantees consistency.

**Q4 (Event bus):** Events should be emitted locally when `apply()` runs on the state machine. Since every node applies every committed entry, every node naturally generates events for every mutation. No forwarding needed — the state machine's apply method IS the event source on each node.

**Q5 (Task/cron):** Leader-only for anything that mutates state. Idempotent read-only tasks (like metrics collection) can run anywhere.

**Q6 (Hot file recovery):** In cluster mode, Raft recovery replaces hot file recovery. The Raft log IS the WAL. On restart, openraft replays uncommitted entries. We should disable the hot file mechanism when `cluster_mode == true` to avoid double-applying operations.

**Q7 (Chunk transfer reliability):** The design's peer-fetch approach works, but we need a "chunk availability bitmap" per node. If no node has the chunk (all crashed), the operation is committed but un-applicable. We need a protocol for the re-elected leader to detect "stuck" operations and either retry or abort them.

**Q8-10:** I'll address these next round — they need more implementation analysis.

The biggest engineering risk I see: **the WriteBatch interception** (design spec Open Question #1). DirectoryOps calls `store_entry()` multiple times per operation. We need a transactional batch mode where we collect ops, then submit the batch through Raft atomically. This is the critical path.

### Cynic

The Engineer wants to intercept at `store_entry()` and calls it the "cleanest path." Let me tell you where this falls apart.

**Q1 (KV-level interception is a trap):** You said "intercept at store_entry() and system tables get replicated automatically." No. What you get is a stream of raw KV ops with no semantic context. When a follower's `apply()` fails halfway through a batch of 15 KV ops (disk full, corruption, whatever), what happens? You have half a FileRecord and orphaned chunks on that follower. You have no idea which *logical* operation these KV ops belonged to, so you can't retry or roll back intelligently. The design spec already acknowledged this with the Batch variant, but even Batch doesn't help if the individual ops inside are opaque byte blobs. You need error recovery semantics, and raw KV ops give you nothing to work with.

**Q2 (Auth bootstrap is worse than you think):** Your `/raft/join-handshake` endpoint is authenticated by the cluster secret. Fine. But the cluster secret itself — where is it stored? If it's in system tables, you need the engine running to read it. If it's a CLI flag, it's in process memory and plaintext in shell history. And here's the real problem: between the moment a node receives the signing key and the moment Raft replication is fully caught up, that node has a valid signing key but *stale auth data*. It could validate a JWT for a user that's been deactivated on the leader. The window is small but real.

**Q3 (Leader-only GC is naive):** If GC only runs on the leader, and the leader crashes mid-GC, the new leader picks up with a partially-GC'd state replicated to some followers but not others. The Raft log guarantees ordering, sure, but GC is a scan-and-delete operation — it's not a single atomic batch. How big is a GC batch? The entire unreachable set? That could be enormous. And during that batch, the cluster is blocked on a massive Raft entry.

**Q6 (Hot file vs Raft):** You said "disable hot file in cluster mode." What about the transition? A standalone node with hot file data joins a cluster. Does the hot file get replayed first? Or discarded? If replayed, those ops aren't in the Raft log, so followers never see them. If discarded, you lose data. This is a data loss edge case hiding in the migration path.

**Q7 (Chunk transfer is the real bomb):** "Chunk availability bitmap" sounds nice on paper. In practice: leader commits StoreChunkRef, sends it to followers, then dies. The chunk data was only on the leader's disk. No follower has it. No peer has it. The operation is committed in the Raft log — you cannot uncommit it. The new leader's `apply()` blocks forever waiting for a chunk that no longer exists anywhere. Your cluster is now stuck. This isn't an edge case — it's a guaranteed failure mode under leader crash during large file upload. What's the actual recovery plan?

I need concrete answers on Q1 error recovery and Q7 chunk orphaning before I'll agree to anything.

### Security Officer

The Cynic raises valid concerns, and several of them are security-critical. Let me rank the open questions by threat severity and address the ones that could lead to data corruption, unauthorized access, or cluster compromise.

**Q2 (Auth bootstrap) — CRITICAL SEVERITY.** The Cynic barely scratched the surface. Looking at the actual code in `auth/provider.rs`, `load_or_create_jwt_manager()` unconditionally generates a signing key on first boot if one does not exist. In a cluster join scenario, there is a race window: the joining node's HTTP server starts, it has no signing key yet (or generates its own), and a client hits that node before the Raft-replicated signing key arrives. That client gets a JWT signed by a key that will be thrown away. Worse: if the joining node generates its own key and persists it before the leader's key replicates, we have key conflict. **My requirement:** the join handshake MUST complete — including signing key transfer — before the joining node opens its HTTP port to clients. No requests accepted until the node is fully caught up. This is non-negotiable.

Additionally, the cluster secret as a `--cluster-secret` CLI flag is unacceptable for production. Shell history, `/proc/PID/cmdline`, `ps aux` — all expose it. The flag is fine for dev, but we need `--cluster-secret-file` that reads from a file with 0600 permissions, and we should zero the memory after reading.

**Q7 (Chunk orphaning) — CRITICAL SEVERITY.** The Cynic is right: this is a stuck-cluster scenario. A committed-but-unfetchable StoreChunkRef permanently blocks `apply()` on followers. My proposal: **the leader MUST NOT commit a StoreChunkRef until at least one other node has confirmed chunk receipt.** This is a two-phase approach: (1) leader pushes chunk data to a quorum of followers, (2) only then submits the StoreChunkRef to Raft. If the leader dies after step 1, the chunk exists on enough followers for the new leader to serve it. This adds latency for large files but prevents a permanent stuck state.

**Q1 (KV-level interception) — HIGH SEVERITY.** I agree with the Cynic that opaque KV ops make forensics and rollback impossible. But the Engineer is right that per-mutation RaftRequest variants are a maintenance burden. **Compromise:** wrap each batch with a semantic tag (an enum label like `FileStore`, `UserCreate`, etc.) but keep the payload as KV ops. This gives us audit logging and error context without N variants.

**Q3 (GC) — MEDIUM SEVERITY.** Leader-only GC is correct from a security standpoint. A compromised or buggy follower running GC could delete chunks still referenced by the leader. But the Cynic's point about GC batch size is valid. GC should run in bounded batches (e.g., 1000 deletions per Raft entry) with leader-only coordination.

**Q6 (Hot file) — DATA INTEGRITY RISK.** During standalone-to-cluster migration, the hot file MUST be replayed and committed to the Raft log before the node advertises cluster membership. Discarding it means data loss. Replaying it outside Raft means state divergence.

The two items I will block consensus on until resolved: (1) joining nodes must not serve client traffic until fully synchronized, and (2) StoreChunkRef must not be committed until chunk data is durable on a quorum.

### QA Tester

Good points from all three of you, but I'm hearing a lot of "should work" and not enough "here's how we prove it." Let me map the test gaps I see.

**Q1 (KV-level vs. semantic):** The Cynic's partial-batch-failure scenario is untestable with raw KV ops. How do I write a test that says "inject disk-full after op 7 of 15 in a batch, verify rollback"? With semantic tagging (the Security Officer's compromise), I can at least assert "no partial FileStore batches exist on any node." I need that semantic label or my failure-mode tests are blind. **Verdict: I back the tagged-batch approach because it's testable.**

**Q2 (Auth bootstrap):** The Security Officer's requirement — no client traffic until fully caught up — is the only testable invariant. I can write a test: spin up a joining node, fire requests at it during sync, assert they all get rejected. But the "stale auth window" the Cynic raised? That's a race condition test I need to write: deactivate a user on the leader, immediately hit a follower with that user's JWT, verify rejection. This test will intermittently pass with stale reads. **We need a defined consistency guarantee for auth data specifically**, or this test is unfalsifiable.

**Q3 (GC):** Leader-only GC through Raft in bounded batches — how do I test that a follower catching up doesn't lose chunks mid-GC? I need a test harness that can: (1) pause replication to a specific follower, (2) run GC on the leader, (3) resume replication, (4) verify the follower converges. This is the hardest test to build. We need an injectable delay/pause in the network layer, or this is untestable.

**Q7 (Chunk orphaning) — this is the one I lose sleep over.** The Security Officer's two-phase approach (push chunks to quorum before Raft commit) is the only path I can verify. But I need to test: leader crashes BETWEEN chunk push and Raft commit. The chunks exist on followers but the operation never committed. Are those orphaned chunks cleaned up? That's a new GC edge case. My test matrix for Q7 alone:
- Leader crashes after chunk push, before commit: verify chunks are eventually GC'd
- Leader crashes after commit, before all followers fetch: verify new leader serves chunk
- All nodes with chunk crash simultaneously: verify cluster detects stuck state
- Network partition splits chunk-holders from non-holders: verify recovery on heal

**Existing test coverage gap:** The current test suite (`raft_spec.rs`) only tests single-node operations. Zero multi-node tests. Zero network failure tests. Zero partition tests. We need a multi-node test harness before any of these design decisions can be validated. I propose Phase 9 (Testing) should actually be Phase 0.5 — build the harness first, then implement against it. TDD for distributed systems.

**My blocking requirements:** (1) semantic batch tagging for testable failure recovery, (2) injectable network delays in the test harness, (3) defined auth consistency guarantee.

### Minimalist

Four speakers in and we're already designing a distributed systems PhD thesis. Let me strip this back to what actually needs to ship in v1.

**Q1 (System table replication):** The Engineer is right. Intercept at `store_entry()`. The Cynic's partial-batch-failure concern is real but the Security Officer's compromise — semantic tags on batches — is over-engineering for v1. Here's why: if `apply()` fails on a follower, *the follower is broken*. Full stop. You don't partially recover a Raft state machine — you snapshot-restore it. Raft guarantees: if it's committed, every node applies it or gets replaced. Stop designing intra-batch rollback logic. **v1: raw KV batches, no semantic tags.** If a follower fails to apply, it requests a full snapshot. That's what Raft snapshots are FOR.

**Q2 (Auth bootstrap):** Security Officer nailed it: no client traffic until caught up. That's it. That's the entire design. Don't over-complicate the join handshake. Node starts, joins Raft, receives snapshot (which includes the signing key in system tables), THEN opens HTTP port. One rule, zero special endpoints for key transfer.

**Q3 (GC):** Leader-only, bounded batches, through Raft. The Cynic's "what if leader crashes mid-GC" concern applies to literally every Raft operation. That's what Raft handles. Next.

**Q4 (Event bus):** Engineer is right — events emit locally in `apply()`. Zero forwarding needed. Done.

**Q5 (Tasks/cron):** Leader-only for mutations. Everything else runs locally. One boolean check. Done.

**Q6 (Hot file):** Disable in cluster mode. The Cynic's migration concern is valid: replay hot file BEFORE initializing Raft on standalone-to-cluster transition. One-time operation. Not an ongoing concern.

**Q7 (Chunk transfer):** The Security Officer's two-phase approach adds significant complexity. For v1: accept that if ALL nodes holding a chunk die simultaneously, that data is lost. This is the same guarantee as any replicated system — quorum survival. Don't build a pre-commit chunk distribution protocol. If the leader dies after commit but before followers fetch, followers fetch from the *new* leader (who was in the quorum that committed). If no one has it, you've lost quorum, and you have bigger problems.

**Q8-10:** Defer detailed design to implementation phase. Streaming snapshots — use openraft's built-in mechanism. Standalone migration — initialize as single-node Raft cluster with existing state as snapshot. Large files — the chunk-ref approach in the spec already handles this.

**What I want to CUT from v1:** Semantic batch tagging, two-phase chunk pre-distribution, injectable test harness delays (use timeouts instead), mTLS (shared secret is fine for v1), linearizable reads (stale reads only for v1 — add `?consistency=leader` in v2).

QA Tester: you can test failure modes with process kills and network partitions via iptables. You don't need an "injectable delay framework." Keep the test harness simple.

## Round 2

### Engineer

Good round. Let me respond to the key tensions and address Q8-10.

**On Q1 (batch failure semantics):** The Minimalist is correct. I'm siding against semantic tags for v1. If `apply()` fails on a follower, that follower is in a bad state and needs a snapshot restore. Raft's guarantee is that committed entries are applied in order — it's not a transactional database within the state machine. Building intra-batch rollback is fighting Raft instead of using it. The QA Tester can verify this with a straightforward test: corrupt a follower's disk mid-apply, verify it detects divergence and requests a snapshot.

**On Q2 (auth):** I agree with the Minimalist's simplification. The snapshot-first-then-serve approach eliminates the entire key transfer problem. Concretely: the joining node's startup sequence is (1) start Raft, (2) receive snapshot from leader (which contains the signing key in system tables), (3) apply any remaining log entries to catch up, (4) THEN bind the HTTP port. No special endpoint needed. The Security Officer's `--cluster-secret-file` is a good addition for production — trivial to implement, so yes include it.

**On Q7 (chunk orphaning):** I've thought about this more carefully. The Security Officer's two-phase approach is overkill. Here's why: StoreChunkRef only appears inside a Batch alongside the metadata ops. The leader has the chunk data when it commits. If the leader dies, the new leader was part of the committing quorum — it received the Raft log entry. But it might NOT have the chunk data. This is the real gap. **My revised proposal:** the leader pushes chunk data to followers as part of AppendEntries — not as a separate phase, but inline. For chunks above the 64KB threshold, the leader streams chunk data to followers BEFORE submitting the Raft entry. If the leader dies mid-push, the entry never committed, no harm done. If it dies after commit, at least one other quorum member has the data. This is simpler than two-phase and doesn't require a new protocol.

**Q8 (Large snapshot transfer):** openraft supports streaming snapshots natively via `install_snapshot`. Our `export_version` already produces a serializable bundle. The implementation: pipe `export_version` output through a chunked HTTP stream to the joining node's `install_snapshot` handler. The cluster continues operating during transfer — openraft handles this. The only concern is export locking: we need a read-only snapshot of the KV store (which redb supports via read transactions) so the export doesn't block writes.

**Q9 (Standalone to cluster):** Concrete steps: (1) replay any hot file entries, (2) initialize a single-node Raft cluster with `raft.initialize({self})`, (3) the current engine state becomes the implicit "state at log index 0," (4) build and store a Raft snapshot of the current state, (5) begin normal Raft operation. Future nodes join and receive this snapshot. No data migration needed — the existing .aeordb file IS the state.

**Q10 (Big file):** The chunk-ref approach already solves this. A 1GB file becomes ~4000 StoreChunkRef entries (just hashes + sizes, ~100 bytes each) in one Batch. The Raft log entry is ~400KB, not 1GB. Chunk data flows through the data plane. This is already in the design spec and I'm satisfied with it.

**I'm ready to call for consensus.** After two rounds, I think we're converging. Let me propose consolidated positions in my next turn if the others agree we're close.

### Cynic

The Engineer wants consensus. Not yet. Let me score my original concerns.

**Q1 (Batch failure) — SATISFIED.** The Minimalist made the winning argument: a follower that fails `apply()` is broken, period. Snapshot-restore it. Raft already handles this. I was over-thinking intra-batch recovery when the protocol-level answer is "replace the node's state." I withdraw my objection. Raw KV batches for v1.

**Q2 (Auth bootstrap) — SATISFIED.** Snapshot-first-then-serve is clean. No client traffic until the snapshot lands and the signing key is in the system tables. `--cluster-secret-file` for production. The stale-auth-data window I raised is eliminated because the node doesn't serve traffic until caught up. Agreed.

**Q7 (Chunk orphaning) — PARTIALLY SATISFIED, needs clarification.** The Engineer's revised proposal: "leader streams chunk data to followers BEFORE submitting the Raft entry." This is better than two-phase, but I need to nail down the semantics. If the leader pushes chunk data to followers, then submits the Raft entry, and the entry commits — we're relying on the fact that any node in the committing quorum already has the chunk data. But "quorum" in Raft means the leader plus a majority. A 3-node cluster commits with 2 nodes. If the leader pushed chunks to node2, submitted the Raft entry, leader and node2 commit it — node3 didn't get the chunk push yet. Leader dies. Node2 becomes leader. Node3 needs the chunk. Node2 has it. Fine. But what if node2 also dies before node3 fetches? Now node3 has a committed entry it can't apply. This is the "lost quorum" scenario the Minimalist dismissed as "you have bigger problems." Fair — if you lose quorum, the cluster is already down. I'll accept this for v1 as long as we document it: **chunk availability has the same durability guarantee as Raft quorum. Lose quorum, potentially lose recent chunk data.**

**Q3 (GC) — SATISFIED with bounded batches.** Leader-only, bounded, through Raft. The crash-mid-GC scenario is just a normal Raft partial-replication situation. Agreed.

**Q6 (Hot file) — SATISFIED.** Replay before Raft init on migration. Disable in cluster mode. Clear.

**Remaining concern — Q10 implementation risk.** Everyone brushed past this. The Batch containing 4000 StoreChunkRef entries at ~100 bytes each is ~400KB in the Raft log. Fine. But the leader must stream ~1GB of chunk data to followers BEFORE committing. During that streaming window, the client is waiting for the upload response. If a follower is slow (network congestion, disk I/O), the client timeout could fire before the Raft entry even gets submitted. What's the client-facing timeout strategy for large file uploads in cluster mode? This needs an answer, even if it's "the client should use chunked upload with per-chunk acknowledgment" (which we already have via the batch-commit flow).

**My verdict:** I can agree to consensus IF (1) the quorum-loss documentation for Q7 is explicit, and (2) we acknowledge the large-file latency cost in cluster mode. These aren't blockers — they're documentation requirements.

### Security Officer

Round 2 responses. I will address the convergence and evaluate whether my blocking requirements from Round 1 are met.

**Q2 (Auth bootstrap) — BLOCKING REQUIREMENT MET.** The snapshot-first-then-serve approach satisfies my concern completely. If the joining node receives the full snapshot (which includes the JWT signing key in system tables) before opening its HTTP port, there is no window for key conflict or stale-auth exploitation. Combined with `--cluster-secret-file` for production, I am satisfied. One addition: the joining node MUST verify the signing key exists in its state after snapshot install, and refuse to open the HTTP port if it does not. Defense in depth.

**Q7 (Chunk orphaning) — BLOCKING REQUIREMENT REVISED, NOW MET.** The Engineer's push-before-commit approach is functionally equivalent to my two-phase proposal but simpler. The Cynic correctly identified that durability is bounded by quorum — if you lose quorum, chunks may be unrecoverable. I accept this for v1. This is the same durability model as Raft itself: lose quorum, lose availability and potentially recent writes. However, I want one safeguard: **the follower's apply() must have a bounded timeout on chunk fetch, with a clear error state.** A permanently blocking apply() is worse than a detected failure. If a follower cannot fetch a chunk within N seconds, it should mark itself as needing a snapshot restore rather than hanging indefinitely.

**Q1 (Batch failure) — CONCESSION.** I withdraw my push for semantic tagging in v1. The Minimalist's argument is correct: Raft's answer to a broken follower is snapshot-restore. Intra-batch recovery is unnecessary complexity. For v2, I still want audit logging with semantic context, but that can be layered on later.

**Q10 (Large file latency) — NEW SECURITY CONCERN.** The Cynic raised a valid point about client timeouts during large uploads. From a security perspective, a slow-follower forcing upload timeouts is a denial-of-service vector. A malicious or degraded node in the cluster could slow-walk chunk receipt and effectively block all large file writes. **Mitigation:** the leader should push chunks to followers asynchronously and commit the Raft entry once it has pushed to a quorum (not all nodes). Slow followers catch up later via the chunk fetch endpoint. The leader should track which followers have which chunks and avoid waiting for stragglers.

**Remaining items (Q3-Q6, Q8-Q9):** I agree with the converged positions. Leader-only GC in bounded batches through Raft. Events from apply(). Hot file replay before Raft init. Streaming snapshots via openraft. Standalone migration as single-node Raft init.

**My consensus position:** I am ready to agree, contingent on two documentation/implementation requirements: (1) bounded timeout on chunk fetch in apply() with fallback to snapshot-restore, and (2) the leader's chunk push targets a quorum, not all followers, to prevent slow-node DoS. These are not new features — they are implementation details within the existing design. If the group accepts these, I support consensus.

### QA Tester

We're converging and I'm ready to evaluate whether each decision has a viable test strategy.

**Q1 (Raw KV batches, no semantic tags) — I CONCEDE for v1.** The Minimalist and Cynic are right: if apply() fails, the answer is snapshot-restore, not intra-batch rollback. My test for this is clean: kill a follower mid-apply (SIGKILL during disk write), restart it, verify it detects inconsistency and requests a snapshot, verify it converges to the correct state. Testable without semantic tags. I withdraw my requirement.

**Q2 (Snapshot-first-then-serve) — SATISFIED.** My test: start a 3-node cluster, add node4 via --join, immediately fire 100 HTTP requests at node4 during sync. Assert ALL get connection-refused or "not ready" responses. After sync completes, verify node4 accepts requests and validates JWTs signed by the leader. This is a deterministic test I can write today.

**Q7 (Push-before-commit with bounded timeout) — SATISFIED with the Security Officer's additions.** The bounded timeout on chunk fetch in apply() is critical for my test harness. Here's my concrete test matrix, now simplified:
- **Normal path:** Upload 10MB file to leader, verify all followers have it within 5s
- **Slow follower:** Inject 2s network delay to one follower, verify commit still succeeds (quorum-based), slow follower catches up later
- **Leader crash post-commit:** Kill leader after client gets 200, verify new leader serves the file, verify remaining follower fetches chunks from new leader
- **Chunk fetch timeout:** Block chunk endpoint on all peers for one follower, verify it marks itself for snapshot-restore after timeout (does NOT hang forever)

The Security Officer's point about quorum-targeted chunk push is important for testability — it gives me a clean invariant: "committed chunks exist on at least quorum-count nodes." I can verify this deterministically.

**Q10 (Large file latency) — needs one test.** Client-side timeout during large upload in cluster mode. My test: upload a 500MB file to a 3-node cluster, one follower artificially throttled to 1MB/s. Verify the client gets a 200 (because leader only waits for quorum), verify the slow follower eventually converges.

**On the test harness question (Minimalist told me to use iptables):** Fair. I do NOT need an injectable delay framework. But I DO need these capabilities in the test harness: (1) start/stop individual nodes programmatically, (2) partition specific node pairs (iptables or netns), (3) inject latency on specific links (tc/netem). These are standard tools. I can build this as a shell-script-based integration test suite that runs against real AeorDB binaries.

**My blocking requirements for consensus — ALL MET:**
1. Semantic batch tagging — withdrawn, snapshot-restore is the answer
2. Injectable network delays — iptables/netem suffices, no framework needed
3. Auth consistency guarantee — satisfied by snapshot-first-then-serve

**I am ready for consensus.** My one addendum: the Phase 9 test plan in the design spec should be updated to include the specific test scenarios we discussed here (chunk fetch timeout, slow follower convergence, mid-apply crash recovery, join-before-ready rejection).

### Minimalist

Everyone has conceded their overengineered positions and we're converging. I'm calling for consensus. Let me propose the simplest viable position for all 10 questions.

---

**CONSENSUS PROPOSAL**

**Q1: System table replication** — Intercept at `store_entry()`. Raw KV batches through Raft. No semantic tags. If a follower's `apply()` fails, it snapshot-restores. No intra-batch rollback.

**Q2: Auth in cluster mode** — Snapshot-first-then-serve. Joining node receives full Raft snapshot (which includes JWT signing key in system tables), verifies signing key exists, THEN opens HTTP port. No special key-transfer endpoints. Support both `--cluster-secret` (dev) and `--cluster-secret-file` (production).

**Q3: GC coordination** — Leader-only. Bounded batches (cap per Raft entry, e.g. 1000 deletions). Through Raft like any other mutation. Followers never run GC.

**Q4: Event bus** — Events emitted locally in `apply()`. Every node applies every committed entry, so every node generates events naturally. No cross-node forwarding.

**Q5: Task/cron** — Leader-only for state-mutating tasks. Read-only tasks (metrics, health) run locally on each node.

**Q6: Hot file recovery** — Disabled in cluster mode (Raft log replaces WAL). On standalone-to-cluster migration: replay hot file first, then initialize Raft. One-time operation.

**Q7: Chunk transfer reliability** — Leader pushes chunk data to quorum of followers BEFORE submitting the Raft entry. Not a separate protocol phase — just a pre-commit data push. Followers' `apply()` has a bounded timeout on chunk fetch; if it exceeds the timeout, the follower marks itself for snapshot-restore instead of hanging. Chunk durability equals quorum durability. Lose quorum, potentially lose recent chunks. Document this explicitly.

**Q8: Large snapshot transfer** — Use openraft's built-in streaming `install_snapshot`. Pipe `export_version` through chunked HTTP. Leader uses a read-only transaction so writes aren't blocked during export.

**Q9: Standalone to cluster migration** — Replay hot file, init single-node Raft cluster, existing engine state is "state at log index 0," build initial Raft snapshot, begin normal operation. Joining nodes receive this snapshot.

**Q10: Big file problem** — Already solved by chunk-ref design. 1GB file = ~400KB Raft log entry (hashes only). Chunk data flows through data plane. Leader pushes to quorum before commit (same as Q7). Client timeout concern addressed by quorum-only push — leader doesn't wait for slow followers.

**What's explicitly OUT of v1:** Semantic batch tagging (v2 for audit logging), mTLS (shared secret sufficient for v1), linearizable reads (stale reads only, `?consistency=leader` deferred to v2), custom test delay framework (use iptables/netem).

**Phase 9 test plan update:** Add the specific scenarios from this debate — chunk fetch timeout, slow follower convergence, mid-apply crash recovery, join-before-ready rejection, quorum-loss documentation.

---

I need agreement from all four of you. Engineer, your turn.

## Consensus Round

### Engineer

**I AGREE with the Minimalist's consensus proposal.** Let me confirm each point from an implementation feasibility perspective:

**Q1-Q6:** All straightforward to implement. The store_entry() interception is the highest-effort item but architecturally clean. I'll implement a `WriteBatch` collector that DirectoryOps writes into, then submit the batch through Raft. Estimated ~2 weeks of work for the write path integration (Phase 4).

**Q7:** The push-before-commit approach maps cleanly to the existing chunk transfer endpoint. Implementation: before calling `raft.client_write(batch)`, the leader iterates StoreChunkRef entries in the batch, pushes each chunk to N/2 followers via `POST /raft/chunks/{hash}`, then submits. The bounded timeout in `apply()` is easy — wrap the chunk fetch in `tokio::time::timeout()`. If it fires, set a `needs_snapshot` flag and return an error. Clean.

**Q8-Q9:** Both leverage existing mechanisms (export_version, redb read transactions). Low implementation risk.

**Q10:** Already handled by the chunk-ref design. No additional work.

**Deferred items (v2):** Agreed. Semantic tagging, mTLS, and linearizable reads are cleanly separable and don't affect v1 architecture.

**One implementation note for the record:** the biggest dependency chain is Phase 1 (disk-backed log store) -> Phase 2 (RaftRequest expansion) -> Phase 4 (write path). Phase 3 (network) can be developed in parallel with Phase 2. This is the critical path for the project.

I support this consensus. Pass to Cynic.

### Cynic

**I AGREE with the consensus proposal.** My concerns are addressed.

Let me confirm explicitly what satisfied each of my objections:

1. **Q1 partial-batch failure** — Snapshot-restore is the answer. I was inventing complexity where Raft already had the solution. Withdrawn.

2. **Q2 stale auth window** — Eliminated by snapshot-first-then-serve. No traffic until caught up. Dead.

3. **Q3 GC crash mid-batch** — Bounded batches through Raft. Normal Raft recovery semantics apply. No special handling needed.

4. **Q6 hot file migration** — Replay before Raft init. One-time, deterministic, no data loss. Accepted.

5. **Q7 chunk orphaning** — Push-to-quorum before commit. Bounded timeout in apply() with fallback to snapshot-restore. Durability equals quorum durability, documented explicitly. This was my biggest concern and the combination of push-before-commit plus the timeout fallback covers it. The remaining failure mode (lose quorum, lose chunks) is inherent to any quorum-based system and is the correct tradeoff for v1.

6. **Q10 client timeout during large uploads** — Quorum-only push means the leader doesn't wait for stragglers. The existing chunked-upload/batch-commit flow gives clients per-chunk acknowledgment for very large files. Acceptable.

**The one thing I want on the record:** the v1 "deferred items" list (semantic tagging, mTLS, linearizable reads) should be tracked as a v2 requirements document, not just a footnote. These aren't nice-to-haves — they're security and observability features that are necessary for production multi-tenant deployments. Deferring them for v1 is correct; forgetting them for v2 would be negligent.

Consensus supported. Pass to Security Officer.

### Security Officer

**I AGREE with the consensus proposal.** All of my blocking requirements have been addressed.

Let me confirm the security posture of the final design:

1. **Auth boundary integrity (Q2) — SECURED.** Snapshot-first-then-serve eliminates the signing key race condition. No client traffic until the node has a valid, cluster-consistent signing key. `--cluster-secret-file` for production prevents credential leakage via process listing. The joining node verifies signing key presence before opening the HTTP port. This is a clean, defensible auth boundary.

2. **Data integrity under failure (Q7) — ACCEPTABLE RISK for v1.** Push-to-quorum before Raft commit ensures chunk data survives leader failure as long as quorum survives. Bounded timeout in apply() prevents hung followers — they fall back to snapshot-restore. The documented invariant is clear: chunk durability equals quorum durability. This is the same guarantee the Raft log itself provides.

3. **GC as an attack surface (Q3) — MITIGATED.** Leader-only GC through Raft means a compromised follower cannot trigger deletions. Bounded batches prevent a single GC cycle from creating an oversized Raft entry that could be used as a resource-exhaustion attack.

4. **Cluster membership trust (implicit) — ADEQUATE for v1.** Shared cluster secret authenticates inter-node communication. This prevents unauthorized nodes from joining. For v2, mTLS adds transport-layer verification and certificate-based identity, which is necessary for zero-trust environments. The Cynic is right: track mTLS as a v2 requirement, not a footnote.

**My additions to the record:**

- **v2 security requirements (non-negotiable for production multi-tenant):** mTLS for inter-node communication, audit logging with semantic context on Raft operations, rate limiting on /raft/* endpoints to prevent amplification attacks, and signing key rotation protocol (currently the key is generated once and never rotated).

- **Documentation requirement:** The operational guide must explicitly state that losing quorum means potential data loss for in-flight writes, including chunk data. Operators must understand that a 3-node cluster tolerates 1 node failure, not 2, and that this applies to chunk availability as well as Raft log durability.

Consensus supported. Pass to QA Tester.

### QA Tester

**I AGREE with the consensus proposal.** Every decision now has a verifiable test strategy.

Let me confirm the test plan maps cleanly to each consensus item:

1. **Q1 (Raw KV batches):** Test — SIGKILL follower mid-apply, verify snapshot-restore convergence. No semantic tags needed.
2. **Q2 (Snapshot-first-then-serve):** Test — fire requests at joining node during sync, assert all rejected. Post-sync, verify JWT validation with leader-signed tokens.
3. **Q3 (Leader-only GC):** Test — pause replication via netem, run GC on leader, resume, verify follower converges. Verify followers never execute GC independently.
4. **Q4 (Events from apply):** Test — subscribe SSE on follower, write on leader, verify follower emits event after apply.
5. **Q5 (Leader-only tasks):** Test — verify mutation tasks only fire on leader node. Verify they stop when leadership transfers.
6. **Q6 (Hot file disabled in cluster):** Test — create standalone node with hot file data, migrate to cluster, verify data survives in Raft log. Verify hot file mechanism is disabled post-migration.
7. **Q7 (Push-before-commit + bounded timeout):** Four-scenario test matrix from my Round 2 response. All testable with process kills and netem.
8. **Q8 (Streaming snapshots):** Test — add node to cluster with 1GB of data, verify snapshot transfer completes and node converges. Verify cluster serves writes during transfer.
9. **Q9 (Standalone migration):** Test — populate standalone node, migrate, join second node, verify second node receives all data via snapshot.
10. **Q10 (Large files):** Test — upload 500MB file to 3-node cluster with one throttled follower, verify client gets 200, verify slow follower catches up.

**I second the Cynic's point:** the v2 deferred items (semantic tagging, mTLS, linearizable reads, key rotation) must be a tracked requirements document, not a footnote. From a testing perspective, those features each require their own test matrices and should be designed with testability in mind from the start.

**CONSENSUS REACHED.** All five debaters agree on the consolidated position for all 10 questions.

### Minimalist

All four agreed. No objections, no counters. We're done.

**CONSENSUS REACHED**

---

## Final Consensus Summary

### Decided Positions (v1)

| # | Question | Decision |
|---|----------|----------|
| 1 | System table replication | Intercept at `store_entry()`, raw KV batches through Raft, snapshot-restore on apply failure |
| 2 | Auth in cluster mode | Snapshot-first-then-serve; no HTTP port until snapshot received and signing key verified; `--cluster-secret-file` for production |
| 3 | GC coordination | Leader-only, bounded batches (e.g. 1000 deletions), through Raft |
| 4 | Event bus | Events emitted locally in `apply()`; no cross-node forwarding |
| 5 | Task/cron | Leader-only for mutations; read-only tasks run locally |
| 6 | Hot file recovery | Disabled in cluster mode; replay before Raft init on migration |
| 7 | Chunk transfer reliability | Push chunk data to quorum before Raft commit; bounded timeout on chunk fetch in `apply()` with fallback to snapshot-restore; durability = quorum durability (documented) |
| 8 | Large snapshot transfer | openraft streaming `install_snapshot`; read-only transaction for export; non-blocking |
| 9 | Standalone to cluster | Replay hot file, init single-node Raft, existing state = log index 0, build snapshot |
| 10 | Big file problem | Chunk-ref design (hashes in Raft log, data via data plane); quorum push before commit |

### Explicitly Deferred to v2
- Semantic batch tagging (audit logging)
- mTLS for inter-node communication
- Linearizable reads (`?consistency=leader`)
- Signing key rotation protocol
- Rate limiting on `/raft/*` endpoints

### Documentation Requirements
- Quorum-loss = potential data loss for in-flight writes (including chunks)
- 3-node cluster tolerates 1 failure, not 2
- v2 deferred items tracked as a requirements document

### Critical Path
Phase 1 (disk-backed log) -> Phase 2 (RaftRequest expansion) -> Phase 4 (write path). Phase 3 (network) parallel with Phase 2.
