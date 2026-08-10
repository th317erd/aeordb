# AeorDB NVT Refactor Planning Conversation

## Round 1: GC Quarantine Representation and Header Capacity

**Status:** Answered by Wyatt; reconciled in Round 2 below.

This round is deliberately limited to category #1: where GC quarantine state
lives, whether the existing persisted formats have enough reserved space, and
whether the already-large migration should introduce a new common entity header.

### What The Current Formats Actually Reserve

| Persisted structure | Scope | Current size under Blake3 | Unused capacity | Relevant constraint |
| --- | --- | ---: | ---: | --- |
| Database `FileHeader` v3 | One per database, stored in two A/B slots | 256 bytes per slot | About 55 bytes per slot with Blake3 | Enough for some **database-global** fields, but cannot hold per-entity GC state. The current layout cannot fit its three stored hashes if a future 64-byte hash algorithm is activated. |
| WAL `EntryHeader` v0 | One per Chunk, FileRecord, DirectoryIndex, Snapshot, Fork, Symlink, DeletionRecord, and Void | 63 bytes (31 fixed + 32-byte hash) | **0 bytes** | Key/value bytes begin immediately after the hash. Expanding it changes every following offset and requires rewriting the database. |
| Disk KV locator | One per live KV key | 45 bytes (1 flag/type + 32-byte hash + 8-byte offset + 4-byte length) | Two unused flag bits, but no spare bytes | The bits can encode a small state/color, not a timestamp or generation. Changing its width changes every fixed-size KV page. |
| Entity payloads | Per entity type | Variable | No intentional common reserve | Many are content-addressed. GC metadata does not belong in payload bytes because changing it would change entity identity. |

Two additional facts constrain the design:

1. `EntryHeader.timestamp` cannot be reused. Dirty recovery uses it to decide
   which mutable-key revision is newest after void reuse makes physical offsets
   non-chronological.
2. `EntryHeader.entry_version` currently means the **entity payload schema
   version**, despite its name. FileRecord already uses values 0 and 1, while
   most other payloads only support 0. It is not presently a physical-header
   version byte.
3. Blake3 is the only operational hash implementation today. Although the enum
   lists 64-byte SHA-512 variants, the current 256-byte database header would
   overrun by 41 bytes if one were enabled. Its apparent reserve is therefore
   not enough for the format's stated future hash agility.

### Current Conclusion

The existing reserve is sufficient only if per-entity quarantine timestamps live
in a separate GC metadata representation. If we require the timestamp inside
each common entity header, the current format is not large enough and a
side-by-side database rewrite is unavoidable.

My recommendation is to use the already-anticipated side-by-side rewrite to
introduce a self-describing common entity header with real reserve, but **not**
to make mutable GC quarantine state canonical inside otherwise immutable WAL
entry headers. A versioned, checksummed GC quarantine artifact can batch writes,
publish atomically, and fail conservatively after a crash. The new common header
still buys future breathing room and fixes the existing header-version/payload-
version conflation.

## Direct Questions

### 1. Where should canonical per-entity GC quarantine state live?

Do you want `pending_since_ms` physically mutated inside every entity's WAL
header, or may the engine keep it in a dedicated versioned GC quarantine
artifact bound to the exact current KV-entry incarnation?

**Recommendation:** Use a dedicated GC quarantine artifact as the canonical
record. Updating millions of scattered WAL headers creates random writes and
torn-header hazards. The artifact can be written sequentially, checksummed, and
published with an A/B root. The entity header should remain immutable after an
entry is committed. The quarantine key must include enough identity to prevent a
stale mark from attaching to a replacement entry that happens to reuse a logical
key or content hash.

**Your answer: I agree with you: a separate and dedicated GC artifact is the way to go here.**


### 2. Should this migration introduce a new common entity header anyway?

The common `EntryHeader` currently has zero reserve and conflates physical-header
versioning with payload-schema versioning. Even if question 1 uses a separate GC
artifact, should the side-by-side migration write every live entity with a new
common header format?

**Recommendation:** Yes. We have found two concrete deficiencies, not merely a
vague desire to future-proof: zero extension space and no independent physical
header version. If we are already paying for a side-by-side rewrite, leaving
those deficiencies guarantees another rewrite later.

**Your answer: Yes. I demanded free reserved bytes on EVERY entity header at the start of design, and an _entity version_ (not payload version...) for every entity type. It appears maybe the previous implementation bot didn't follow my instructions... or maybe you are a little confused here? A "payload version" for some entities _is essentially a entity version_. In any case, I can certainly see that we need some reserved bytes here, and some cleanup. Please help me figure out what _might_ be needed in the future, so we can estimate the number of reserved bytes we might need. One thing that might impact this: Our next major feature (after stability) is _content encryption_.**


### 3. Is roughly 97 bytes of common per-entity reserve acceptable?

My proposed budget is a 128-byte fixed prefix followed by the algorithm-sized
entry hash. Under Blake3, the new common header would therefore be 160 bytes
instead of 63 bytes: 97 additional bytes per rewritten entity. At 7.8 million
live entities this costs about 757 MB; at 100 million entities it costs about
9.7 GB. The format would carry an explicit `header_length`, so readers can skip
unknown reserved bytes safely.

**Recommendation:** Accept this budget. It is enough for explicit versions,
feature flags, integrity metadata, and substantial future growth without making
small-file overhead absurd. A 256-byte prefix would be easier to fill but would
waste nearly twice as much space forever. This recommendation assumes question
1 keeps mutable quarantine state in a separate artifact. If quarantine must be
stored inline with crash-safe A/B lifecycle slots, I recommend a 192-byte fixed
prefix instead so GC does not consume most of the reserve on day one.

**Your answer: I am concerned that this might be a little too much, and be wasteful. Let's do a better job at projecting what we might need, based on what we want from this database, and see if we can narrow this down considerably. Keep in mind that this is _also_ the reason we have an "entity version" tagged to everything. The idea is clear: entity version 1 might use 67 bytes for the header, entity version 2 might have 80, and entity version 3 might have 128. This is EXACTLY the use-case of the version. The intent and idea of the design of these datastructures _has always been_: the version bytes come right up-front, along with other important never-changing header information, such that the engine can always read a smaller subset of the header that is known to be canonical... so that once the version is read, we can then use the correct versioned reader in the code to read the remainder of the entity (including the remaining larger header, should one exist).**


### 4. Should the database-level header slots also grow?

Should the database `FileHeader` slots also grow from 256 to 512 bytes during the
side-by-side rewrite? This adds only 512 bytes total per database because there
are two slots, not one allocation per entity.

**Recommendation:** Yes. It gives about 311 bytes of global reserve under
Blake3, still leaves about 215 bytes with three future 64-byte hashes, and makes
room for reader-capability bits and other global migration state. The storage
cost is negligible; changing the leading data offset later is not.

**Your answer: I don't think I fully understand you here... I thought `FileHeader` was used per-file? How can adding an extra 256 bytes per-file add "only 512 total per database"?**


### 5. Should reserve exist once in the common header or inside every payload too?

When you say "reserved header space for all entities," should one expanded
common `EntryHeader` cover every entity type, or should FileRecord, B-tree node,
snapshot, fork, symlink, and future artifact payload formats each add their own
padding too?

**Recommendation:** Reserve space once in the common header. Keep payloads
compact and independently versioned. Several payloads participate in content
identity, so padding them creates larger hashes/objects and still does not help
cross-cutting metadata. A payload should grow only when that entity type gains a
real field.

**Your answer: Yes and no... the "version" for each entity type is to account for that specific type's structure, including the size, and size of the header. Partial migrations, or zero migration with the writing of newly versioned entities is supported, so this needs to be a "per-entity" thing, not a "per-database thing". This version also _technically_ means that we don't have to reserve anything. If we want new header information in the future, we add to the header, bump the version, and walla! However, this DOES NOT account for anything where we need _existing entities to be modified_... such as possible GC, or other operation data needs. I would like to opt for both: the "version" tag on every entity header allows painless future expansion of NEW entities, and a little reserved space on existing entity headers _might just save our butts_ if the need becomes urgent/difficult for some specific future need.**


### 6. May we split physical-header and payload-schema versions?

The new common header would have distinct fields such as:

```text
header_format_version
header_length
payload_schema_version
feature_flags
```

Legacy v0 entries would continue interpreting today's byte as
`payload_schema_version`; migrated entries would use the explicit fields.

**Recommendation:** Yes, and treat this as a required invariant. A FileRecord
payload revision must never accidentally claim that every B-tree, snapshot,
fork, symlink, and deletion payload changed format too.

**Your answer: No, I don't see a need for this. An "entity" (both its header and its payload) are versioned. This version accounts for both, as it should, because the entity is treated as a "whole". If we ever update the payload, that is a version bump to the whole entity, and a new versioned reader would be used to load and parse the entity... negating the need for a header version and payload version. The process is simple (though it requires two reads): Read the header "header" first, which includes the entity version and size, then read the entire entity into memory using the defined size, and use the proper versioned reader to parse the entity. As an optimization, we can skip the dual-read in some cases just by reading the entire entity + some safe amount of extra data beyond what we believe the entity to be. Then, once off disk and in-memory, we can read the real version from the block of memory, and select the correct versioned reader to parse the entity... which will ignore any random trailing bytes we eagerly scarfed into memory.**


### 7. What exact GC mark should be persisted?

Is a single `pending_since_ms: u64` enough for your intended meaning, with zero
meaning "not pending," or do you also want the first complete GC mark generation
persisted?

**Recommendation:** Persist both `pending_since_ms: u64` and
`first_unreachable_generation: u64`. The timestamp is the human/operator-facing
mark and enforces grace. The generation proves that reclamation occurs during a
later complete mark, including when grace is configured to zero. Neither field
is changed by reads or by repeated unreachable scans.

**Your answer: I am okay with two timestamps, as you have recommended here.**


### 8. Which entity types participate in quarantine?

Should quarantine cover every reclaimable current and future entity type, or
only authoritative user-content entities?

**Recommendation:** Cover every reclaimable entity uniformly: chunks, file
records, directory nodes, retired snapshot/fork records once they are no longer
roots, symlinks, and future index artifacts. Exclude Void records and durable
DeletionRecords because they have different lifecycle/recovery roles. Avoid a
second GC path just for derived indexes.

**Your answer: Everything that can be reclaimed should be, yes. There are exceptions, as you call out... one thing I note that you didn't call out is the GC artifacts themselves. Those obviously will be on the "exclusion" list (at least until those artifacts have fully served their purpose, and the resulting data is reclaimed... at which point we obviously won't need the GC artifacts any longer). Thought: It will probably be desirable to have the GC artifacts remain kicking around for a few days even after they have served their purpose. This will allow and admin to audit what was reclaimed (if needed), and will also be useful for debugging the GC. i.e. a few-day lag in purging GC artifacts is probably desirable.**


### 9. What should happen when quarantine metadata is damaged or missing?

Should the engine attempt to infer age from another timestamp, or reset the
affected entity to a non-eligible state?

**Recommendation:** Always fail conservative: missing, corrupt, partially
published, or version-unknown quarantine metadata means "not eligible." Never
infer `pending_since` from entry write time, database update time, or filesystem
mtime. This may leak space for one additional grace period, but cannot delete
live data early.

**Your answer: Agreed. Deletion is a dangerous operation. We should fail in a safe way. If something is corrupt or missing, then we should clear the state, and await a future GC scan to re-mark.**


### 10. How online must the side-by-side rewrite be?

For multi-terabyte production databases, is it acceptable to require the source
database to remain read-only for the entire copy, or must normal writes continue
while the destination is built?

**Recommendation:** Build the destination beside the live source while recording
a bounded, durable mutation journal; then take a short exclusive write window to
drain the journal, verify roots/counts/hashes, and atomically cut over. Requiring
hours or days of read-only time is operationally unrealistic. The migration must
preflight free space and leave the source untouched until explicit finalization.

**Your answer: Agreed. Uptime is critical, and a full clone operation could take a long time. We should clone while the original database remains active, and then the cut-over should be rapid, and only capture whatever small delta might remain.**


## Proof Obligations Already Identified

These are not additional policy questions, but category #1 is not implementable
until its tests prove all of the following:

- golden-byte v0 and new-entity fixtures dispatch each complete
  `(EntryType, EntityVersion)` codec correctly;
- unknown entity versions/header lengths fail clearly without mis-scanning the
  WAL;
- failed or incomplete marks publish no new quarantine state;
- torn/corrupt A/B quarantine publication always chooses the last valid root;
- stale quarantine state cannot attach to a replacement KV-entry incarnation;
- grace zero still requires a later complete mark;
- reachable-again entries clear quarantine only after a complete mark publishes;
- migration preserves entity keys, values, roots, and entity-version semantics;
- crash/restart at every copy, journal, validation, and cutover boundary leaves
  either the untouched source or a fully verified destination usable.

---

## Round 2: Entity Framing, GC Artifact Semantics, and Migration State

**Status:** Answered by Wyatt; reconciled in Round 3 below.

### Corrections and Decisions Locked From Round 1

1. **The GC quarantine state will live in dedicated GC artifacts.** Entity
   headers remain immutable during ordinary GC.
2. **Wyatt's entity-version model is the intended model.** I overstated the
   need for separate header and payload versions. The correct dispatch key is
   `(EntryType, EntityVersion)`, and that codec owns the entire entity: its
   version-specific header and payload. Two versions may happen to share a
   header layout, but that is an implementation detail.
3. The implementation is what diverged. It currently parses every entity using
   one hard-coded 31-byte fixed header, then passes `entry_version` only to the
   payload parser. The refactor must remove the global
   `CURRENT_ENTRY_VERSION` assumption and give every entity type its own current
   version and complete codec.
4. Every reclaimable entity type participates in the same quarantine process.
   GC artifacts are roots while active or retained for audit; after retirement,
   they become ordinary reclaimable entities and pass through quarantine too.
5. Missing or corrupt quarantine state always resets eligibility
   conservatively. It can leak space for another grace period; it can never
   shorten retention.
6. The side-by-side migration must remain online during the long copy and use a
   short exclusive write window only for final delta drain, verification, and
   cutover.

### Clarification: `FileHeader` Is Not A User File's Header

The current Rust name is misleading in this conversation. `FileHeader` is the
header of the **single `.aeordb` database container file**. It appears twice at
physical offsets 0 and 256 as crash-safe A/B slots. A user's file is represented
by a `FileRecord`, and every WAL entity has an `EntryHeader`.

Therefore, increasing each database-header slot from 256 to 512 bytes costs:

```text
2 slots * 256 additional bytes = 512 additional bytes per database
```

It does not add 256 bytes to every user file or entity.

<!-- Feedback from human: This is very confusing... let's please do a small refactor here to rename "FileHeader" to instead be "DatabaseHeader" like it should be. -->

## Direct Questions

### 1. Is a 32-byte per-entity reserve the right budget?

The earlier 97-byte proposal was too generous given the whole-entity version
model. A more focused new header would add:

| Addition | Bytes | Why it is needed now |
| --- | ---: | --- |
| Allocated header length | 2 | Locates the payload and permits later versions to consume reserved bytes without moving it. |
| Header CRC32 | 4 | Detects corruption in lengths, flags, compression/encryption selectors, timestamps, and reserved bytes; the current content hash does not protect these fields. It reuses the checksum family already used by database headers and KV pages. |
| Reserved extension area | 32 | Emergency room for a later entity version that must update existing entities without copying their payloads. |

Under Blake3 this makes a new entity header 101 bytes instead of 63, an
increase of 38 bytes per entity: about 296 MB for 7.8 million entities or 3.8 GB
for 100 million entities.

Content encryption does not require us to reserve its entire future envelope
today. The current header already has an algorithm byte. A future encrypted
entity may additionally need a key generation/ID (roughly 8-16 bytes), a nonce
(12-24 bytes), and perhaps a plaintext length (8 bytes); the authentication tag
normally travels with the ciphertext. Encrypting an existing entity necessarily
rewrites its payload, so that operation can emit a larger entity version and
header at the same time.

**Recommendation:** Reserve 32 bytes, not 97. This is enough to rescue a modest
header-only evolution while keeping millions of small entities economical. Do
not prematurely freeze the encryption envelope before its threat model, key
rotation, deduplication, and streaming design are settled.

**Your answer: Agreed. I like this plan better. This seems much more reasonable to me.**


### 2. Should reserved bytes be explicitly consumable by later entity versions?

Merely writing zero padding does not make it useful. I propose that each entity
version define an `allocated_header_length`. A later version may consume bytes
inside that allocation and bump the entity version without moving the payload.
If it needs more than the allocation, it must append/rewrite the entity normally.

Any in-place header upgrade must be crash-safe: persist an update intent,
rewrite and sync the header, verify its CRC/version, then retire the intent.
Naked random header overwrites are forbidden.

**Recommendation:** Make this a formal format invariant and implement a generic,
tested header-upgrade primitive during this migration. Otherwise the reserve is
decorative padding that cannot safely "save our butts" when needed.

**Your answer: This exposes a concern that you may not have considered: Our database is content hashed, and so if we update the header so much that we expand it, and then have to relocate the entire entity to the end of the WAL, that means we now have TWO entities with the same content-address. This isn't an impossible situation to solve (for example, once the new write is confirmed, we could Void the old entity space immediately). _However_, I just all it out as something that needs consideration.

And to answer the original question: Yes, reserved bytes are intended for explicit consuming by later versions. That is their entire point. Reservation is only for the purpose of future growth and future needs. The reserve bytes are also intended to help _prevent_ fully relocating the entity upon change.**


### 3. With the naming clarified, should database-header slots grow to 512 bytes?

The 256-byte database header has about 55 global spare bytes under Blake3, but
would overrun by 41 bytes if the currently enumerated 64-byte hash algorithms
were ever implemented. It also needs future reader-capability and migration
state, plus a stable database identity for stateless root cursors.

**Recommendation:** Grow both slots to 512 bytes in the side-by-side format and
rename the Rust type to `DatabaseHeader`. The permanent cost is only 512 bytes
per database, while moving the database's leading data boundary again later is
needlessly invasive.

**Your answer: Yes, with the naming clarified, I am totally okay with growing the DatabaseHeader to reserve this extra space you recommend.**


### 4. Do you agree with this entity read contract?

Your small canonical-prefix design is correct, with one adjustment: correctness
must not depend on reading arbitrary bytes from the following entity.

The first read obtains enough bytes for magic, entity version, entity type, and
the version-specific size/framing fields. Then:

- a KV lookup already knows `total_length`, so it can issue one exact `pread`;
- a sequential recovery/verify scan reads the small prefix, dispatches the
  `(type, version)` codec, then reads the exact remainder;
- bounded read-ahead may combine adjacent I/O, but each decoder receives a
  length-bounded slice and ignores bytes outside that slice; and
- large payloads may be streamed. "Entity is one versioned unit" must not imply
  that a potentially huge value is always allocated in one buffer.

**Recommendation:** Adopt this contract. Opportunistic read-ahead is a valid
optimization; over-reading random trailing bytes is not part of the disk-format
correctness model.

**Your answer: Correct. This was just an idea for possible optimization. Hopefully it is never needed. But you are correct in defining that if we _are_ greedily reading for optimization, then the codec should ensure that it will never read beyond entity boundaries.**


### 5. Did you intend one timestamp plus one generation, or two timestamps?

My Round 1 recommendation was:

```text
pending_since_ms: u64
first_unreachable_generation: u64
```

The second value is a monotonically increasing complete-mark generation, not a
timestamp. A possible second timestamp such as `last_confirmed_unreachable_ms`
would change on every scan and add writes without affecting eligibility; each
GC run's own start/completion timestamps already provide that audit trail.

**Recommendation:** Persist one timestamp plus one generation. Do not add a
second per-candidate timestamp unless you have a separate behavior in mind for
it.

**Your answer: Ah, nope, this was simply confusion on my part. I understand now. No need for two timestamps.**


### 6. Do you agree with this GC artifact representation?

Use a dedicated versioned `GcArtifact` entity family with three layers:

1. Two deterministic A/B control records contain sequence, active manifest
   hash, and checksum. Publishing the inactive control record is the commit.
2. Immutable manifests describe one complete mark generation, its source roots,
   policy, start/completion times, additions, cleared candidates, reclaim set,
   errors, and statistics.
3. Immutable sorted pages hold candidate records. Active state is a compacted
   checkpoint plus bounded delta generations so every scan does not rewrite the
   entire pending set.

Artifact pages and manifests are written, synced, and verified before the
inactive control record publishes their root. An incomplete run may retain an
audit report, but it never advances either active control record.

A candidate binds to the exact current incarnation using at least:

```text
KV key hash
WAL offset
EntryHeader.timestamp
total length
entry type
pending_since_ms
first_unreachable_generation
```

The offset plus entry timestamp prevents stale quarantine from attaching after
void reuse places a replacement at the same logical key or physical offset.
Before reclamation, GC still rechecks the current KV locator, header identity,
reachability, and request/task pins.

**Recommendation:** Adopt this checkpoint-plus-delta representation. A mutable
row per candidate would recreate random-write pressure; a full immutable
snapshot every scan would create excessive write amplification.

`GcArtifact` requires the same minimum-reader capability gate as other new
entity types. It is root-only internal metadata, is never exposed as a user
namespace entry, and is excluded from ordinary content indexing/search.

**Your answer: I do agree. This seems like a good schema... and when I say this, this is me _assuming_ that you are going to have an "entity version", and other common entity fields in the header of the new GcArtifact entity--just like all other entities have.**


### 7. What retention policy should GC audit history use?

I propose two retention tiers:

- **Full detail:** reclaimed identities, paths where known, sizes, offsets,
  reasons, and run manifest for 7 days, with at least the latest 2 completed
  runs retained.
- **Summary:** counts, bytes, timing, roots digest, policy, errors, and artifact
  hashes for 90 days.

Full-detail history also needs a configurable byte cap so repeated enormous
runs cannot fill the disk. My proposed default is 4 GiB. Active quarantine state
is exempt from that audit cap. The latest two completed runs are retained even
if they put audit usage over budget; while over budget, older detail is pruned
and no additional optional history is retained. Preserve every pruned run's
summary and emit a prominent warning/audit event.

**Recommendation:** Make all three controls configurable, with defaults of 7
days full detail, 90 days summaries, and a 4 GiB full-detail cap. Retention time
alone is unsafe for disk usage; a byte cap alone makes audit duration
unpredictable.

Detailed audit access is root-only because records may contain user paths,
system-entry identities, and physical database offsets.

**Your answer: Yes, this all seems reasonable. Don't forget that this requires that we are able to compute _when_ a record was removed, so that we can properly compute _how long_ that GcEntity must be retained. We could go off existing `pending_since_ms` timestamp, and just _assume_ that the entity in-question was reclaimed _when it actually was supposed to be_. This is probably good enough. Any "strange gaps" in time between marking and actual deltion are likely not too important (especially since they would lean in the direction of _retained a little longer than we expected_).**


### 8. Do you agree with this lifecycle for the GC artifacts themselves?

The active control slots, active checkpoint/deltas, and unexpired audit catalog
are explicit GC roots. Nothing should blanket-exempt all `GcArtifact` entries
forever. When an audit generation expires, the catalog stops rooting it; a later
complete mark quarantines those artifact entities, and only a subsequent mark
after grace can reclaim them.

**Recommendation:** Yes. This avoids both recursion problems and immortal GC
metadata while applying the same deletion-safety invariant to the collector's
own history.

**Your answer: This makes sense. Agreed.**


### 9. How should the engine surface corrupt active GC artifacts?

The engine can first fall back to the other valid A/B control generation. If no
complete generation is usable, I propose that it:

1. preserves the corrupt artifacts as evidence;
2. treats every candidate as not eligible;
3. reports `gc_state: "needs_fresh_mark"` prominently in health, metrics,
   diagnostics, and the dashboard;
4. allows ordinary reads and writes because user data is not damaged; and
5. prohibits sweep until a fresh complete mark publishes a valid generation.

**Recommendation:** Degrade GC, not the entire database. Automatically resetting
eligibility is safe, but silently hiding the loss of quarantine history is not.
Corrupt evidence enters a diagnostic hold governed by audit retention unless an
administrator explicitly pins or exports it.

**Your answer: I agree... but I also don't want to hold onto it forever. So having it fall under our grace period should be the plan. We need to ensure that we are properly logging such corruption so that it can be identified, found, and audited for future bug fixes/correction to the DB (if needed).**


### 10. Should a side-by-side database migration reset active quarantine state?

Candidate identities contain source WAL offsets and timestamps, so they cannot
be copied as active state into the rewritten destination. Carrying them forward
by logical hash also risks attaching stale state to a newly written incarnation.

**Recommendation:** Start the destination with an empty quarantine generation.
Previously pending entries receive a fresh complete mark and full grace after
cutover. Preserve the source database unchanged through rollback finalization;
copy only inert GC audit summaries tagged with the source database ID if we want
them visible from the destination. Do not import source candidate records as
operational state.

**Your answer: No. GC operations should be clearly kept in the "nice to have" category. It is by no means a "vital" component of the database from an every-day use perspective, and CERTAINLY not from the perspective of a one-time migration. Retaining GC system correctness (when, i.e., chunk offsets are part of the GC careful-gaurentee) across a migration boundary sounds like a headache, and a ticking time bomb. I'd vote we just strip everything GC related during a migration... including GRACE PERIOD RETENTIN OF GC ENTITIES (which will need to be updated to account for new chunk offsets, etc...). We should just display a warning to the user when a full database clone is initiated that all GC entities will be dropped, and that the existing database should be retained for evidence if there is any concern by the user.

A full database migration will be a large and difficult task, and one that will happen rarely, not regularly.**


## Round 2 Proof Obligations

- Mixed `(EntryType, EntityVersion)` codecs must scan, read, verify, back up,
  restore, repair, and GC correctly in one database.
- Every current entity type must have golden v0/v1-or-v2 fixtures proving exact
  prefix dispatch, header length, payload boundary, CRC, and unknown-version
  failure behavior.
- Header-only upgrade crash injection must prove intent replay before write,
  during torn write, after sync, and during intent retirement.
- Exact KV reads, sequential prefix reads, bounded read-ahead, EOF, corrupt
  lengths, and streaming payload paths must all agree on entity boundaries.
- GC control publication must survive either control slot, manifest, page, or
  sync being torn, missing, duplicated, or reordered.
- Checkpoint/delta compaction must preserve candidate identity, original
  `pending_since_ms`, and first generation without resurrecting cleared state.
- Retention tests must cover duration expiry, byte-cap pressure, minimum-run
  preservation, active-state exemption, and retirement through ordinary GC.
- Migration tests must prove source quarantine cannot attach to destination
  entities and that reset grants at least one complete fresh grace interval.

---

## Round 3: Physical Revision Retirement and Reclamation Receipts

**Status:** Answered by Wyatt; GC category closed by the final decision record below.

This should be the final GC round. Round 2 locked the header sizes, whole-entity
version model, artifact schema, retention defaults, conservative corruption
behavior, and stripped-GC migration policy. Five correctness boundaries remain.

### Decisions Locked From Round 2

1. New entity versions reserve 32 bytes and carry an allocated header length
   plus header CRC32.
2. Reserved bytes are explicitly consumable by later whole-entity versions.
3. The database-level A/B slots grow to 512 bytes, and the Rust type is renamed
   from `FileHeader` to `DatabaseHeader`.
4. Decoding dispatches on `(EntryType, EntityVersion)`. Exact reads are the
   correctness path; bounded read-ahead is only an optimization.
5. Candidate state stores one `pending_since_ms` timestamp and one
   `first_unreachable_generation` counter.
6. `GcArtifact` is a normal versioned entity family with the same common fields
   as every other entity. Its active state uses A/B controls and immutable
   checkpoint-plus-delta generations.
7. Detailed GC history defaults to 7 days and 4 GiB, with two completed runs
   retained; summaries default to 90 days. All detailed access is root-only.
8. Active and retained GC artifacts are roots. Retired GC artifacts become
   ordinary quarantine candidates rather than remaining immortal.
9. Corrupt active state degrades GC only, blocks sweep, preserves evidence, and
   requires a fresh complete mark.
10. Full database migration copies **no** operational or audit GC artifacts.
    The destination starts with no quarantine state and cannot sweep until a
    fresh complete mark. The source database is the evidence/rollback copy.

## Direct Questions

### 1. Do you agree that content addressing is logically unique, not physically unique?

Your relocation concern is valid, but two physical WAL revisions carrying one
logical KV/content key are not inherently corrupt. The WAL is already
multi-version: mutable-key updates leave older physical entries behind while KV
selects the active offset.

When a future entity header no longer fits its allocated reserve, use
copy-on-write relocation:

1. Read and verify the old entity.
2. Append the new entity version under the same logical key.
3. Verify and durably sync the new bytes.
4. Durably switch the KV locator to the new offset.
5. Make readers pin or revalidate the physical incarnation so a reader that
   captured the old offset cannot race its reuse.
6. Record the old offset as a retired physical incarnation.

Only one physical incarnation is active through KV. The older one is fallback
evidence, not a second logical object. Dirty rebuild must choose the newest valid
revision and fall back to the old revision if the new one is incomplete or
corrupt.

**Recommendation:** Formalize this distinction. Do not try to enforce one
physical WAL record per content key; doing so conflicts with append-only crash
safety. Enforce one active KV locator and exact incarnation checks instead.

**Your answer: Logical is what I am going for here. My only concern was that we were properly _considering_ this edge case. We also need to consider crashes and power-off situations: What if we write the new cloned entity to the end of the WAL, and _before we are able to update the old entity to mark it obsolete_, the database powers off? At the very least, the database would need to _know_ when it came back up that the _newer_ version (how do we know which is _newer_?) is the correct entity to select (instead of the old entity).**


### 2. When may the old physical incarnation become Void?

Immediate voiding after the KV switch is too aggressive because an in-flight
reader may already hold the old offset, and latent verification/recovery bugs may
not be visible at the instant of publication.

I propose representing relocation retirement in GC artifacts using the old
logical key, offset, entry timestamp, length, retirement time, replacement
identity, and `reason = entity_relocation`. Reclamation requires:

- the replacement remains the current, fully verified KV incarnation;
- the active locator and retirement record are durable;
- no reader/task pin references the old incarnation;
- a later complete mark confirms the old incarnation is not active; and
- the normal pending-delete grace has elapsed.

**Recommendation:** Put retired physical revisions through the same grace-based
quarantine rather than voiding immediately. The temporary duplicate costs disk
space, but gives us a recoverable fallback and one deletion path. An explicit
root-only emergency reclaim may bypass elapsed grace only under the already
agreed fresh-mark and confirmation rules.

**Your answer: I totally agree with this. The GC reclaing stale entity clones is indeed ideal. Again, as I stated above: I just wanted us to be considering these concerns. We now are, and I like the recommendations you are making.**


### 3. May we record actual reclamation time instead of estimating it?

I need to push back on using `pending_since_ms` as the audit-retention clock.
Consider an entity marked January 1, eligible January 8, but not actually swept
until February 1 because the server was offline or GC was degraded. Seven-day
history measured from the mark would expire before the deletion even occurred.

We can avoid another timestamp on every pending candidate. Each durable sweep
batch writes a versioned receipt containing:

```text
sweep_generation
reclaim_committed_at_ms
reclaimed candidate identities
skipped/recheck-failed identities and reasons
bytes reclaimed
void/hot-tail durability reference
```

Detailed retention begins at the receipt's actual
`reclaim_committed_at_ms`. If a crash occurs after void publication but before
the receipt commits, recovery reconciles the persisted KV/void state and emits a
recovered receipt before another sweep.

**Recommendation:** Record actual reclamation at sweep-batch granularity. It is
more accurate and cheaper than storing another timestamp in every candidate.

**Your answer: Yes, agreed, we should indeed have _two_ timestamps: one for marking, and one for reclaimation.**


### 4. Is this the intended expiry rule for corrupt GC evidence?

"Fall under our grace period" could mean either audit retention or
pending-delete grace, so I want to make the boundary explicit:

1. Corruption detection writes a durable diagnostic record with
   `detected_at_ms`, artifact/control identities, offsets, expected/actual
   checksums, and the error.
2. The corrupt evidence remains rooted for the configured 7-day full-detail
   audit period unless an administrator pins or exports it.
3. After audit expiry, it is unrooted and enters ordinary GC quarantine.
4. Physical reclamation therefore occurs only after both audit retention and
   pending-delete grace. If GC remains degraded, the evidence can remain longer,
   but never because it was silently made immortal.

Health, metrics, diagnostics, the dashboard, and structured logs expose the
failure and retained artifact IDs throughout this period.

**Recommendation:** Use this two-stage lifecycle. The audit window preserves
debug evidence; ordinary grace protects deletion; explicit admin export/pinning
handles investigations that need longer retention.

**Your answer: Agreed.**


### 5. Should online migration suspend mutating GC on the source database?

The destination will intentionally omit all GC artifacts. Allowing source GC to
reclaim physical entries while a multi-terabyte traversal is copying them adds
an avoidable race, even if migration roots and request pins are implemented
correctly.

**Recommendation:** Acquiring the migration lease suspends mutating GC on the
source until cutover, abort, or lease-expiry recovery. GC dry-run/diagnostics may
continue, but no sweep or GC-artifact retention cleanup runs. Normal database
reads and writes continue. Startup detects and safely resumes or releases a
stale migration lease before allowing GC again.

**Your answer: Agreed. Keeping the GC running during a migration could be dangerous (it could mark or reclaim entities that have already been moved over to the new database). It also is pointless, since we are deliberately deciding to drop all GC-related data.**


## Round 3 Proof Obligations

- Relocation crash injection must cover append, data sync, KV switch, locator
  durability, retirement publication, pin drain, and old-incarnation reclaim.
- Dirty full rebuild must select the newest valid relocated incarnation, reject
  a torn replacement, and retain the old valid fallback.
- A reader that captures an old KV snapshot before relocation must either finish
  against pinned old bytes or revalidate and retry against the new incarnation;
  it must never read a reused Void range.
- Sweep receipts must distinguish proposed, skipped, durably reclaimed, and
  crash-recovered outcomes without double-counting bytes or shortening audit
  retention.
- Corrupt evidence retention must cover A/B fallback, both-control failure,
  page-level corruption, audit expiry, admin pin/export, and degraded-GC delay.
- Migration lease tests must cover normal completion, abort, process crash,
  stale lease recovery, dry-run GC, and rejection of every mutating GC entry
  point while migration is active.

---

## GC Category: Final Decision Record

**Status:** Closed. No further policy questions remain in this category.

### 1. Whole-Entity Versioning and Framing

- Every persisted entity is decoded by `(EntryType, EntityVersion)`.
- The entity version governs the complete version-specific header and payload.
  There is no separate payload-version field.
- A small canonical prefix contains enough information to identify the entity
  type/version and locate its version-specific framing.
- New entity formats carry an allocated header length and header CRC32.
- New entity formats reserve 32 bytes that later versions may explicitly
  consume without relocating the payload.
- Consuming reserve requires an entity-version bump and a crash-safe header
  update intent/verify/retire protocol. Naked in-place header overwrites are
  forbidden.
- If a later version exceeds the allocation, the engine performs copy-on-write
  relocation and retires the old physical incarnation through GC.
- Exact length-bounded reads are the correctness path. Bounded read-ahead and
  coalescing may optimize I/O, but codecs never parse bytes outside an entity's
  declared boundary. Large values may be streamed.

The earlier 101-byte Blake3 estimate becomes **109 bytes** after the recovery
ordering field below is made explicit: 63 current bytes + 2 allocated-length +
4 CRC32 + 8 write-sequence + 32 reserve. This is 46 additional bytes per
entity, approximately 359 MB at 7.8 million entities or 4.6 GB at 100 million
entities. The reserve itself remains the agreed 32 bytes.

### 2. Physical-Incarnation Ordering and Crash Recovery

Logical content/KV identity is unique; physical WAL representation is
multi-version. A content key may temporarily have multiple valid physical
incarnations while exactly one KV locator is active.

New entity headers carry a monotonic `write_sequence: u64`. Wall-clock time is
not authoritative for revision ordering. The expanded `DatabaseHeader` stores a
durable sequence high-water mark; the writer reserves sequence ranges ahead of
use through the A/B database-header protocol so a crash may skip numbers but can
never reuse one.

Legacy entities retain their current timestamp/offset ordering. During a mixed
format transition, a valid sequenced entity written after capability activation
orders after a legacy incarnation of the same key. Once migration completes,
all live destination entities use sequence ordering.

Copy-on-write relocation follows this order:

1. Reserve a new write sequence durably.
2. Make encryption keys, codec/config versions, and every other dependency of
   the new entity version durable and readable.
3. Read and verify the active old incarnation.
4. Append the new version with the same logical key and a higher sequence.
5. Decode/verify the new representation, confirm the same canonical logical
   identity, and durably sync its bytes.
6. Durably publish the KV locator and retirement metadata.
7. Drain or revalidate readers pinned to the old incarnation.
8. Quarantine the old incarnation for later GC reclamation.

The old entity is never required to be mutated to establish which copy wins.
After power loss:

- a torn or unverifiable new incarnation is rejected and the old valid one
  remains active;
- a complete valid new incarnation with the higher sequence is selected;
- a durable KV publication selects the new locator directly; and
- if retirement publication was interrupted, recovery synthesizes a retired-
  incarnation quarantine record for every valid lower-sequence duplicate.

Full dirty rebuild groups duplicate logical keys, chooses the highest valid
write sequence, and retains lower valid revisions as recoverable retired
incarnations. Offset is never chronology after Void reuse.

### 3. Database Header

- Rename the Rust `FileHeader` type and related APIs to `DatabaseHeader` to
  distinguish the `.aeordb` container header from user `FileRecord` entities.
- Grow both A/B database-header slots from 256 to 512 bytes during side-by-side
  migration, for 512 additional bytes per database.
- Add a stable database identity, minimum-reader capability fields, and durable
  write-sequence reservation high-water state.
- Preserve explicit versioned readers/writers for old and new database headers;
  an unknown version is rejected rather than reinterpreted.

### 4. GC Quarantine Artifacts

- Quarantine is canonical in a dedicated versioned `GcArtifact` entity family,
  not mutable entity headers or KV flag bits.
- `GcArtifact` uses the same common whole-entity version framing as every other
  entity and requires the minimum-reader capability gate before first append.
- Two deterministic A/B control records publish immutable manifests.
- Candidate state is stored as immutable sorted pages represented by compacted
  checkpoints plus bounded deltas.
- Pages/manifests are written, synced, and verified before the inactive control
  slot publishes their root.
- An incomplete mark may publish an audit failure record but cannot advance
  active quarantine state or reclaim anything.
- Candidate identity binds at least KV key, WAL offset, entry write identity,
  total length, and entry type so state cannot attach to a replacement
  incarnation.

Each pending candidate stores:

```text
pending_since_ms: u64
first_unreachable_generation: u64
```

The generation is not a second timestamp. Reclamation requires a later complete
generation, elapsed grace, exact current-incarnation/reachability rechecks, and
no reader/task pin. A reachable-again entity clears pending state only through a
successfully published complete mark.

### 5. Relocated and Retired Incarnations

- Copy-on-write replacement never immediately converts the old bytes to Void.
- Retirement state records old and replacement identities, old offset/write
  identity/length, retirement time, and reason `entity_relocation`.
- The old revision remains a recovery fallback until the replacement is the
  durable verified active locator and all old-incarnation pins drain.
- The normal two-complete-mark and pending-delete grace rules apply before old
  physical bytes are reusable.
- The root-only emergency reclaim policy remains available only with a fresh
  complete mark, exact confirmation, dry-run report, and explicit `--yes`.

### 6. Actual Reclamation Receipts

Mark time and reclamation time are distinct:

- `pending_since_ms` records first completed unreachable marking.
- `reclaim_committed_at_ms` records when a sweep batch became durably reclaimed.

Each sweep batch publishes a versioned receipt containing proposed, reclaimed,
skipped, and failed identities; reasons; reclaimed bytes; generation; and the
durable Void/hot-tail reference. Full-detail audit retention begins at actual
`reclaim_committed_at_ms`, never at the earlier pending timestamp.

If a crash occurs after durable Void/KV publication but before receipt commit,
startup reconciles the persisted locator/Void state and writes a recovered
receipt before allowing another sweep. Receipts are idempotent and cannot
double-count bytes.

### 7. GC Artifact and Audit Lifecycle

- Active controls, active checkpoint/deltas, incomplete-run diagnostics, and
  unexpired audit catalogs are explicit GC roots.
- Detailed history defaults to 7 days with a 4 GiB soft budget and a minimum of
  the latest two completed runs.
- Summary history defaults to 90 days.
- Active state is exempt from the audit budget. Minimum retained runs may put
  history temporarily over budget; older optional detail is pruned first with a
  prominent audit/health warning.
- Detailed artifacts and APIs are root-only and never enter user namespace
  listing, content indexing, or search.
- Expired history is unrooted, then enters ordinary quarantine; it is never
  blanket-exempt or immediately destroyed.

Corrupt GC evidence receives its own durable `detected_at_ms` diagnostic with
artifact/control identities, offsets, checksums, and errors. It remains rooted
for full-detail audit retention, then enters ordinary pending-delete grace.
Admin pin/export can retain it longer. Health, metrics, diagnostics, dashboard,
and structured logs expose the degraded state and evidence identifiers.

### 8. Corruption and Failure Policy

- Try the other valid A/B control generation first.
- If no complete generation is usable, preserve evidence, treat every candidate
  as non-eligible, report `gc_state: "needs_fresh_mark"`, and prohibit sweep.
- Ordinary reads and writes continue because GC is derived/nonvital and user
  data has not been shown corrupt.
- A fresh complete mark must publish valid active state before sweep resumes.
- Missing, corrupt, partial, or unknown quarantine state is never inferred from
  entity timestamps, database timestamps, or filesystem metadata.

### 9. Full Database Migration

- Migration is an online side-by-side clone with normal source reads/writes,
  durable bounded mutation capture, and a short exclusive final delta/cutover
  window.
- Acquiring the migration lease suspends every mutating GC and GC-retention
  cleanup entry point on the source. Dry-run diagnostics may continue.
- Startup safely resumes or releases a stale migration lease before permitting
  mutating GC again.
- The destination copies no GC controls, candidates, receipts, audit summaries,
  corrupt evidence, or grace state. It starts `gc_state = never_marked` and
  cannot sweep until a fresh complete mark.
- The migration command warns explicitly that all GC state/history is omitted
  and the source database must be retained if the operator needs that evidence.
- The source remains untouched through rollback finalization.

### 10. Required Verification Matrix

Implementation is not complete until automated tests cover:

- golden bytes and mixed reads for every `(EntryType, EntityVersion)` plus old
  and new `DatabaseHeader` versions;
- unknown versions, corrupt allocated lengths/CRCs, truncation, EOF, bounded
  read-ahead, and streaming boundaries;
- sequence-range reservation, clock rollback, crash/restart, skipped ranges,
  exhaustion, and mixed legacy/sequenced rebuild ordering;
- relocation crash points before/after append, sync, KV publication, retirement
  publication, reader-pin drain, and old-incarnation reclamation;
- dirty rebuild choosing the newest valid incarnation, falling back from a torn
  replacement, and reconstructing missing retirement state;
- incomplete marks, grace zero, reachable-again clearing, stale incarnation
  rejection, pin races, and final sweep rechecks;
- torn/missing/reordered A/B controls, manifests, pages, compaction deltas,
  sweep receipts, and sync barriers;
- actual reclaim-time retention, skipped candidates, crash-recovered receipts,
  byte caps, minimum-run preservation, and no double accounting;
- corrupt-evidence fallback, logging, health/dashboard visibility, audit expiry,
  admin pin/export, and delayed cleanup while GC is degraded; and
- migration completion, abort, crash, stale lease, GC suspension, no copied GC
  artifacts, source rollback, and first safe destination mark/sweep.

---

# Category 2: Bounded Mark Implementation - Round 1

**Status:** Answered. Unresolved explanations and narrower decisions continue
in Round 2 below.

This round resolves the shape of the replacement for the current monolithic GC
mark. I traced the existing implementation before drafting these questions. The
important findings are:

- `gc_mark` uses one `HashSet<Vec<u8>>` as three different things: the live
  entity set, the traversal visited set, and the sweep membership oracle;
- each namespace BFS level is materialized as a `Vec`, sorted, and replaced by
  another unbounded `Vec` for the next level;
- `collect_btree_children` recursively materializes every child of a directory
  B-tree into one `Vec<ChildEntry>`;
- `iter_kv_entries` materializes a second complete `Vec<KVEntry>` during both
  mark and sweep;
- the concurrent-write `gc_recheck` queue is another unbounded
  `HashSet<Vec<u8>>`;
- queued GC tasks are nominally cancellable, but `execute_gc` does not pass the
  task cancellation state or shutdown token into `run_gc`;
- the direct HTTP route is one synchronous `spawn_blocking` call with no useful
  cancellation or durable progress contract; and
- reachable B-tree corruption aborts at the first problem. This is fail-closed,
  which is correct for deletion safety, but it neither bounds pathological
  traversal nor gathers a useful multi-error diagnostic report.

I do **not** recommend replacing the hash set with a generic disk-backed hash
table. That would preserve the conflation above and write roughly one full hash
per live entity. At 100 million Blake3 identities, hashes alone are about 3.2
GB before table, allocator, checksum, or filesystem overhead.

The better primitive is available in the KV layout we already have. A KV page
has a fixed maximum of 32 slots. While a KV layout generation remains stable,
`(bucket_index, slot_index)` is a dense exact ordinal for the logical KV key.
One bit per slot therefore represents the complete live membership oracle:

```text
4 GiB Blake3 KV block  -> approximately 94.8 million slots -> 11.3 MiB
8 GiB Blake3 KV block  -> approximately 189.6 million slots -> 22.6 MiB
100 million used slots -> 12.5 MiB of actual mark bits
```

The proposal below uses that dense bitmap for entity liveness, then uses
separate bounded structures for traversal work and path-sensitive visits. This
keeps each concern explicit and prevents another multi-gigabyte GC transient.

## Proposed Mark Pipeline

1. Flush the pre-existing KV write buffer, capture the KV layout generation,
   capture the authoritative root set, and start a bounded mutation journal.
2. Walk captured roots through a bounded external work queue. Resolve each hash
   to `(layout_generation, bucket, slot, KVEntry)` and set its exact mark bit.
3. Stream chunk leaves without reading payloads. Stream directory B-tree leaves
   through a visitor rather than collecting a whole directory in memory.
4. Keep logical-path traversal distinct from physical entity liveness. A
   partitioned exact set of `(directory identity, logical path identity)` avoids
   repeating the same snapshot/path projection while still processing identical
   directory content mounted at different paths.
5. Spill frontier runs, path-visit runs, diagnostic records, and mutation
   records to a per-run scratch workspace. Buffers reserve memory before use and
   externally sort/compact runs when thresholds are reached.
6. Drain the mutation journal in bounded passes. Each post-boundary entry is
   conservatively protected for this generation and its references are walked,
   so a new FileRecord can rescue old chunks and a new directory node can rescue
   old children.
7. Enter a short exclusive namespace barrier, drain the final mutation tail,
   verify that the KV layout generation is unchanged, and stream the KV by page
   against the mark bitmap and previous quarantine generation.
8. Write, sync, verify, and publish the new immutable `GcArtifact` generation.
   No mark scratch data is authoritative and no incomplete run can publish.
9. Reclamation runs in bounded batches. Before every batch it drains later
   mutations, performs exact identity/pin/reachability checks, and holds the
   exclusive publication barrier only for that batch.

## 1. May the mark bitmap use KV layout ordinals?

The choices are:

1. Capture one KV layout generation and use `(bucket, slot)` bits. A KV
   expansion/rebuild aborts the current mark as incomplete; normal inserts and
   updates remain online and are caught by the mutation journal.
2. Add `write_sequence` to every KV locator/page and key the bitmap by sequence.
   This survives KV expansion, but changes the KV on-disk format and adds eight
   bytes to every 45-byte Blake3 KV row, approximately 17.8% raw row overhead.
3. Store exact hashes in a partitioned disk set, paying gigabytes of scratch I/O
   at large scale.

**Recommendation:** Use option 1. It gives exact membership with tiny bounded
state and does not disturb the KV format. Add an internal
`KvMarkLocator { layout_generation, bucket, slot, entry }` API and a strict
streaming page scanner. Force-flush the old buffer before capture. If expansion
becomes necessary during the run, signal cancellation to the mark, wait for it
to release its layout lease, expand, and leave the previous quarantine
generation unchanged. GC may occasionally restart at a KV growth boundary;
that is much better than pinning writers or silently using shifted ordinals.

**Your answer: This appears to be a good idea to me. Where is this bitmap stored? Is it just stored in-memory during the GC operation?**

## 2. Where should non-authoritative mark scratch data live?

I propose an adjacent, explicitly ephemeral workspace:

```text
<database-parent>/.<database-name>.work/gc/<database-id>/<run-id>/
```

It would contain a run manifest, segmented bitmap files when the bitmap exceeds
the in-memory reservation, externally sorted frontier runs, path-visit runs,
the concurrent mutation journal, and bounded corruption diagnostics. Records
would be versioned, length-bounded, and checksummed so damaged scratch causes a
safe abort rather than bad mark state.

This scratch must **not** be stored as canonical `GcArtifact` data in the WAL.
Doing that would expand the database merely to decide what can be reclaimed and
would make incomplete work part of its own reachability problem. Only the final
quarantine checkpoint/deltas and audit result become `GcArtifact` entities.

On clean completion or explicit cancellation, the workspace is deleted. On
startup, stale workspaces are reported and removed after validating that their
database ID is local and that they are not a published artifact root. Failure
to remove one is a health warning and disk leak, never permission to resume or
sweep from it.

**Recommendation:** Use the adjacent workspace by default, with an optional
admin-configured scratch root for deployments that have a dedicated fast work
volume. Do not silently fall back to `/tmp`; a different filesystem can have
surprising capacity and reboot semantics. If the configured/default workspace
cannot be created, the mark fails before doing work.

**Your answer: So, in answer you my question above, we are storing these bitmaps as files in the DB itself?**

## 3. Should marking remain online while normal writes continue?

Holding the namespace write lock for a multi-hour mark is not acceptable. The
current code effectively does this for mutating GC despite also maintaining a
recheck set, which gives us both write starvation and unbounded memory. Quite an
achievement, just not the sort we want to preserve.

I propose replacing `gc_recheck: HashSet<Vec<u8>>` with one bounded
`GcMutationJournal` shared by every GC entry point:

- successful writes append compact identity records to bounded memory buffers;
- full buffers spill to the mark workspace;
- journal I/O failure marks the GC run incomplete but does **not** fail the
  user's otherwise durable write;
- finalization drains and verifies all records under a short exclusive barrier;
- post-boundary entities are protected for the current generation, and their
  old referenced dependencies are traversed; and
- the journal remains active through sweep batches so writes between batches
  cannot resurrect a candidate unnoticed.

**Recommendation:** Keep marking online. Use short exclusive barriers only for
final publication and each physical reclaim batch. If mutation volume prevents
the journal tail from converging beneath the configured finalization bound, the
run should report `catch_up_starved`, publish nothing, and retry later rather
than eventually freezing writers for an unbounded interval.

**Your answer: Agreed. I am kinda shocked to see that implementation once-again deviated so far from design. Thank you for catching these mistakes.**

## 4. May we require a shared reference-publication guard on every writer?

The mutation journal closes the broad race, but one narrow race still matters:
a writer can resolve an old chunk or B-tree child, then GC can reclaim it, then
the writer can publish a new FileRecord/root that references reclaimed bytes.

The proper invariant is:

- every operation that publishes references acquires a shared namespace/
  generation guard **before** resolving dependencies and holds it through the
  durable publication of the new root/path key;
- a reclaim batch acquires the corresponding exclusive guard, drains the
  mutation tail, revalidates candidate identities and pins, commits its bounded
  batch, and releases the guard; and
- read-only requests use the request-generation pins already decided in the GC
  category rather than taking this writer guard.

This is not intended to serialize writers with one another. Shared writer
guards coexist; only the short reclaim batch excludes them.

**Recommendation:** Make this a required engine invariant and audit every raw,
batch, blob-commit, merge, move/copy, restore, embedded SDK, plugin, and repair
write path against it. A GC design cannot be correct if reference resolution
and reference publication are allowed to straddle physical reclamation.

**Your answer: Agreed.**

## 5. What should explicit cancellation and process shutdown mean?

There are two different operator intents:

- **Explicit task cancellation:** stop because the operator no longer wants
  this run.
- **Graceful server shutdown:** stop because the process is leaving, not
  necessarily because the work is unwanted.

Safe durable resume is harder than retaining scratch files. Writes that happen
after restart were not observed by the old in-process mutation journal, so a
resumed run needs a durable mutation boundary and a proven catch-up source. We
will have the new entity `write_sequence`, but Void reuse means WAL offsets are
not chronological and sequence reservations can contain gaps. Pretending an
old scratch queue is resumable without solving that would be a particularly
polished data-loss bug.

**Recommendation for the first implementation:** Explicit cancel and graceful
shutdown both terminate the run without publishing, preserve the previous
quarantine generation, and clean the scratch workspace. A later checkpoint/
resume feature should require a durable write-sequence-to-incarnation change
journal and its own crash tests. Cancellation is polled before/after every I/O
batch and at a work quantum targeting no more than 100 ms response latency.

Do you agree with restart-from-scratch initially, or do you consider durable
resume a requirement for the first bounded implementation?

**Your answer: I am leaning towards a restart-from-scratch pattern here... but am curious what a "durable resume" would look like. Can you give me an example of what it would entail to make a resumable GC operation?**

## 6. What default memory and scratch-space policy should we freeze?

The budget must include all GC-owned memory: resident bitmap segments,
frontier/sort buffers, path-visit compaction buffers, B-tree stacks, mutation
journal buffers, candidate merge buffers, diagnostics, and allocator overhead.
It cannot honestly include the engine's existing KV pages and caches, so metrics
must report both GC-owned bytes and process RSS separately.

**Recommendation:**

```text
gc.mark_memory_budget_bytes = 256 MiB
gc.mark_memory_minimum_bytes = 64 MiB
gc.mark_scratch_free_reserve = max(8 GiB, min(64 GiB, 2% of filesystem capacity))
gc.mark_scratch_max_bytes = null  # reserve policy is authoritative by default
```

- Reserve the dense bitmap first.
- Divide the remainder among explicit sub-budgets; no component borrows past
  the hard total without first releasing another reservation.
- Preflight estimated scratch, then continuously stop before crossing the free
  reserve.
- An admin may set a hard scratch maximum in addition to the reserve.
- Allocation failure, reserve breach, ENOSPC, short write, or compaction failure
  makes the mark incomplete and publishes nothing.
- Expose current/peak/reserved/spilled/compacted bytes per component plus
  filesystem reserve pressure through metrics, task progress, logs, CLI, and
  the dashboard.

Do these defaults and this policy fit what you want, or should the default
memory budget be lower/higher?

**Your answer: Are we talking about RAM, or disk space here? My question will be vastly different depending on if this is RAM or disk space you are asking about.**

## 7. How should reachable B-tree corruption affect the mark and database?

For GC, `BestEffort` traversal is never sufficient evidence of unreachability.
Skipping one damaged branch could classify every entity below it as garbage.
At the same time, aborting on the first malformed child gives the operator poor
evidence and repeatedly rediscovers one problem per run.

**Recommendation:** Use a strict-but-diagnostic GC walker:

1. Mark the run incomplete on the first reachable missing, malformed,
   wrong-type, hash-mismatched, cyclic, over-depth, unsorted, duplicate-child,
   or separator-range-invalid B-tree node.
2. Continue other independent roots and branches only where traversal remains
   structurally safe, gathering diagnostics; never infer liveness beneath the
   damaged edge.
3. Publish no quarantine generation and perform no sweep from the incomplete
   result. Preserve the previous valid generation but disallow sweep until a
   later fresh complete mark.
4. Keep normal reads/writes available, report health as degraded, and point at
   `verify`/`repair`. Do not auto-repair during GC.
5. Bound the in-memory diagnostic sample (I propose 1,000 issues), spill full
   details to the scratch/audit report, and keep aggregate counts after the
   detail cap so corruption cannot become another memory attack.

I do not recommend putting the whole database into read-only mode merely
because GC found a damaged namespace branch. That policy belongs to verify/
repair and the durability latch, and automatically changing write policy from a
maintenance scan could make recovery harder. Do you agree?

**Your answer: I do agree. I would very much like to skip damaged branches/leafs, while continuing normal operations with healthy ones. The "mark for repair" is something I also like. We already have built-in CRON tasks in the database, and I'd certainly like to move to a place where one of the tasks is deep surgical repair. The ability to mark a single branch/leaf as "read only" would be a great feature too.**

## 8. Should derived-index tree corruption be isolated from authoritative tree corruption?

The refactor introduces multiple root families. A broken HEAD/snapshot/fork/
system B-tree means user-data reachability is unknown and must invalidate the
whole mark. A broken derived-index artifact is different: the index is
rebuildable, but its artifact closure may be unknown.

There are two safe initial policies:

1. Any rooted tree corruption invalidates the whole mark. Simple and safest,
   but one disposable index blocks all garbage collection.
2. Authoritative corruption invalidates the whole mark. Derived corruption
   degrades that index generation, conservatively protects the entire tagged
   artifact family, and permits the authoritative-content mark to complete.

**Recommendation:** Implement option 1 until every derived artifact carries an
exact, cheaply enumerable `(index_id, generation)` family identity. Then enable
option 2 as an explicit capability. Without family enumeration, saying
"protect the damaged index" is hand-waving because we cannot know which
unreachable pages belonged to it.

**Your answer: True, but I'd prefer we go with #2 here, and since we have already decided that we will copy the entire database for the side-by-side migration operation, then we will guarentee needed support for #2 after migration, correct?**

## 9. Should every GC invocation use the task system?

Cancellation and progress will remain inconsistent if the HTTP route, CLI, cron
runner, embedded API, and task worker each call a different wrapper.

**Recommendation:** Build one `GcRunContext` carrying run ID, cancellation
token, budgets, progress sink, mutation journal, root/layout boundaries, and
mode. All entry points call the same engine implementation. HTTP starts a task
and returns a task ID; CLI may enqueue and wait while rendering progress; cron
uses the same task; embedded trusted callers may either await the same run or
provide their own context. Keep an explicit compatibility wait mode if an
existing HTTP caller needs the final `GcResult` in one response, but do not keep
a second synchronous GC implementation.

**Your answer: Yes, absolutely. I thought this is what was already happening. Sounds like more implementation drift...**

## AGIS Test-Protocol Findings for This Category

No answer is required here; this records the proof obligations that informed
the questions above.

### Existing coverage

The current suite verifies basic HEAD/snapshot/fork marking, structural-sharing
deduplication, stale path-key FileRecords, one missing B-tree child failure,
ordinary sweep/readability, dry-run behavior, HTTP authorization, and a handful
of restore/concurrent-write regressions. It does **not** prove a memory ceiling,
bounded frontiers, cancellation, scratch failures, cyclic/deep B-trees,
multi-error diagnostics, KV layout invalidation, mutation-journal overflow, or
candidate publication crash ordering.

### Core hypotheses

1. Peak GC-owned memory remains below the configured hard budget independent of
   live entity count, directory width, snapshot count, and mutation volume.
2. No reachable physical incarnation becomes pending or reclaimed under
   concurrent writes, relocation, restore, cancellation, corruption, layout
   expansion, I/O failure, or process death.
3. Only a complete, verified mark can atomically replace active quarantine
   state; incomplete runs are observationally incapable of advancing deletion.

### Tests to write before implementation

- Unit/property tests for bitmap location, slot boundaries, segment spill/LRU,
  exact path-visit sets, run merge/compaction, duplicate work, checksums,
  truncation, malformed lengths, and deterministic cancellation at every work
  quantum.
- B-tree generator tests for valid trees plus missing nodes, wrong types, hash
  mismatches, cycles, excessive depth/fanout, duplicate children, unsorted keys,
  separator violations, corrupt leaves, and multiple independent faults.
- Integration tests that force tiny memory/scratch budgets so spill, compaction,
  reserve breach, ENOSPC/short-write injection, and cleanup paths execute on
  small databases rather than relying on impractically huge fixtures.
- Concurrency model tests that pause writers between dependency lookup and root
  publication, race KV expansion/rebuild against mark, continuously mutate
  files/chunks/B-trees during catch-up, and prove the exclusive reclaim guard
  closes every race.
- Publication fault-injection tests at every page/manifest/control/receipt sync
  boundary and restart tests proving the old valid generation always wins after
  incomplete work.
- Task/API/CLI tests for cancel-before-start, cancellation during every phase,
  double cancellation, shutdown, panic containment, progress monotonicity, and
  bounded cancellation latency.
- A synthetic scale harness that represents at least 100 million KV slots
  without writing 100 million payloads, proving bitmap size and process-owned
  budget mathematically; plus a real `/tmp/codex` live-server test with broad
  directories, concurrent blob commits, cancellation, restart, verify, and two
  quarantine generations before any physical reclaim.

Tests must use deterministic failpoints rather than timing luck. Unit and normal
integration cases retain the standard 10-second timeout. Explicit scale/stress
targets receive separately declared bounded timeouts and run outside the normal
fast suite so a deadlocked GC cannot quietly cook the machine overnight.

---

# Category 2: Bounded Mark Implementation - Round 2

**Status:** Answered. One corrected durable-workspace decision continues in
Round 3 below.

This round is deliberately more explanatory. Round 1 asked you to make several
choices before I had made the physical boundaries concrete enough. You should
be deciding product and failure policy; you should not have to reverse-engineer
where I imagined a bitmap living.

The following decisions are already locked from Round 1 and are not being asked
again:

- normal writes remain online during mark;
- the unbounded in-memory `gc_recheck` set becomes a bounded spillable mutation
  journal;
- every reference-producing writer holds a shared publication guard from
  dependency resolution through durable root publication;
- reclaim batches take the corresponding exclusive guard for a bounded time;
- all HTTP, CLI, cron, embedded, and task-worker entry points use one
  `GcRunContext` and one implementation; and
- GC B-tree walking is diagnostic and fail-closed for deletion authorization,
  while ordinary service should remain available wherever safely possible.

## 1. Where the bitmap and scratch files actually live

The bitmap is **not an entity inside the `.aeordb` file**. There are three
different storage lifetimes:

```text
RAM
  Hot bitmap/frontier/journal segments needed right now.

Temporary sibling workspace (outside the database file)
  /data/.taraani.aeordb.work/gc/<database-id>/<run-id>/...
  Overflow bitmap segments, frontier runs, path-visit runs, mutation-journal
  runs, and diagnostics. Everything here is disposable.

The .aeordb database itself
  Only the final, complete, verified quarantine generation and retained audit
  artifacts. These are canonical GcArtifact entities.
```

For the current 4 GiB Blake3 KV layout, the complete bitmap is only about 11.3
MiB. Under normal conditions I would keep that bitmap in RAM because doing so is
cheap and avoids random scratch I/O. The run still creates a sibling workspace
for larger frontier/path/journal data. If a future bitmap no longer fits its RAM
reservation, fixed-size cold segments spill into that workspace and are loaded
through an explicit LRU segment cache.

I do **not** recommend memory-mapping an arbitrarily large bitmap and calling it
"disk backed." An mmap can still become resident through the OS page cache and
makes our process-memory promise difficult to reason about. Explicit bounded
`pread`/`pwrite`-style segment I/O gives us a real userspace ceiling. Scratch
writes are sequential where possible, and consumed runs are advised/dropped
from cache where the platform supports it.

If the process crashes, the temporary files authorize nothing. Startup validates
their database/run IDs and removes them. The previously published quarantine
generation remains authoritative.

**Recommendation:** Keep the approximately 11.3 MiB current bitmap in RAM,
spill only when the bitmap reservation or other bounded work buffers require
it, and place all spills in the adjacent sibling workspace by default. Permit an
administrator to configure a different scratch volume, but never silently use
`/tmp`.

Does this resolve the RAM-versus-database-file confusion, and do you approve
this placement policy?

**Your answer: I am not a big fan of `/data/.taraani.aeordb.work/gc/<database-id>/<run-id>/`... I do approve of it being in RAM. Even if the bitmap is huge, we are talking <100mb... this is not a concern for me. Besides, if we "spill over" in RAM, the kernel will just dump us to swap space anyhow. If we must end up writing cache files, then I would prefer they be at /tmp/ (unless the risk of reboots and wiping these files is a concern... in which case you should give me your thoughts, and we should revisit this discussion)**

## 2. What durable resume would actually require

Suppose a mark has run for seven hours and the server shuts down. A genuine
resumable implementation cannot merely leave `bitmap.bin` behind. It needs a
coherent checkpoint such as:

```text
GcMarkCheckpoint
  database_id
  run_id / format_version
  captured KV layout generation
  captured HEAD, snapshots, forks, system roots, and derived roots
  root/config/capability fingerprints
  durable mark-bitmap segment generation
  durable frontier-run catalog and read positions
  durable path-visit-set generation
  durable mutation-journal watermark
  last completely reconciled write_sequence
  counters/progress/diagnostic state
  checksums for every referenced scratch object
```

Checkpoint publication itself needs an A/B protocol:

1. Stop advancing the logical checkpoint.
2. Flush and sync dirty bitmap segments, work runs, path sets, and mutation
   records.
3. Write and sync a new immutable checkpoint manifest.
4. Verify every referenced object.
5. Publish the inactive A/B pointer and sync it.
6. Only then discard scratch objects older than the previous valid checkpoint.

On restart, before accepting writes, AeorDB would:

1. validate database identity, capability/config fingerprints, KV layout
   generation, root availability, every scratch checksum, and the A/B pointer;
2. reject resume if KV rebuild/expansion changed slot ordinals;
3. restore the bitmap, work queues, and path-visit set from the last valid
   checkpoint;
4. reconcile committed entities newer than the journal watermark using the new
   `write_sequence` metadata, because a crash may have committed a write before
   its non-authoritative GC journal record reached disk;
5. traverse those newer entities as protective roots so their old chunk/tree
   dependencies are rescued; and
6. install live mutation capture before opening normal writer admission.

A graceful shutdown can make this cheaper by freezing writer admission before
the final checkpoint. Crash resume still needs the sequence reconciliation
path. Torn scratch, changed layout, missing roots, unknown versions, insufficient
disk, or an unprovable mutation gap must discard the checkpoint and restart the
mark.

The benefit is obvious: a seven-hour mark might resume near hour seven. The
cost is also real: extra scratch fsyncs, checkpoint compaction, startup logic,
write-sequence reconciliation, a longer-lived scratch format, and a large crash
test matrix. It is nearly a second maintenance engine layered on the first.

**Recommendation:** The first bounded implementation should restart marks from
scratch after explicit cancellation, graceful shutdown, or crash. We should
make its scratch records versioned/checksummed and its stages restartable in
code structure, but should not claim durable resume until the sequence journal
and crash protocol above exist. A fast, bounded restart is safer than a clever
resume protocol we only half built.

With that concrete example, do you confirm restart-from-scratch for the first
implementation?

**Your answer: You appear to be telling me two different things: 1) We need durable scratch files, 2) We should just restart-from-scratch for the first implementation... WHAT is your actual position? I originally told you that we would "restart-from-scratch" ONLY as a _phased implementation approach_... at the end of the day, I want our database to be a first-class enterprise-level database, so we should do the _correct_ thing, not the _easiest_ thing.**

## 3. RAM limits and disk limits are separate controls

Round 1 mixed two resources in one question. Here is the separation:

| Control | Resource | Meaning |
|---|---|---|
| `memory.soft_limit_bytes` | RAM/RSS | Begin aggressive eviction, pause maintenance admission, and shrink spillable buffers. |
| `memory.hard_limit_bytes` | RAM/RSS | Admit no new memory-amplifying work; cancel/pause maintenance and preserve headroom for streaming reads and durable small writes. |
| `gc.mark_memory_budget_bytes` | RAM within the process limit | Maximum GC-owned resident buffers, not extra memory above the process ceiling. |
| `gc.mark_memory_minimum_bytes` | RAM within the process limit | Smallest useful GC reservation; below it, delay rather than thrash indefinitely. |
| `gc.mark_scratch_free_reserve` | Filesystem space | Free space GC is forbidden to consume. |
| `gc.mark_scratch_max_bytes` | Filesystem space | Optional administrator cap on one run's disposable scratch. |

The 16 GiB server changes my recommendation. AeorDB shares that host, so merely
giving GC 256 MiB while the rest of the engine grows to 13 GiB does not solve
the problem.

### Recommended 16 GiB production envelope

```text
AeorDB process soft limit:       6 GiB
AeorDB process hard limit:       8 GiB
Minimum host MemAvailable to
start/continue maintenance:      2 GiB

GC preferred RAM reservation: 128 MiB
GC minimum RAM reservation:     64 MiB

GC disk reserve:
  max(8 GiB, min(64 GiB, 2% of filesystem capacity))
GC scratch hard maximum:       unset by default
```

The GC reservation is carved out of the 6/8 GiB process envelope. If AeorDB is
already near the soft limit, GC first evicts clean caches and waits. It does not
blindly allocate another 128 MiB. Under pressure it may operate with 64 MiB and
spill more work to disk. If that reservation cannot be made, the task remains
queued/paused rather than risking the server.

At the soft limit the global memory coordinator should:

- evict clean derived-index caches and bound their current 2 GiB default;
- evict/bound the currently unbounded directory-content cache;
- flush bounded dirty index/mutation buffers safely;
- reduce resident GC/reindex/repair work segments;
- refuse new parser/plugin jobs whose declared memory cannot fit; and
- pause lower-priority maintenance until RSS and host available memory recover.

At the hard limit it should cancel or pause spillable maintenance, reject new
memory-amplifying requests with a retryable response, and preserve a reserved
emergency margin for durability flushes, health/status, streaming reads, and
orderly shutdown. RSS is a backstop; every known large allocator must reserve
through the coordinator before allocating so this is proactive rather than an
OOM autopsy.

### An existing non-GC blocker

The current `ReadSnapshot` holds every fixed KV bucket page as in-process
`Arc<[u8]>` data. Therefore a 4 GiB KV block consumes roughly 4 GiB of process
memory before indexes, directory caches, request buffers, old page generations,
allocator overhead, and GC. The current `estimated_engine_owned_bytes` metric
counts only index and directory caches, so it substantially understates engine
ownership.

Worse, the next 8 GiB KV stage cannot coexist comfortably with an 8 GiB process
hard ceiling. We cannot honestly promise healthy operation on this 16 GiB host
while allowing the KV block to grow and remain fully resident.

This does **not** require changing KV ordering or the public KV model. It does
require changing page residency before an 8 GiB expansion: retain the NVT and
page metadata needed to locate buckets, but load bucket pages through a bounded
cache/coalesced I/O path instead of retaining the entire reserved block in RAM.
The captured layout-generation bitmap design still works because ordinals come
from the stable page layout, not from every page remaining resident.

The memory diagnostics must also add KV resident/shared/retained-generation
bytes, KV buffer bytes, VoidManager bytes, GC/task bytes, plugin/parser
reservations, and unattributed RSS. Otherwise a dashboard showing "750 MiB
engine-owned" beside an 8 GiB process is not observability; it is optimism with
JSON syntax.

**Recommendation:** Freeze 6 GiB soft and 8 GiB hard limits for FS-Server1,
make auto defaults conservative percentages of physical memory on other hosts,
set GC to 128 MiB preferred/64 MiB minimum, and make bounded KV-page residency
a prerequisite before AeorDB may expand this database to an 8 GiB KV block.

Do you approve this production envelope and the KV-residency prerequisite? If
you want AeorDB's hard ceiling below 8 GiB, state the target and I will rebalance
the cache/maintenance defaults around it.

**Your answer: Agreed, though I want these configurable, and might decide to allocate some more to the files.taraani.org server in the future.**

## 4. "Skip the damaged branch" means different things for service and GC

Your desired behavior is sound for normal operation, but it needs one critical
distinction.

Imagine the root has two children:

```text
/
|-- Healthy-A/   -> fully readable
`-- Damaged-B/   -> its B-tree node cannot be decoded
```

AeorDB can safely:

- serve `Healthy-A/`;
- continue writes under `Healthy-A/`;
- return partial/warning-bearing reads for the known parts of `Damaged-B/`;
- continue scanning other branches to collect all repair evidence; and
- latch `Damaged-B/` read-only so writes do not destroy evidence or deepen the
  inconsistency.

GC cannot safely say that its mark is complete. Because the damaged node cannot
be decoded, we do not know which FileRecords, chunks, or lower B-tree nodes it
references. If we publish the partial bitmap, those hidden descendants look
unreachable. After two similarly partial marks and elapsed grace, they could be
deleted. Quarantine delays the mistake; it does not turn missing information
into proof.

The safe combined policy is:

1. Retry a suspected node through a fresh bounded read and verify header,
   checksum/hash, type, and parent edge so a transient read error is not
   mislabeled as deterministic corruption.
2. For deterministic reachable corruption, create a durable `RepairTicket`
   outside the damaged subtree. Record the nearest proven path prefix, root and
   node identities, parent edge, failure details, first/last detection times,
   and evidence references.
3. Latch the nearest safely identified subtree read-only. If separator damage
   makes the exact child range ambiguous, latch the nearest unambiguous
   ancestor. Block writes, moves, renames, deletes, restores, and parent
   operations that would mutate or detach that scope.
4. Keep healthy scopes online. Reads of the damaged scope use explicit
   best-effort semantics and surface warnings rather than silently presenting a
   partial directory as complete.
5. Let GC continue scanning healthy branches for diagnostics and progress, but
   finish with `incomplete_authoritative_tree`, publish no quarantine
   generation, and perform no reclaim.
6. A scheduled surgical-repair task consumes the ticket, gathers redundant path
   records/revisions/snapshots, and produces a repair plan. It may automatically
   apply only a reconstruction that has one provable result and passes full
   post-write verification; ambiguous recovery remains operator-approved.
7. After repair, a fresh complete mark is required before sweep is re-enabled.

The durable branch read-only latch and repair ticket are useful beyond GC. The
same mechanism should be used by verification, ordinary reads, dirty recovery,
and writer-side invariant checks so corruption has one lifecycle rather than a
different warning format in every subsystem.

**Recommendation:** Add `RepairTicket` plus path-scoped read-only latches to the
refactor plan, but preserve the rule that any unresolved authoritative branch
blocks publication of a deletion-authorizing mark. Healthy database operations
continue; only GC reclamation waits for repair.

Is this the distinction you intended, and do you approve the automatic
path-scoped read-only latch after deterministic corruption is confirmed?

**Your answer: Agreed**

## 5. Derived-index corruption can be isolated after migration

Yes: the side-by-side migration can and should guarantee support for option 2.
Copying the database does not provide that guarantee by itself; the destination
format must deliberately make artifact ownership enumerable without traversing
the damaged tree.

Every destination derived artifact must carry a validated owner identity such
as:

```text
DerivedArtifactOwner
  artifact_kind
  index_id
  generation
  source_namespace_root
  definition_fingerprint
```

The active derived-root catalog identifies which owner generations are rooted.
GC can then stream small artifact headers and protect every entity with the
matching `(index_id, generation)` even when that generation's internal tree is
unreadable. It does not need to discover family membership by traversing the
broken structure.

The failure policy becomes:

1. Mark the affected derived generation `degraded_corrupt` and preserve its
   evidence.
2. Conservatively protect its entire owner family from reclamation.
3. Continue and publish a complete authoritative-content mark.
4. Schedule a rebuild into a new immutable generation.
5. Atomically activate the verified replacement.
6. Let the old damaged family enter ordinary two-mark/grace quarantine.

If an individual artifact's owner header is itself unreadable, GC protects all
derived-artifact entities of that artifact kind for the run and reports the
leak/degradation; it still does not need to block authoritative content GC.

Migration must not raw-copy an unowned v0 derived artifact and then activate
option 2. It must either rewrite/rebuild that artifact with v1 owner metadata or
omit it and set the destination index state to `needs_rebuild`. Capability
activation is forbidden until the destination contains no active unowned
derived generation.

**Recommendation:** Lock option 2 as the destination behavior and add this
owner-metadata invariant to migration validation and old-reader capability
gating.

Do you approve this exact guarantee?

**Your answer: I am totally confused about this question. You appear to be asking about GC artifacts in our side-by-side migration... and yet, I thought we had already clearly decided that we WEREN'T going to attempt to copy in-flight GC data. What are you _actually_ asking here. What is "option 2"?**

## 6. Unified task execution is confirmed

Your expectation was correct; the implementation drifted:

- queued GC reaches `execute_gc`, but that function does not pass task or
  shutdown cancellation into `run_gc`;
- the HTTP route calls `run_gc` directly inside `spawn_blocking`; and
- progress/cancellation therefore depends on which doorway started the same
  conceptual operation.

The refactor will remove that split. One engine task implementation owns
cancellation, progress, budgets, and cleanup. Wrappers may wait or return a task
ID, but they do not contain another GC algorithm. No further policy answer is
needed for this item.

## Round 2 Test Additions

The 16 GiB requirement adds proof obligations beyond the Round 1 GC tests:

- run the real server under an 8 GiB cgroup/job-object-equivalent hard ceiling
  with a synthetic 4 GiB KV layout, active writes, reads, indexes, and GC;
- prove RSS crosses neither the configured hard limit nor the host-available
  maintenance floor under broad-directory and high-mutation workloads;
- force soft-limit transitions and prove clean-cache eviction, GC buffer
  shrinking/spill, parser admission rejection, and later recovery;
- force hard-limit transitions and prove the server retains health/status,
  streaming reads, durability flush, spill, and orderly shutdown capacity;
- account separately for resident KV pages, retained old page generations,
  write buffers, indexes, directory cache, GC, tasks, plugins, allocator/RSS
  remainder, and disk scratch;
- test bitmap in-RAM and spilled-segment modes with identical mark results;
- test valid and torn resumable checkpoints even though initial policy restarts,
  proving stale scratch can never be mistaken for published state;
- test transient B-tree read failure versus deterministic corruption, nearest
  safe path-scope selection, read-only enforcement across every writer, repair
  ticket deduplication, partial-read warnings, and fresh-mark requirement after
  repair; and
- test derived-owner corruption at tree, manifest, owner-header, and active-root
  levels, proving user-content GC remains safe while derived bytes leak only
  conservatively.

---

# Category 2: Bounded Mark Implementation - Round 3

**Status:** Answered. Category closed by the final decision record below.

Round 2 exposed two ambiguities in my wording. This round corrects them rather
than asking you to reconcile contradictory recommendations.

## 1. The finished design includes durable resume

My actual position is:

```text
Delivery Phase A
  Bounded online mark, resident bitmap, bounded work-file spills, cancellation,
  no publication from incomplete work, and restart-from-scratch after process
  loss.

Delivery Phase B
  Durable incremental checkpoints, crash/reboot resume, mutation reconciliation,
  checkpoint cleanup, and all associated fault-injection tests.

Category complete
  Only after both Phase A and Phase B are implemented and proven.
```

Restart-from-scratch is a safe implementation milestone, not the final product
policy. We must not describe GC as enterprise-ready or close this design after
Phase A. Phase A must deliberately use the same run IDs, stage model, versioned
work records, checksums, and cleanup abstraction that Phase B extends; otherwise
"phase" becomes the traditional euphemism for "rewrite it later."

### Active bitmap policy

Your RAM preference is reasonable for the dense bitmap itself:

- the current 4 GiB KV layout needs about 11.3 MiB;
- an 8 GiB layout needs about 22.6 MiB;
- a 32 GiB layout needs about 90.4 MiB; and
- the bitmap reserves through the global memory coordinator before mark starts.

The active bitmap therefore remains entirely resident when it fits the
configured GC reservation. If it does not fit, GC waits or refuses to start
with an explicit required-versus-available report. It does not silently push
the bitmap into swap or change algorithms mid-run.

I need to push back on treating swap as our spill implementation. Swap may be
disabled, capped by a cgroup, exhausted by another process, or slow enough to
make health requests time out for minutes. It is an emergency OS mechanism, not
a database memory-budget API. AeorDB should stay within its resident reservation
when swap is completely disabled; tests will prove that case.

The bitmap is not the main reason a work directory exists. A broad namespace
frontier, path-sensitive visit set, mutation journal, sorted candidate runs,
diagnostics, and durable checkpoints can be much larger than 100 MiB. Those
structures must remain bounded through work files even while the bitmap stays
resident.

<!-- Note from user: Yes, agreed. I was talking from the perspective/assumption of _nothing is durable, in-RAM processes only_. Now that I know we ARE targeting durable as the final product, I agree with you entirely. Thank you for thinking properly like a senior engineer, and thank you for pushing back. -->

## 2. `/tmp` is safe for Phase A, but not sufficient for Phase B

For nonresumable Phase A work, reboot deletion is harmless:

1. no temporary record authorizes quarantine or sweep;
2. the previous complete `GcArtifact` generation remains authoritative;
3. reboot removes or leaves disposable work;
4. startup records the abandoned run; and
5. the next GC starts a fresh mark.

Therefore Phase A may use the platform temporary directory by default:

```text
Linux/macOS: <os-temp>/aeordb/gc/<database-id>/<run-id>/
Windows:     <os-temp>\AeorDB\gc\<database-id>\<run-id>\
```

The directory is private to the service identity, created without following
symlinks, bound to the database/run ID, space-preflighted, and checksummed.
AeorDB must detect memory-backed temporary filesystems where possible. A tmpfs
does not solve memory pressure; in that case GC either uses an explicitly
configured filesystem-backed work directory or refuses work that needs spill.

For durable Phase B resume, `/tmp` cannot be the only copy because an ordinary
reboot may erase seven hours of otherwise valid progress. I recommend a hybrid:

### Inside `.aeordb`: small authoritative run control

A versioned `GcArtifact` control/checkpoint manifest stores:

- database and run identity;
- captured roots, layout generation, and capability/config fingerprints;
- checkpoint generation and last reconciled write sequence;
- durable-workspace identity plus object checksums;
- task phase/status and cleanup state; and
- the previous valid checkpoint pointer for A/B fallback.

This small record says which checkpoint is recognized. It contains no bitmap,
frontier, or large sort runs. A torn or missing external object makes the run
nonresumable; it never advances quarantine.

### Outside `.aeordb`: durable bulk work

Use an administrator-configurable `gc.work_dir`. Its platform default is the
OS application-state location, not the database directory and not `/tmp`:

```text
Linux user:    $XDG_STATE_HOME/aeordb/gc/ or ~/.local/state/aeordb/gc/
Linux service: service StateDirectory, normally /var/lib/aeordb/gc/
macOS:         ~/Library/Application Support/AeorDB/gc/
Windows:       %LOCALAPPDATA%\AeorDB\State\gc\
```

It stores immutable/checksummed bitmap checkpoint segments, frontier runs,
path-visit generations, mutation-journal runs, and diagnostic runs. Checkpoint
publication syncs these objects before publishing the small control manifest
inside `.aeordb`.

The default checkpoint cadence should be configurable, initially every five
minutes or one GiB of newly processed logical work, whichever comes first, plus
a final graceful-shutdown checkpoint. Checkpoints write only dirty/new segments
and compact obsolete runs under the same memory/disk budget.

If the durable work directory is unavailable, full resumable mode fails
explicitly. AeorDB may run nonresumable GC only when the administrator requested
that mode; it never silently downgrades an enterprise resumable task to `/tmp`.

If the external state is lost, corrupted, or restored out of sync with the
database, AeorDB abandons the run, preserves the last complete quarantine
generation, and restarts from scratch. Losing work loses time, never user data.

### Migration remains unchanged

The side-by-side database migration copies **none** of this in-flight GC state:

- no run control/checkpoint manifest;
- no bitmap/frontier/path/mutation work;
- no active quarantine candidates or grace state;
- no sweep receipts or GC audit history; and
- no corrupt-GC evidence.

The destination starts `gc_state = never_marked` and performs a fresh complete
mark. The source's external work remains associated with the source database ID
for rollback/resume or later cleanup. This is exactly the migration policy we
already approved.

### Final workspace question

**Recommendation:** Use RAM for the active bitmap; OS temp for explicitly
nonresumable Phase A work; and the hybrid small-in-database control plus
configurable OS application-state bulk workspace for finished durable resume.
Do not put the bulk checkpoint stream into `.aeordb`, because repeated bitmap/
frontier checkpoints would inflate and churn the database being collected.

Do you approve this finished storage policy and checkpoint cadence?

**Your answer: I agree with all of this. When it comes to durable storage, I'd prefer we use files side-by-side with the database itself... i.e. if we have `files.taraani.org.aeordb` as the database file, then I'd like to see `.files.taraani.org.aeordb-gc-database-id-run-id`. However, I'd like this to be the _default_... the admin can still configure a "durable GC scratch-pad location" where these files could be located instead.**

## 3. "Option 2" referred to search indexes, not GC state

No further answer is needed here. My terminology was poor.

There are three different entity families:

| Family | Examples | Migration behavior |
|---|---|---|
| Authoritative user/database data | directories, files, chunks, snapshots, forks, users/config | Migrated into the destination's new entity format. |
| GC state | active quarantine, in-flight checkpoints, receipts, GC audit/evidence | Not copied. Destination starts never-marked. |
| Derived search-index data | NVT tiles, postings, canonical-value pages, index manifests | Rebuilt/rewritten with the new format or omitted as `needs_rebuild`; never blindly activated as unowned v0 data. |

The earlier "option 2" meant **derived-corruption isolation**:

> If one rebuildable search-index generation is corrupt, protect that entire
> index generation and continue authoritative user-data GC, instead of letting
> a disposable search index block all garbage collection.

It said nothing about copying in-flight GC data. The migration guarantee needed
for derived-corruption isolation is simply that every new derived search-index
artifact identifies its `(index_id, generation)` owner. That lets GC protect a
damaged index family without traversing it. Old derived artifacts are rebuilt
with ownership metadata or omitted; GC state is still omitted entirely.

Your Round 1 preference for derived-corruption isolation is recorded as the
decision. No additional policy choice remains.

## 4. Decisions now locked from Round 2

- FS-Server1 uses configurable limits initially set to 6 GiB soft and 8 GiB
  hard; administrators may raise or lower them deliberately.
- Other installations receive conservative RAM-derived defaults plus explicit
  overrides.
- GC reserves 128 MiB preferred/64 MiB minimum inside the global process
  envelope; it is not extra headroom beyond the hard limit.
- The memory coordinator, expanded ownership metrics, and bounded KV-page
  residency are required before an 8 GiB KV-stage expansion.
- Deterministic authoritative B-tree corruption creates a durable repair ticket
  and nearest-safe path-scoped read-only latch.
- Healthy scopes remain readable/writable and damaged scopes remain
  warning-bearing readable where possible.
- Any unresolved authoritative branch makes the GC mark incomplete and blocks
  quarantine publication/reclaim until repair plus a fresh complete mark.
- Derived search-index corruption is isolated by owner family after migration;
  it does not block authoritative user-content GC.
- All GC invocation paths use one task/run implementation with the same
  cancellation, budgets, progress, and cleanup semantics.

## Round 3 Test Clarifications

- Run bounded-memory GC with swap disabled and prove the process remains below
  its reservation and configured hard ceiling.
- Remove the Phase A temp directory during simulated crash/reboot and prove only
  progress is lost; quarantine and sweep state remain unchanged.
- Crash before/after every durable bulk-object sync and in-database checkpoint
  pointer publication, then prove A/B fallback or safe restart.
- Change KV layout, roots, capabilities, and config between checkpoint and
  restart and prove only compatible state resumes.
- Delete, truncate, reorder, duplicate, and tamper with durable work objects and
  prove the run is abandoned without candidate publication.
- Exhaust the durable work filesystem while normal writes continue and prove
  GC becomes incomplete without failing those durable writes.
- Restore/copy a database without external work state and prove it starts with
  the correct nonresumable/never-marked status rather than searching unrelated
  state by filename.
- Verify migration excludes every GC-state family while separately rebuilding
  or omitting derived search-index artifacts with owner metadata.

---

## Bounded Mark Category: Final Decision Record

**Status:** Closed. No further policy questions remain in this category.

### 1. Safety Model and Completion Boundary

- Marking is online and may run while normal reads and writes continue.
- Only a complete, verified mark can publish a new quarantine generation.
- A canceled, crashed, corrupt, stale, resource-exhausted, or otherwise
  incomplete run may publish diagnostics, but it cannot classify anything as
  reclaimable.
- Physical liveness and path-sensitive traversal are separate concerns. The
  liveness set tracks physical entity incarnations exactly; path traversal may
  additionally track `(directory identity, logical path)` when permissions,
  links, or namespace projection require it.
- Payload chunks are marked from verified references without reading their
  payload bytes. Metadata traversal uses exact, length-bounded entity reads.

### 2. Exact Bounded Mark Representation

- At run start, flush the KV write buffer, capture the stable KV layout
  generation, root set, configuration/capability fingerprint, and durable write
  sequence boundary.
- Represent physical liveness as a dense bitmap addressed by the stable KV
  `(bucket, slot)` location. The key stored in that slot remains the authority;
  the bitmap is only an exact one-bit liveness projection for that captured
  layout.
- Expected bitmap sizes are approximately 11.3 MiB for a 4 GiB KV layout, 22.6
  MiB for 8 GiB, and 90.4 MiB for 32 GiB.
- The active bitmap stays resident in RAM after reserving its full size from the
  global memory coordinator. If it cannot reserve the required memory, GC waits
  or refuses to start with an explicit required-versus-available report. It
  does not depend on swap or silently change representation.
- A KV layout rebuild, resize, or generation change invalidates the run unless
  an explicitly proven remap exists. The initial implementation restarts.
- KV scanning is page-streamed. No GC path may materialize the full KV table,
  all keys, all B-tree children, or all candidates in a single `Vec`/`HashSet`.

### 3. Bounded Traversal and Work Runs

- Replace the monolithic live `HashSet`, unbounded BFS queues, recursive
  all-child materialization, and full-scan vectors with bounded iterators and
  partitioned external work runs.
- Use a streaming B-tree visitor that yields validated child/reference batches
  under a fixed memory reservation and releases each batch before continuing.
- Frontier entries, path-sensitive visits, mutation catch-up, candidate-sort
  runs, and diagnostics use versioned, length-bounded, checksummed work records.
- Work buffers reserve memory before allocation, spill at configured thresholds,
  and compact incrementally. Scratch exhaustion makes the GC run incomplete;
  it does not fail an otherwise durable user write.
- Candidate enumeration streams unmarked occupied KV slots into sorted runs.
  Publication revalidates entity identity and current reachability before a
  candidate can enter quarantine.

### 4. Concurrent Writes and Publication Guards

- Replace `gc_recheck` with a bounded, spillable `GcMutationJournal` containing
  every root/reference/incarnation change after the captured mark boundary.
- Writers take a shared publication guard before dependency resolution and hold
  it through durable root/KV publication. This protocol applies to every writer
  path, including HTTP, embedded APIs, maintenance, plugins, repair, and batch
  operations.
- GC drains and reconciles the journal in bounded passes while normal writes
  continue.
- Final publication takes a short exclusive guard, captures the final sequence
  boundary, drains the remaining journal, verifies that no sequence gap exists,
  and only then publishes the immutable quarantine generation.
- Reclamation uses short exclusive batches with exact incarnation,
  reachability, reader/task-pin, and grace-period rechecks. It never holds the
  database read-only for the duration of mark.
- If mutation volume prevents convergence, GC records catch-up starvation and
  publishes nothing. It may retry later with adjusted bounded batches; it may
  not weaken the proof.

### 5. Process-Wide Memory Governance

- Memory limits cover the whole process, not just GC. Initial FS-Server1 values
  are configurable 6 GiB soft and 8 GiB hard limits.
- Other installations derive conservative defaults from available RAM and may
  override them explicitly.
- GC receives a preferred 128 MiB and minimum 64 MiB reservation inside that
  global envelope. These values are not extra headroom above the process limit.
- The coordinator accounts for resident KV pages, retained page generations,
  write/index buffers, loaded indexes, directory caches, GC, tasks, plugins,
  parser amplification, allocator/RSS remainder, and emergency durability/
  shutdown headroom.
- Soft pressure first evicts clean/rebuildable caches, shrinks or spills GC
  buffers, and rejects new memory-amplifying maintenance. Hard pressure stops
  new large work while preserving health/status, streaming reads, durable small
  writes, spill, and orderly shutdown capacity.
- Bounded KV-page residency and complete memory attribution are prerequisites
  to an 8 GiB KV-stage expansion. The current whole-KV `ReadSnapshot` behavior
  cannot remain hidden outside memory metrics.
- Correctness and bounded operation must be proven with swap disabled.

### 6. Workspaces and Durable Resume

- Delivery Phase A may use disposable OS temporary work and restart from
  scratch after process loss. It must already use final run IDs, stages,
  versioned records, checksums, and workspace APIs.
- Delivery Phase B adds durable incremental checkpoints, crash/reboot resume,
  mutation reconciliation, safe cleanup, and fault-injection coverage. This
  category is not implementation-complete until both phases are delivered.
- The default durable bulk workspace is a private hidden sibling of the
  database file:

```text
<database-parent>/.<database-filename>-gc-<database-id>-<run-id>/

Example:
.files.taraani.org.aeordb-gc-<database-id>-<run-id>/
```

- An administrator may configure a different durable GC scratch-pad root. The
  engine creates database-ID and run-ID scoped directories beneath it.
- Workspaces are private to the service identity (mode `0700` or the platform
  equivalent), created without following symlinks, bound to the expected
  database/run identity, space-preflighted, and continuously constrained by a
  free-space reserve and optional per-run maximum.
- A small versioned `GcArtifact` control manifest inside `.aeordb` identifies
  the recognized checkpoint generation, captured roots/layout/capabilities,
  reconciled write sequence, external workspace identity and checksums, task
  state, cleanup state, and previous valid checkpoint pointer.
- The bitmap checkpoints, frontier/path runs, mutation journal, candidate runs,
  and diagnostics remain in the sibling/overridden workspace. Repeated bulk
  checkpoint streams are not appended to the database being collected.
- Checkpoint bulk objects are immutable and synced before the inactive in-DB
  A/B control slot publishes them. The previous valid generation remains until
  the new generation is verified.
- Default checkpoint cadence is configurable and initially five minutes or one
  GiB of newly processed logical work, whichever comes first, plus graceful
  shutdown. Only dirty/new segments are written.
- Missing, torn, tampered, incompatible, or out-of-sync external state makes
  the run nonresumable. AeorDB preserves the last complete quarantine
  generation and restarts mark; lost work loses time, never user data.
- If durable storage is unavailable, resumable mode fails explicitly. AeorDB
  uses disposable/nonresumable mode only when the administrator selected it; it
  never silently downgrades.

### 7. Cancellation, Startup, and Cleanup

- Every GC loop and blocking batch observes the common task cancellation token
  at bounded intervals.
- Explicit cancellation publishes no quarantine state. Phase A removes or
  abandons disposable work; Phase B may write a final internally consistent
  canceled checkpoint for explicit later resume or cleanup policy.
- Graceful shutdown requests a bounded final checkpoint. Crash/startup resumes
  only after validating database identity, layout generation, captured roots,
  capability/config fingerprint, write-sequence continuity, manifests, and all
  referenced checksums.
- Startup inventories stale sibling and configured-root workspaces by embedded
  database/run identity, never by filename alone. Unknown or damaged work is
  reported and conservatively cleaned only under the defined retention policy.
- No cleanup operation may remove the active checkpoint, its A/B fallback, or
  evidence needed to explain an abandoned run.

### 8. Authoritative B-Tree Corruption

- GC uses strict structural validation but diagnostic traversal: it records the
  exact tree/page/path failure and continues independent branches when that is
  provably safe.
- A deterministic failure in an authoritative tree creates or deduplicates a
  durable `RepairTicket` and applies a nearest-safe path-scoped read-only latch.
- Healthy scopes remain readable and writable. Damaged scopes remain readable
  with explicit partial/corrupt warnings wherever safe decoding permits it.
- Any untraversable authoritative branch makes the mark incomplete. GC cannot
  publish quarantine or reclaim until repair succeeds and a fresh complete mark
  proves reachability.
- Transient I/O failures are retried under bounded policy and diagnosed
  separately; they do not become silent corruption or silently skipped roots.

### 9. Derived Search-Index Corruption

- New derived artifacts identify their `(index_id, generation)` owner in
  versioned metadata.
- Corruption isolated to one rebuildable search-index generation protects that
  entire owner family and marks it `needs_rebuild`, while authoritative
  user-content GC may continue if its own reachability proof is complete.
- Missing or malformed owner metadata conservatively protects all artifacts of
  that derived type. It never permits guessed reclamation.
- Old derived artifacts are rebuilt with ownership metadata during migration or
  omitted as `needs_rebuild`; they are not blindly activated as unowned v0 data.

### 10. One GC Execution Path

- CLI, HTTP, startup/automatic cadence, repair follow-up, and embedded APIs all
  create the same `GcRunContext` and execute the same task implementation.
- Wrappers may wait, stream progress, or return a task ID. They do not implement
  separate traversal, cancellation, memory, checkpoint, or cleanup behavior.
- Progress, phase, ETA, memory reservation, scratch use, journal lag,
  checkpoint age, corruption scope, and completion/incompleteness reason are
  exposed consistently through tasks, health, metrics, logs, diagnostics, and
  the dashboard.

### 11. Side-by-Side Migration

- Migration copies no in-flight GC control, bitmap/frontier/path/mutation work,
  quarantine/grace state, receipts, GC history, or corrupt-GC evidence.
- The destination starts `gc_state = never_marked` and must complete a fresh
  mark before any sweep.
- Source sibling work remains bound to the source database ID for rollback,
  resume, evidence, or later cleanup. It is never discovered by destination
  filename matching.
- Derived search-index data is a separate family: migrate by rebuilding it with
  owner metadata or omit it as `needs_rebuild`.
- The source migration lease suspends mutating GC and GC-retention cleanup until
  migration completes, aborts, or safely recovers a stale lease.

### 12. Required Verification Matrix

Implementation is not complete until automated and real-world tests cover:

- identical exact mark results across tiny forced spill budgets, broad/deep/
  cyclic namespaces, links, snapshots/forks, and large chunk populations;
- a real server under an 8 GiB cgroup/job-object-equivalent hard ceiling with a
  synthetic 4 GiB KV layout, active reads/writes/indexes, and swap disabled;
- soft/hard memory transitions, complete ownership metrics, bounded KV page
  residency, scratch exhaustion, and recovery after pressure subsides;
- writer races at every dependency/root publication point, mutation-journal
  spill/compaction, sequence gaps, convergence starvation, and final guard
  boundaries;
- cancellation at every stage and publication crash point, proving incomplete
  work never changes quarantine or sweep eligibility;
- crash before/after every bulk-object sync and A/B control publication,
  checkpoint fallback/resume, stale layout/root/config rejection, tampering,
  truncation, reordering, duplication, and missing workspace data;
- default sibling and configured workspace placement, permissions, symlink
  attacks, identity collision, free-space reserve, stale cleanup, and reboot;
- transient and deterministic B-tree failures, independent-branch diagnostics,
  repair-ticket/latch behavior across every writer, repair, and mandatory fresh
  complete mark;
- derived-owner corruption at tree, manifest, owner-header, and active-root
  levels without putting authoritative user content at risk;
- identical behavior through CLI, HTTP, scheduled, repair, and embedded entry
  points; and
- migration completion/abort/crash, source lease handling, no copied GC state,
  destination `never_marked`, source workspace retention, and rebuilt/omitted
  derived indexes.

At least one `/tmp/codex` real-world database run and one constrained production-
shape soak must exercise spills, online mutation, checkpoint/restart, repair
diagnostics, and a full mark-to-quarantine publication before release.

---

# Category 3: Stable Root API Contract - Round 1

**Status:** Answered. Category closed by the final decision record below.

## Direction Now Locked

The encoded root cursor is removed from the design. An immutable read view has
one authoritative selector: the namespace **root_hash**.

The fields previously proposed for a cursor were a format version, database
identity, root hash, and advisory expiration. Format version and database
identity existed only to make an opaque token portable and self-describing.
They are unnecessary when a request is already addressed to one AeorDB database.
Expiration does not select data and belongs in the response.

The common request contract is therefore:

~~~json
{
  "root_hash": "<optional full lowercase hex hash>",
  "limit": 100,
  "offset": 200
}
~~~

If **root_hash** is omitted, the server captures current HEAD exactly once at
request admission. Every successful read reports the root actually used:

~~~json
{
  "root": {
    "hash": "<full lowercase hex hash>",
    "state": "live",
    "expires_at": null
  },
  "results": []
}
~~~

A retained root pending GC reports:

~~~json
{
  "root": {
    "hash": "<full lowercase hex hash>",
    "state": "pending_delete",
    "expires_at": 1780000000000
  },
  "results": []
}
~~~

Existing route-specific collection names such as **results**, **items**, or
keyed path objects remain compatible; the example does not rename them.

**expires_at** is an advisory millisecond timestamp. For a pending root it is
the earliest time at which that root may become unavailable under the current
pending-delete grace policy. A later complete mark, GC cadence, pins, snapshots,
or conservative recovery may retain it longer. Passing this timestamp does not
itself invalidate a still-readable root, and reading does not refresh retention.

A supplied root that is logically expired, reclaimed, or no longer resolvable
returns HTTP 410. It never falls through to HEAD. Knowledge of a root hash grants
no authority; normal path, scope, share, JWT, and API-key authorization is
reapplied before every observable result.

For raw or compatibility-sensitive responses, equivalent metadata is returned
in headers:

~~~http
X-AeorDB-Root-Hash: <hash>
X-AeorDB-Root-State: live
X-AeorDB-Root-Expires-At:
~~~

There is no root-cursor mint endpoint, root-cursor encoding, cursor signature,
cursor database-mismatch state, or stored cursor object. Logical pagination
positions remain a separate concern.

## 1. Which read surfaces accept root_hash?

**Recommendation:** Use one engine-level ReadViewSelector and ResolvedReadView
for every namespace read:

1. POST /files/query;
2. POST /files/search;
3. directory listing through GET /files and GET /files/{path};
4. individual file GET;
5. POST /files/fetch, including whole-file, line, character, and JSON range
   extraction; and
6. embedded SDK equivalents.

POST routes carry **root_hash** in their JSON body. GET routes carry it as a
query parameter. JSON collection envelopes include **root**; every read also
emits the root headers so raw file responses and existing keyed batch-fetch
responses do not require incompatible envelope changes.

This is required for the bot workflow:

    search root X -> receive match ranges -> fetch those exact ranges from root X

**if_content_hash** remains useful as an additional assertion, but it is not a
substitute for retrieving historical bytes. Mutation routes reject
**root_hash**; a historical root is an immutable view, not a writable branch.

Do you approve this coverage for all namespace reads?

**Your answer: Yes, approved (and, ironically, things were _already supposed to be implemented this way_--more implementation drift)**

## 2. Which directory hashes qualify as namespace roots?

A valid DirectoryIndex hash is not necessarily a complete namespace root. It may
be an internal B-tree page or a nested directory root. Its bytes alone do not
prove the absolute path where it was mounted.

Two coherent policies are possible.

### Policy A: Known Complete Namespace Roots

Accept a hash only if it is structurally valid and known as a complete namespace
root: current or former HEAD, snapshot, fork, migration/maintenance root, or a
pending-GC historical root.

This needs durable root-provenance metadata during migration, but stores no
cursor or lease. Absolute paths, inherited configuration, index scope, and
authorization retain their normal meaning.

### Policy B: Any Valid Directory Closure

Treat any structurally valid retained directory closure as synthetic root.
This is more flexible, but requires complete path-rebasing semantics for
permissions, FileRecord paths, configuration inheritance, index scope,
listings, counts, and query observables. Rechecking only returned FileRecord
paths is insufficient because planning, totals, names, and timing can reveal
information first.

**Recommendation:** Ship Policy A for v1. If subtree-root views later prove
useful, add an explicit canonical anchor-path contract instead of pretending a
subtree was originally mounted at root.

Do you choose known complete namespace roots, or arbitrary valid directory
closures with synthetic-root semantics?

**Your answer: Agreed. We only need actual roots. FYI: a snapshot IS simply a root folder hash... so essentially we are defining "snapshots" when requesting a "root_hash" (though, "essentially" is loose here, because a root_hash is not a snapshot, even though a snapshot is a root_hash... I am just explaining that more often than not, a user will be supplying a snapshot hash, which is a true "root" hash)**

## 3. Which pagination combinations are legal?

The **root_hash** fixes the immutable dataset. It does not identify a logical
position within an ordered result set.

**Recommendation:** Apply one strict matrix to query, search, and listing:

| Parameters | Valid? | Meaning |
|---|---:|---|
| limit | Yes | Bounded page size; server default if omitted. |
| page plus optional limit | Yes | One-based page using checked arithmetic. |
| offset plus optional limit | Yes | Zero-based absolute logical offset. |
| after plus optional limit | Yes | Items strictly after a logical position. |
| before plus optional limit | Yes | Closest preceding items in requested order. |
| page plus offset/after/before | No | Ambiguous. |
| offset plus after/before | No | Competing window origins. |
| after plus before | No | Ambiguous window semantics. |

Every ordering becomes total by appending an immutable FileKey/incarnation
tie-breaker. Default order is canonical path ascending plus that tie-breaker.
**before** returns the immediately preceding page, not the first limit items
before the position.

Page zero, overflow, an oversized limit, a malformed position, or a position
missing from the selected immutable root fails before query planning. It never
silently becomes the first page.

Do you approve this strict matrix, including rejection of the current
after-then-offset behavior?

**Your answer: Yes, approved.**

## 4. How do logical position tokens transition?

Keep the public names **after**, **before**, **next_cursor**, and
**prev_cursor**, but define their values as logical position tokens, never root
selectors.

The new position token contains only:

    format and kind version
    root hash for mismatch detection
    order fingerprint
    canonical sort-value tuple
    immutable FileKey/incarnation tie-breaker

It contains no physical page, WAL offset, NVT cell, manifest, or planner state.
The position changes between pages; the request **root_hash** remains unchanged.

**Recommendation:** For one transition release:

- requests without root_hash retain HEAD-at-request behavior;
- existing hash/version position tokens remain accepted where safe;
- responses add root metadata without removing current pagination fields;
- new position tokens are explicitly tagged and versioned;
- root or order mismatches fail explicitly; and
- telemetry measures legacy use before any separately announced removal.

Do you approve this separation and one-transition-release compatibility policy,
or should legacy support have a different minimum lifetime?

**Your answer: I approve, and we don't need to treat "legacy" differently**

## 5. Which stable errors should clients receive?

Use the existing JSON error envelope with machine-readable codes:

| HTTP | Code | Condition |
|---:|---|---|
| 400 | INVALID_ROOT_HASH | Malformed, wrong-length, or noncanonical hash. |
| 400 | INVALID_PAGINATION | Ambiguous, zero/negative, oversized, or overflowing window. |
| 400 | INVALID_POSITION_CURSOR | Malformed logical position. |
| 409 | POSITION_ROOT_MISMATCH | Position token names a different root. |
| 409 | POSITION_ORDER_MISMATCH | Position token does not match requested ordering. |
| 410 | ROOT_EXPIRED | Previously valid root is logically unavailable or reclaimed. |
| 422 | INVALID_NAMESPACE_ROOT | Hash resolves but is not admissible as a namespace root. |
| 503 | HISTORICAL_VIEW_UNAVAILABLE | Exact historical index/parser dependency is temporarily unavailable. |
| 500 | DATABASE_CORRUPTION | An admitted retained root or closure is structurally corrupt. |

A random well-formed hash that was never an admissible namespace root returns
INVALID_NAMESPACE_ROOT, not ROOT_EXPIRED. Authorization preserves each route's
existing concealment behavior, such as HTTP 404 or filtered results where
required. Root metadata never upgrades authority or exposes unauthorized counts,
names, scores, snippets, positions, or EXPLAIN details.

Do you approve these statuses and codes?

**Your answer: Yes, approved**

## Round 1 Proof Obligations

- Omitted root_hash captures HEAD once and reports it consistently while
  concurrent writes advance HEAD.
- Query, search, listing, full fetch, and range fetch resolve identical paths,
  FileRecords, bytes, and content hashes for one supplied root.
- Search locator ranges fetched from the reported root reproduce the exact bytes
  that generated the locator.
- Live roots report expires_at null; pending roots report the earliest advisory
  boundary; reading does not extend it.
- Pending-root request pins win concurrent sweep races. Eligible or reclaimed
  roots return HTTP 410 without falling through to HEAD.
- Forward and backward pages contain no duplicates or omissions under duplicate
  sort values, and page, offset, and keyset results match a reference evaluator.
- Every invalid pagination combination and mismatch fails before index or
  namespace amplification.
- Authorization precedes results, totals, groups, names, scores, snippets,
  position tokens, root metadata, and EXPLAIN observables on historical views.
- Legacy clients pass through the declared transition release while new clients
  reuse one reported root_hash across every page and fetch request.

<!-- FYI: Note from user: We don't need to worry too much about legacy semantics. I am the only user of aeordb currently. I would certainly be more concerned if it was being used out in the wild... but it isn't. -->

---

## Stable Root API Category: Final Decision Record

**Status:** Closed. No further policy questions remain in this category.

### 1. Root Hash Is the Read-View Selector

- AeorDB does not create an encoded, signed, encrypted, persisted, or otherwise
  opaque root cursor.
- An optional full hexadecimal `root_hash` selects one immutable namespace root.
- If omitted, the server captures current HEAD exactly once at request
  admission. Every part of that request uses the captured value even if HEAD
  advances concurrently.
- A supplied root that cannot be used never falls through to HEAD.
- `root_hash` grants no authority and contains no pagination, ordering, planner,
  NVT, page, WAL, or physical-index state.
- Mutation routes reject `root_hash`. A historical root is an immutable read
  view, not a writable fork.

The removal of an encoded root cursor also removes cursor format versions,
database IDs inside tokens, root-cursor mint endpoints, cursor signatures,
stored cursor objects, and client-provided expiration fields.

### 2. Root Metadata Is Returned on Every Read

Every successful namespace read reports the root actually used:

~~~json
{
  "root": {
    "hash": "<full lowercase hex hash>",
    "state": "live",
    "expires_at": null
  }
}
~~~

The defined root states are initially:

- `live`: reachable from an authoritative root such as HEAD, snapshot, fork, or
  active maintenance/migration state; `expires_at` is `null`;
- `pending_delete`: no longer authoritatively reachable but still retained by
  GC quarantine; `expires_at` is the earliest advisory millisecond boundary at
  which it may become unavailable.

Existing response collection fields remain unchanged. Query/search/listing
envelopes add `root`. All namespace reads also emit:

~~~http
X-AeorDB-Root-Hash: <hash>
X-AeorDB-Root-State: live
X-AeorDB-Root-Expires-At:
~~~

The headers preserve raw file responses and compatibility-sensitive keyed batch
fetch shapes. JSON APIs may include both the envelope field and headers.

### 3. Expiration Is Advisory; Root State Is Authoritative

- `expires_at` is server-produced response metadata, never a request parameter.
- For a pending root it means "may become unavailable at or after this time,"
  not a promise that sweep runs at that instant.
- GC cadence, the required later complete mark, request/task pins, snapshots,
  forks, migration roots, and conservative recovery may retain a root longer.
- Reading a pending root does not refresh, clear, or extend its quarantine.
- Passing the advisory time does not itself make a retained root unreadable.
- A root becomes logically unavailable when GC eligibility/reclamation state
  says it is unavailable, even if physical bytes have not yet been overwritten.
- Such a request returns `410 ROOT_EXPIRED` and never current HEAD.
- A request admitted while the root is readable takes a request-lifetime pin;
  concurrent sweep cannot reclaim its closure before that request completes.

### 4. One Read-View Resolver Covers Every Namespace Read

One engine-level `ReadViewSelector` resolves to one `ResolvedReadView` and is
used by:

1. `POST /files/query`;
2. `POST /files/search`;
3. `GET /files` and `GET /files/{path}` directory listing;
4. individual file GET;
5. `POST /files/fetch`, including whole-file, line, character, and JSON range
   extraction; and
6. embedded SDK equivalents.

POST requests carry `root_hash` in JSON. GET requests carry it as a query
parameter. The resolver validates/adopts the root, pins it, exposes root status,
and supplies root-aware namespace operations; route handlers do not each
implement historical lookup.

This makes the bot workflow coherent:

~~~text
search root X -> receive match ranges -> fetch exact ranges from root X
~~~

`if_content_hash` and `if_updated_at` remain optional assertions. They detect
caller mistakes but do not replace historical-root fetch support.

### 5. Only Proven Complete Namespace Roots Are Admissible

- A supplied hash must resolve to a structurally valid complete namespace root.
- Internal B-tree pages and nested directory roots are not accepted as synthetic
  `/` views.
- Admissible roots include current/former HEAD roots, snapshots, forks, active
  migration/maintenance roots, and pending-GC historical roots.
- A snapshot remains its existing named durable record pointing to a root hash.
  Supplying that root hash selects the same immutable namespace content without
  pretending the raw hash itself is a snapshot object.
- A future subtree-view feature must carry an explicit canonical anchor path and
  define rebasing semantics. It cannot reinterpret arbitrary directory hashes as
  namespace roots under this contract.

The new-format migration must provide durable root-provenance support sufficient
to distinguish a proven former namespace root from an arbitrary DirectoryIndex
hash. Provenance records do not pin the root forever. Current authoritative root
references and GC quarantine determine retention; provenance establishes type/
history and supports deterministic `INVALID_NAMESPACE_ROOT` versus
`ROOT_EXPIRED` behavior.

Implementation should piggyback root provenance on the existing durable HEAD,
snapshot, fork, migration, and quarantine publication paths rather than append
an unrelated per-read record or maintain a cursor registry.

### 6. Authorization and Historical Semantics

- JWT, share, API-key, user/group, path, and system-namespace restrictions are
  reapplied to the selected root.
- Authorization and path/config scope resolution occur before results, names,
  totals, groups, scores, snippets, pagination positions, root metadata, timing
  shortcuts, or `EXPLAIN` details become observable.
- Historical queries resolve index definitions, parser/plugin dependencies, and
  inherited configuration from the selected root.
- A matching retained index generation may accelerate the read only when its
  source root and full definition fingerprint match.
- Otherwise AeorDB performs an exact authoritative historical fallback when
  available or returns `HISTORICAL_VIEW_UNAVAILABLE`. It never evaluates an old
  root using incompatible current-HEAD semantics.

### 7. Pagination Parameter Matrix

The following contract applies consistently to query, search, and listing:

| Parameters | Valid | Meaning |
|---|---:|---|
| `limit` | Yes | Bounded page size; server default if omitted. |
| `page` plus optional `limit` | Yes | One-based page with checked arithmetic. |
| `offset` plus optional `limit` | Yes | Zero-based absolute logical offset. |
| `after` plus optional `limit` | Yes | Items strictly after a logical position. |
| `before` plus optional `limit` | Yes | Closest preceding items in requested order. |
| `page` plus `offset`/`after`/`before` | No | Ambiguous origins. |
| `offset` plus `after`/`before` | No | Competing origins. |
| `after` plus `before` | No | Ambiguous window. |

- `page = 0`, checked-arithmetic overflow, oversized limits, malformed
  positions, and positions absent from the immutable root fail before planning.
- The current `after`-then-`offset` behavior is removed.
- Every ordering becomes total by appending an immutable FileKey/incarnation
  tie-breaker.
- Default order is canonical path ascending plus that tie-breaker.
- `before` returns the immediately preceding page in requested order, not the
  first `limit` records before the position.
- Deep offset/page execution must use rank metadata or bounded scans where
  possible; accepting an offset does not permit unbounded result materialization.

### 8. Logical Position Tokens Remain Separate

The public fields `after`, `before`, `next_cursor`, and `prev_cursor` remain,
but their values represent only logical ordered positions.

A new position token contains:

~~~text
format and kind version
selected root hash for mismatch detection
order fingerprint
canonical sort-value tuple
immutable FileKey/incarnation tie-breaker
~~~

- It contains no physical page, WAL offset, NVT cell, artifact manifest, or
  planner state.
- It is bounded, strictly decoded, and opaque to callers.
- It changes between pages; `root_hash` remains constant.
- `after` or `before` requires the request to supply `root_hash`. Otherwise the
  position token would become an implicit root selector and recreate the cursor
  design that was removed.
- Root and order mismatches fail explicitly.

### 9. Coordinated Cutover; No Legacy Mode

AeorDB currently has one controlled user/deployment environment. Therefore:

- retain useful public parameter/response names;
- replace the legacy position payload with the new versioned logical format;
- update the web UI, bundled SDK/client, tests, and documentation in the same
  release;
- do not build a dual legacy-token decoder, legacy behavior switch, telemetry
  deprecation window, or guaranteed transition release;
- requests without `root_hash` remain valid for an initial read and capture
  HEAD, but stable continuation requests send the returned root hash; and
- old in-flight position tokens may fail after deployment, which is accepted for
  this coordinated pre-public compatibility boundary.

### 10. Stable Error Contract

Use the existing `{ "error": "...", "code": "..." }` envelope:

| HTTP | Code | Condition |
|---:|---|---|
| 400 | `INVALID_ROOT_HASH` | Malformed, wrong-length, or noncanonical hash. |
| 400 | `INVALID_PAGINATION` | Ambiguous, zero/negative, oversized, or overflowing window. |
| 400 | `INVALID_POSITION_CURSOR` | Malformed logical position. |
| 409 | `POSITION_ROOT_MISMATCH` | Position token names a different root. |
| 409 | `POSITION_ORDER_MISMATCH` | Position token does not match requested ordering. |
| 410 | `ROOT_EXPIRED` | Previously valid root is logically unavailable/reclaimed. |
| 422 | `INVALID_NAMESPACE_ROOT` | Hash resolves but is not an admissible namespace root. |
| 503 | `HISTORICAL_VIEW_UNAVAILABLE` | Exact historical dependency is unavailable. |
| 500 | `DATABASE_CORRUPTION` | An admitted retained root/closure is corrupt. |

A random well-formed hash that was never an admissible root returns
`INVALID_NAMESPACE_ROOT`, not `ROOT_EXPIRED`. Malformed hashes are rejected
before storage I/O. Authorization preserves each route's concealment policy,
including `404` or filtered results where appropriate.

### 11. Parent Plan Corrections

Every parent-plan reference to an unsigned/base64 root cursor must be rewritten
to the plain `root_hash` selector and response `root` metadata. Specifically:

- cursor TTL configuration becomes an advisory root-expiration reporting bound,
  not a token-lifetime policy;
- GC still guarantees pending-delete grace, later-complete-mark eligibility,
  request pins, and `410` after logical expiration;
- "stateless cursor" becomes "stateless historical-root read";
- physical index state remains forbidden at the API boundary; and
- logical position tokens remain independent pagination helpers.

### 12. Required Verification Matrix

Implementation is not complete until automated and real-world tests cover:

- concurrent HEAD movement after request admission without mixed-root results;
- identical query, search, listing, full fetch, and range fetch content for one
  root, including search-locator-to-range-fetch byte identity;
- current HEAD, snapshot, fork, former HEAD, pending root, eligible root,
  reclaimed root, random hash, internal B-tree page, nested directory root, and
  corrupt admitted root outcomes;
- durable root-provenance recovery, migration, quarantine transitions, and no
  accidental provenance-as-retention behavior;
- live/pending root response fields and headers on every JSON/raw read shape;
- request pins racing later complete mark and sweep without retention refresh;
- historical authorization/config/index/plugin semantics before every
  observable field;
- every legal/illegal pagination combination, arithmetic bound, absent position,
  root/order mismatch, duplicate sort value, and forward/backward boundary;
- no duplicate or omitted results across page, offset, after, and before against
  an authoritative reference evaluator;
- bounded position-token decoding and rejection of physical/planner state;
- coordinated web UI, SDK/client, API, embedded API, and documentation cutover;
  and
- a real `/tmp/codex` database exercised through HTTP and embedded APIs while
  writes advance HEAD, GC transitions an old root to pending, and ranges are
  fetched from both current and retained roots.

---

# Category 4: Configuration Defaults - Round 1

**Status:** Answered. Two configuration-resolution details continue in Round 2.

## Current Implementation Findings

- The lifecycle document currently contains snapshot-write and
  snapshot-retention policy only.
- GC cadence is already independently configurable through cron schedules. It
  should remain scheduling policy, not become reclamation age.
- The current GC has no two-generation pending-delete implementation yet; it
  marks and sweeps in one invocation.
- The lifecycle loader currently returns defaults when the file is missing,
  unreadable, or malformed. It logs a warning and suppresses the error.
- PUT /system/lifecycle replaces the whole document and currently has no schema
  version or explicit semantic validation.

Silently substituting defaults for malformed lifecycle policy is unsafe once
that policy controls deletion. A corrupt configured 30-day grace must not turn
into the 24-hour default and permit earlier reclamation.

## Superseded Cursor Defaults

The plain root-hash contract from Category 3 eliminates:

~~~text
maximum cursor TTL
cursor safety margin
~~~

There is no client-provided expiration and no root-cursor lifetime. A live root
reports an expiration of null. A pending root reports its earliest advisory GC
eligibility boundary from persisted quarantine state and effective grace.

The remaining defaults are:

~~~text
pending-delete grace:       24 hours
required complete marks:    exactly 2 (engine invariant)
GC scan cadence:            independently configured by cron
root expiration reporting:  derived, not configured
~~~

## 1. What exact configuration field and minimum should we expose?

**Recommendation:** Extend the existing versioned lifecycle document:

~~~json
{
  "schema_version": 1,
  "snapshot_writes_enabled": true,
  "snapshot_retention": {
    "auto_months": 0,
    "manual_months": 0
  },
  "garbage_collection": {
    "pending_delete_grace_seconds": 86400
  }
}
~~~

- Use explicit integer seconds in the administrator-facing schema and checked
  milliseconds internally and persistently.
- Missing garbage-collection policy or grace defaults to 86,400 seconds.
- Allow explicit zero for aggressive cleanup. Zero removes the elapsed-time
  requirement but never the later-complete-mark requirement.
- Reject negative, non-integer, overflowing, or unrepresentable values.
- Do not expose required-complete-marks as writable configuration. It is a
  correctness invariant fixed at exactly two: discovery generation plus a later
  complete generation.
- Report the effective value of two through lifecycle API/status, diagnostics,
  metrics, and dashboard.

This is stronger than "not configurable below two": there is no ordinary knob
that can accidentally change it.

Do you approve this schema, 24-hour default, explicit zero-grace support, and
fixed two-mark invariant?

**Your answer: Yes, approved **

## 2. What happens when grace changes for already-pending entries?

Three policies are possible:

1. Fully retroactive: increases and decreases recalculate every pending entry.
2. Frozen: each entry keeps the grace active when it became pending.
3. Safety-asymmetric: increases protect existing entries; decreases affect only
   entries first marked pending after the change.

**Recommendation:** Use safety-asymmetric behavior.

Each candidate stores:

~~~text
pending_since_ms
first_unreachable_generation
grace_at_pending_ms
~~~

Its normal eligibility threshold is:

~~~text
pending_since_ms + max(grace_at_pending_ms, current_configured_grace_ms)
~~~

Consequences:

- increasing grace extends every still-pending candidate immediately;
- decreasing grace never moves an existing candidate earlier;
- candidates discovered after the decrease use the shorter grace;
- intermediate marks reset neither pending time nor stored grace;
- becoming reachable clears the candidate; a later pending transition uses the
  then-current policy; and
- deliberate earlier cleanup uses the approved root-only emergency path: fresh
  complete mark, dry-run report, explicit confirmation, and no same-generation
  discovery/reclaim.

This prevents an accidental config reduction from making millions of old
candidates immediately eligible. A policy increase may move a previously
reported expiration later; a normal decrease never moves it earlier.

Do you approve safety-asymmetric changes, or should reductions apply
retroactively to existing candidates?

**Your answer: Let's go with Option #2 here... we don't need the added complexity. This is a configuration parameter that will _very rarely_ be modified, and the consequences here if modified are _very minor_ to most users.**

## 3. How should missing, malformed, or unreadable config behave?

**Recommendation:** Distinguish absence from failure and fail closed for policy
mutations:

- a genuinely missing file uses documented defaults;
- a valid older schema uses an explicit legacy reader and missing-field defaults;
- malformed JSON, invalid values, unknown versions, unreadable content, and
  corruption return an error rather than defaults;
- ordinary user file reads and writes continue;
- existing snapshot reads, restores, and exports remain available;
- new snapshot creation is rejected because snapshot-write policy is unknown;
- snapshot pruning, quarantine publication, sweep/reclamation, and GC artifact
  retention deletion are disabled;
- diagnostic/dry-run mark may inspect reachability but publishes no candidates
  or deletion state; and
- health, metrics, logs, diagnostics, dashboard, and GET /system/lifecycle show
  a prominent lifecycle-config-invalid state and repair path.

A valid root-only update repairs the file and clears the latch only after
durable read-back validation. The loader must return a result; warning plus
default substitution is removed.

Do you approve this fail-closed boundary?

**Your answer: We _must_ have these values. That isn't optional. I think the correct approach here is _for every config property individually_, if we don't have a value, we provide a default. The attempt to _load_ a config, and failing, should _itself_ log an error... but I don't think it should halt all operations. I say "any config value individually", because it is possible some of these will be provided via other sources (i.e. command line)**

## 4. How should lifecycle configuration be versioned and updated?

**Recommendation:**

- Missing schema-version means legacy v0 and uses an explicit v0 decoder.
- Every new write emits schema-version 1.
- V1 rejects unknown fields and invalid durations instead of ignoring typos.
- PUT /system/lifecycle remains full replacement with missing optional fields
  receiving documented defaults.
- Add root-only PATCH /system/lifecycle for validated partial updates so grace
  can change without resetting snapshot policy.
- GET returns normalized persisted policy plus read-only effective invariants;
  persisted values and defaulted/effective values remain distinguishable.
  <!-- Note from user: Actually, I think this should always return the FULL configuration _as it currently stands_, even if some properties are defaults. -->
- Save uses normal crash-safe file/HEAD publication and durable read-back before
  reporting success.
- Mark traversal is independent of grace. Candidate publication reloads the
  latest valid policy and records it on newly pending candidates.
- Sweep batches hold a policy read guard. A concurrent config update completes
  between batches or invalidates/retries that batch; one reclaim batch never
  mixes two policy decisions.
  <!-- Agreed... which means that config properties need to be stored/copied _in the GC tracking/checkpoint files_... next full GC sweep picks-up and uses newly configured values. -->
- Policy identity uses the validated lifecycle root revision and GC control
  generation rather than a process-local counter.

Do you approve explicit v0/v1 readers, strict v1 validation, full PUT plus
partial PATCH, and generation-safe GC adoption?

**Your answer: Yes, I approve (I especially like the strict schema validation, so things aren't silently failing) **

## Round 1 Proof Obligations

- Missing config produces the 24-hour default; malformed, unreadable, corrupt,
  and unknown-version config never substitutes defaults.
- Golden v0/v1 documents decode consistently across platforms; unknown or
  duplicate fields, bad numeric forms, overflow, and truncation fail.
- Grace zero still requires a later complete generation; one mark can never
  discover and reclaim the same candidate.
- Grace increases extend existing candidates. Grace decreases leave existing
  boundaries unchanged and affect newly pending candidates only.
- Config changes during mark, candidate publication, and every sweep crash point
  never produce mixed-policy or prematurely reclaimed batches.
- Invalid config leaves ordinary reads/writes and existing snapshot reads usable
  while blocking snapshot creation and every mutating retention/GC operation.
- Valid PUT/PATCH repair performs durable read-back before clearing degraded
  state; failed or torn writes preserve the previous effective policy.
- Root expiration metadata matches persisted candidate state and effective grace
  across changes, restart, and conservative recovery.
- Cron cadence changes do not alter pending age; grace changes do not schedule
  GC by themselves.
- API, embedded API, CLI diagnostics, metrics, dashboard, and documentation show
  persisted policy, effective defaults, fixed two-mark invariant, and config
  health consistently.

---

# Category 4: Configuration Defaults - Round 2

**Status:** Answered. Category closed by the final decision record below.

## Reconciled Decisions from Round 1

The following points are now locked:

1. The lifecycle schema gains garbage-collection policy with a 24-hour default
   pending-delete grace.
2. Explicit zero grace is valid, but a later complete mark remains mandatory.
3. Required complete marks is an engine invariant fixed at exactly two and is
   not writable configuration.
4. Cursor TTL and cursor safety-margin settings do not exist.
5. Grace policy is **frozen per candidate** (Round 1 Option 2), not
   safety-asymmetric:

~~~text
eligible_not_before =
  pending_since_ms + grace_at_pending_ms
~~~

   Later configuration increases or decreases do not alter existing pending
   candidates. A candidate that becomes reachable is cleared; if it becomes
   pending again later, it captures the then-current grace.
6. V1 lifecycle configuration is strict and versioned. Missing individual
   properties receive defaults, while present invalid properties and unknown
   fields are errors.
7. PUT remains complete replacement and PATCH performs validated partial
   updates.
8. GET always returns the complete **effective configuration as it currently
   stands**, including values supplied by defaults or higher-precedence sources.
9. A GC run captures the complete effective lifecycle policy in its durable run
   manifest/checkpoints. It uses that immutable copy through resume and
   completion. A configuration change affects the next newly started full GC
   run, not one already in progress.
10. Newly pending candidates record the grace captured by their GC run.
    Existing candidates retain their own recorded grace.

The Round 1 proof statements saying increases/decreases alter existing
candidates are superseded by the frozen-per-candidate decision.

## 1. What is the exact per-property source precedence and API shape?

Your comment is correct: configuration is an effective per-property merge, not
merely one JSON file.

**Recommendation:** Resolve every property independently in this order:

~~~text
built-in default
  < persisted lifecycle.json value
  < environment override (when that property supports one)
  < command-line override
~~~

The highest present valid source wins. Omission at one layer falls through to
the next lower layer; it is not an error. A value explicitly present at any
layer must validate and cannot silently fall through after failing validation.

For example, an omitted grace in lifecycle.json uses 86,400 seconds unless an
environment or command-line override supplies another valid value. A command
line value is process policy and is not written back into the database file.
Each GC checkpoint nevertheless stores the complete effective value it actually
used, plus source/fingerprint metadata, so resume is deterministic.

GET /system/lifecycle should return an envelope such as:

~~~json
{
  "config": {
    "schema_version": 1,
    "snapshot_writes_enabled": true,
    "snapshot_retention": {
      "auto_months": 0,
      "manual_months": 0
    },
    "garbage_collection": {
      "pending_delete_grace_seconds": 86400
    }
  },
  "invariants": {
    "required_complete_marks": 2
  },
  "status": {
    "valid": true,
    "sources": {
      "garbage_collection.pending_delete_grace_seconds": "default"
    }
  }
}
~~~

The main config object is always complete. Source metadata is diagnostic; it
does not make callers reconstruct effective values. PUT/PATCH responses use the
same envelope after durable read-back.

Do you approve this source precedence and full-effective-config envelope?

**Your answer: Agreed/Approved**

## 2. What survives a whole-file load or parse failure?

We were partly talking past each other in Round 1. I did not recommend halting
ordinary database operation. The unresolved question is whether destructive
policy may fall back to a potentially shorter default when a file that
previously contained administrator intent becomes unreadable.

These cases should be distinct:

### Missing Property in a Valid Document

Use the next configured source, then the built-in default. This is normal and
not degraded.

### Present but Invalid Property

Reject PUT/PATCH before publication. Strict validation means an invalid supplied
value never becomes a default.

### Existing Document Becomes Malformed or Unreadable

**Recommendation:** Maintain a small durable last-known-good validated file
policy:

- every valid lifecycle-file publication durably records its canonical policy
  identity before it becomes effective;
- after a load/I/O/parse failure, use that last-known-good file layer plus
  current valid environment/CLI overrides;
- log and expose the file error prominently, but ordinary operations and GC may
  continue under the known policy;
- GET returns the full effective configuration with status degraded and the
  last-known-good source identified;
- repair by valid PUT/PATCH writes and verifies a new policy, then clears the
  degraded state; and
- an active GC run remains deterministic from its checkpointed policy regardless
  of the file failure.

If a malformed/unreadable file exists but there is **no** durable
last-known-good policy (principally a legacy/corrupt migration case), defaults
and valid process overrides still provide a complete displayed configuration.
However, I recommend blocking only destructive lifecycle actions
(snapshot pruning, quarantine publication, and sweep) until a root administrator
repairs or explicitly confirms that fallback policy. Normal file reads/writes,
snapshot reads, and diagnostics continue.

This avoids both bad extremes:

- the database does not become generally unavailable due to one config file;
- a damaged 30-day policy cannot silently become 24 hours and delete data sooner.

The exact last-known-good artifact layout belongs to Category 5, but this
category must freeze the behavior it serves.

Do you approve durable last-known-good fallback, with destructive GC blocked
only when an invalid existing file has no last-known-good policy?

**Your answer: Yes, I approve. And, as one extra step, if you think it warrented, is if we attempt to load the lifecycle policy config, and it fails (corruption/etc...), AND we can not find a fallback (which should be fairly easy, since AEORDB never really deletes/overwrite things), I am okay with us _disabling the GC altogether_. Obviously we throw an error in the logs, and properly warn the user... but I am okay with just disabling the GC (at least the "dangerous" collection phase). We could even show right in the DB UI (for which their is a GC configuration page) that the GC is currently disabled/partially disabled, until the matter is resolved.**

## Round 2 Proof Additions

- Every property resolves through default, file, environment, and CLI layers
  independently with visible source attribution.
- Omitted values default; explicitly invalid values fail rather than falling
  through.
- CLI overrides affect effective GET and newly started runs but do not mutate
  lifecycle.json.
- A resumed GC uses its checkpointed effective policy even when process
  overrides differ after restart.
- Existing pending candidates keep their stored grace across every config
  increase/decrease and restart.
- A malformed file with last-known-good state continues safely under that state
  while reporting degradation.
- A malformed legacy file without last-known-good state leaves normal database
  operation available but cannot prune, publish quarantine, or sweep until
  explicit repair/confirmation.

---

## Configuration Defaults Category: Final Decision Record

**Status:** Closed. No further policy questions remain in this category.

### 1. Lifecycle Schema and Defaults

The lifecycle file remains `/.aeordb-config/lifecycle.json`. New writes use:

~~~json
{
  "schema_version": 1,
  "snapshot_writes_enabled": true,
  "snapshot_retention": {
    "auto_months": 0,
    "manual_months": 0
  },
  "garbage_collection": {
    "pending_delete_grace_seconds": 86400
  }
}
~~~

The initial defaults and invariants are:

~~~text
pending-delete grace:       86,400 seconds (24 hours)
required complete marks:    exactly 2
GC scan cadence:            independent cron configuration
root expiration reporting:  derived from candidate state
~~~

- Missing individual properties receive documented defaults.
- Explicit zero grace is valid. It removes elapsed-time delay but never permits
  discovery and reclaim in one complete mark generation.
- Negative, non-integer, overflowing, and unrepresentable durations are invalid.
- Required complete marks is not writable configuration. V1 fixes it at exactly
  two: one complete discovery generation and a later complete generation.
- The effective invariant is visible through API, diagnostics, metrics, health,
  and dashboard.
- Maximum cursor TTL and cursor safety-margin settings do not exist.

### 2. Per-Property Source Resolution

Every property resolves independently:

~~~text
built-in default
  < persisted lifecycle.json value
  < supported environment override
  < command-line override
~~~

- The highest present valid source wins.
- Omission falls through and is not an error.
- A present invalid value at the winning source fails validation; it never
  silently falls through.
- Failure in a lower-precedence source does not make a property unavailable when
  a valid higher-precedence environment or CLI override supplies it. The lower
  source failure remains visible as degraded configuration health.
- Environment and CLI values are process policy and are not written back into
  lifecycle.json.
- GC checkpoints store the complete effective values, source identity, and
  policy fingerprint used by the run so resume remains deterministic.

### 3. Complete Effective API Response

GET /system/lifecycle always returns the complete effective configuration:

~~~json
{
  "config": {
    "schema_version": 1,
    "snapshot_writes_enabled": true,
    "snapshot_retention": {
      "auto_months": 0,
      "manual_months": 0
    },
    "garbage_collection": {
      "pending_delete_grace_seconds": 86400
    }
  },
  "invariants": {
    "required_complete_marks": 2
  },
  "status": {
    "valid": true,
    "gc_mode": "enabled",
    "sources": {
      "garbage_collection.pending_delete_grace_seconds": "default"
    }
  }
}
~~~

Callers never reconstruct effective policy from a sparse document. Source
metadata accompanies rather than replaces values. PUT/PATCH return this same
effective envelope after durable read-back.

### 4. Versioning and Strict Validation

- Missing schema version selects an explicit legacy v0 reader.
- Every new write emits v1.
- V1 rejects unknown/duplicate fields, invalid types/durations, and malformed
  structure rather than ignoring likely mistakes.
- A valid partial document may omit known optional properties and receive
  defaults.
- PUT /system/lifecycle remains validated full replacement.
- Root-only PATCH /system/lifecycle performs validated partial updates.
- PATCH requires a valid base document or last-known-good file layer. Otherwise
  it instructs the administrator to repair with a complete PUT.
- Publication uses normal crash-safe file/HEAD publication and reports success
  only after durable read-back and semantic validation.
- Minimum-reader capability gating prevents an old binary from ignoring or
  rewriting v1 deletion policy.

### 5. Grace Is Frozen Per Pending Candidate

Each pending candidate stores:

~~~text
pending_since_ms
first_unreachable_generation
grace_at_pending_ms
~~~

Eligibility uses:

~~~text
eligible_not_before_ms = pending_since_ms + grace_at_pending_ms
~~~

- Later grace increases/decreases do not alter existing candidates.
- Newly pending candidates use the policy captured by their GC run.
- Intermediate marks reset neither pending time nor stored grace.
- Becoming reachable clears pending state.
- Re-pending later captures then-current policy and starts a new interval.
- Deliberate earlier cleanup uses the approved root-only emergency workflow,
  never an ordinary config edit.
- Root response expiration derives from the frozen candidate boundary.

### 6. GC Runs Capture Immutable Effective Policy

- A newly started full GC resolves and validates complete effective policy once.
- Its durable manifest/checkpoints copy all effective values, source fingerprint,
  and fixed invariants.
- Resume uses captured policy even if file, environment, or CLI values change.
- A config change affects the next newly started full GC, not a running one.
- Newly pending candidates record the run's grace; existing candidates keep
  their own grace.
- This prevents mixed-policy run and sweep behavior.

### 7. Missing and Invalid Configuration

These cases remain distinct:

- **Missing file:** use defaults plus valid higher-precedence overrides.
- **Missing property in a valid file:** use the next lower source.
- **Present invalid property:** reject the update/file; do not default it.
- **Malformed, unreadable, corrupt, or unknown-version existing file:** enter
  degraded lifecycle state and recover from a validated fallback.

The loader becomes a result-bearing resolver. It no longer logs a warning and
blindly substitutes defaults for all failures.

### 8. Last-Known-Good and Append-History Recovery

Every valid lifecycle-file publication durably records a small canonical
last-known-good policy identity before activation. Category 5 freezes its exact
artifact encoding; behavior is fixed here.

Resolution after current-file failure is per property and operation:

1. an active/resumable GC uses its valid immutable captured run policy;
2. for a new operation, a valid CLI override resolves that property;
3. otherwise a valid environment override resolves that property;
4. otherwise use the durable last-known-good lifecycle file policy;
5. otherwise use the newest prior valid lifecycle revision recoverable from
   AeorDB append-only history/root provenance;
6. if the file is genuinely absent, use the built-in default;
7. if an existing file is invalid and no explicit override or validated
   fallback supplies a destructive property, that property is unresolved.

A lower-precedence file failure never overrides a valid environment/CLI value.
Likewise, one unresolved property does not disable an operation that does not
depend on it. An active/resumed GC keeps its captured immutable policy.

Recovery validates schema, content/checksum identity, write ordering, and
database identity. It never selects an older policy merely because it decodes.
Recovery source and original error remain visible until current policy is
durably repaired.

### 9. Degraded and Disabled GC Modes

If current lifecycle policy is invalid but every property required by an
operation is supplied by valid higher-precedence overrides or a validated
fallback:

- ordinary reads/writes continue;
- that operation may continue or start under its fully resolved policy;
- GET returns the complete effective configuration;
- status reports `gc_mode: degraded_override` or
  `gc_mode: degraded_fallback` as appropriate;
- logs, health, metrics, diagnostics, and GC UI identify the file failure and
  every override/fallback source; and
- valid root-only PUT/PATCH clears degradation after durable validation.

If current policy is invalid, disablement is capability-scoped:

- ordinary file reads/writes continue;
- existing snapshot reads, restores, and exports continue;
- non-destructive values may resolve from defaults/valid process overrides;
- snapshot pruning requires valid effective snapshot-retention properties;
- quarantine publication, sweep/reclamation, and emergency reclaim require a
  valid effective pending-delete grace plus fixed engine invariants;
- GC-artifact retention cleanup requires its own valid effective policy;
- a valid environment/CLI override satisfies its property even when the stored
  file and historical fallbacks are unreadable;
- only capabilities whose required properties remain unresolved are disabled;
- diagnostic reachability, verify, and dry-run work may run but publish no
  deletion state when publication policy is unresolved;
- an active run without valid captured policy cannot advance;
- status reports aggregate mode plus each capability's enabled/disabled state
  and unresolved property names;
- the dashboard GC configuration page shows the disabled state and concrete
  repair instructions prominently; and
- a valid process override may enable an affected capability, while durable
  repair clears stored-policy degradation. Restart alone does not pretend the
  file was repaired.

This is partial maintenance disablement, not a database-wide outage.

### 10. Scheduling Remains Independent

- Cron controls when a new GC task is requested.
- Candidate grace controls its minimum elapsed retention.
- Complete generations control proof sequence.
- Cadence changes do not alter candidate state.
- Grace changes do not schedule GC.
- Disabling scheduled GC does not clear pending candidates.

### 11. Observability

Expose at least:

- persisted schema version and current-file validity;
- complete effective configuration and each property's source;
- fallback identity/age and original load error;
- GC mode: enabled, degraded override, degraded fallback, partially disabled, or
  destructive disabled;
- per-capability policy readiness and unresolved property names;
- valid environment/CLI overrides that keep capabilities enabled despite stored
  policy failure;
- fixed complete-mark invariant;
- active run policy fingerprint/checkpoint source;
- pending counts/bytes by frozen grace or eligibility boundary;
- oldest pending age and next possible reclamation;
- config validation/fallback/repair status; and
- mismatch between current effective policy and active-run policy.

Logs alone are insufficient. Health, diagnostics, task status, API, CLI, and
dashboard GC configuration all surface destructive disablement.

### 12. Required Verification Matrix

Implementation is not complete until tests cover:

- v0/v1 golden documents and strict unknown/duplicate/type/range validation;
- every default/file/environment/CLI precedence combination per property;
- unreadable file with complete, partial, invalid, and absent
  higher-precedence overrides, proving only dependent capabilities are affected;
- full effective GET and PUT/PATCH responses with source attribution;
- missing file/property versus invalid property/document;
- zero grace with mandatory later complete generation;
- frozen grace through changes, restart, reachability clearing, and re-pending;
- policy capture/checkpoint/resume and overrides changing during every run phase;
- torn current policy, last-known-good fallback, append-history fallback,
  corrupt/conflicting fallbacks, and no-fallback behavior;
- no destructive action whose required property remains unresolved through
  scheduled, HTTP, CLI, embedded, emergency, startup, and retention paths;
- continued destructive operation when every required property is supplied by a
  valid higher-precedence override despite invalid stored policy;
- normal reads/writes and snapshot reads while destructive GC is disabled;
- repair success/failure/torn writes and clearing only after read-back;
- root expiration metadata from frozen candidate policy;
- cadence changes independent from grace/candidate state; and
- a real /tmp database with malformed policy, recovered history, active GC
  checkpoint policy, restart, and dashboard/API degraded-state verification.

---

# Category 5: Persistent Artifact Format Freeze

## Round 1: Physical Type Taxonomy and Freeze Boundary

**Status:** Approved. The physical type taxonomy and freeze boundary are
closed.

Yes, we are ready for this category. The code and plan audit found one issue we
must resolve before assigning payload offsets: the original item referred to a
single `IndexArtifact`, but the completed GC decisions also introduced a
physically distinct `GcArtifact`. These two families have different startup,
backup, migration, corruption, and reclamation behavior, so the format freeze
must account for both.

Current on-disk facts:

- `EntryType` currently uses `0x01` through `0x08`.
- The KV type nibble currently uses `0x0` through `0x9`; only `0xA` through
  `0xF` remain without a KV format migration.
- The KV upper nibble already carries `PENDING` and `DELETED` flags. It is not
  spare type space.
- Existing entry framing is version 0. New framing, write sequencing, and
  reserved header space were approved in Category 1 but do not yet have exact
  byte offsets.
- There are currently no checked-in golden binary fixtures for the database
  header, entry header, or artifact payloads. Existing tests exercise selected
  serializers but do not freeze the complete cross-platform bytes.

This category should proceed in four rounds:

1. freeze physical type taxonomy and the common framing boundary;
2. freeze every `IndexArtifact` kind and binary layout;
3. freeze every `GcArtifact` kind and binary layout; and
4. freeze capability bits, limits, malformed-input behavior, and the complete
   golden/fuzz/crash-test corpus.

The first writer remains forbidden until all four rounds are reflected in the
formal plan and independent golden fixtures exist.

## Direct Questions

### 1. Should indexes and GC use separate physical entry/KV types?

**Recommendation:** Yes. Assign permanently:

~~~text
EntryType::IndexArtifact = 0x09
KV_TYPE_INDEX_ARTIFACT   = 0xA

EntryType::GcArtifact    = 0x0A
KV_TYPE_GC_ARTIFACT      = 0xB
~~~

Reserve `EntryType` values from `0x0B` onward and KV tags `0xC` through `0xF`.
Never recycle a historical tag, including currently unusual tags such as
`HEAD` and `VERSION`, even if a future audit finds them unused.

`IndexArtifact` contains only rebuildable derived index state. `GcArtifact`
contains quarantine authority, mark checkpoints/deltas, sweep receipts, and GC
audit/evidence. Keeping the physical types separate lets dirty startup skip
large index payloads, lets migration strip all GC state without decoding every
artifact, and prevents corruption policy from confusing "rebuild this index"
with "disable destructive deletion."

One generic artifact tag would conserve a nibble but force scanners, counters,
backup, repair, and GC to inspect payload prefixes before they can classify an
entry. Three or more new physical tags would make classification finer but
would spend half or more of the remaining KV namespace immediately. Two is the
best balance.

Do you approve these two physical types and exact numeric assignments?

**Your answer: Yes**

### 2. Should low-volume engine controls consume another physical type?

The plan also mentions lifecycle last-known-good identity, root provenance,
repair tickets/latches, task records, and migration leases. Not all persistent
schemas deserve a new WAL/KV type.

**Recommendation:** Do not add a generic `ControlArtifact` type in v1. Keep
low-volume named controls as strictly versioned root-only system records using
existing system-record storage. Give each payload its own magic, schema, bounds,
checksum, and canonical key. Put a control under `GcArtifact` only when it is
part of GC authority/history, and under `IndexArtifact` only when it is
rebuildable index-generation state. This preserves four KV tags for genuinely
new physical behavior.

If a later control family needs distinct scanner, backup, GC, or retention
semantics, it can spend a reserved tag in a later capability-gated format.

Do you approve reserving physical types for behaviorally distinct/high-volume
families rather than allocating one for every persisted struct?

**Your answer: Yes**

### 3. What exactly is inside this format freeze?

**Recommendation:** Treat the following as one compatibility unit:

- legacy `DatabaseHeader` v3 bytes, preserved exactly;
- new `DatabaseHeader` v4 A/B slot bytes and capability fields;
- legacy `EntryHeader`/whole-entity v0 bytes, preserved exactly;
- new whole-entity v1 common prefix/header, including exact offsets, allocated
  header length, CRC, write sequence, and 32 reserved bytes;
- every `IndexArtifact` v1 envelope and kind body;
- every `GcArtifact` v1 envelope and kind body;
- each new root-only system-control schema referenced by these plans;
- canonical key preimages and domain separators;
- fixed-point scalar, identity, optional-field, bitmap, string/byte-vector, and
  length-prefix encodings;
- capability-bit assignments, hard decoder limits, checksum/hash algorithms,
  and error/degradation behavior; and
- fixture provenance and exact expected bytes on Linux, macOS, and Windows.

No implementation may serialize `usize`, native-endian numbers, Rust enum
layout, `bincode`, or an unstable/default `serde` representation. Existing v0
bytes are compatibility fixtures, not reformatted to resemble v1.

Do you approve this freeze boundary, including the database/entity framing and
small control schemas rather than only the large page payloads?

**Your answer: Yes**

### 4. How should unknown or malformed artifacts fail?

**Recommendation:** Use family-specific fail-closed behavior:

- malformed common framing is structural WAL corruption; scanning may resync
  only through the existing bounded, evidence-reporting recovery rules;
- an unknown required database capability rejects open before any writer runs;
- an unknown entity version/type is never guessed or interpreted as the newest
  known layout;
- a malformed/unknown `IndexArtifact` degrades only the affected index
  generation, preserves evidence, and uses an authoritative fallback or an
  explicit incomplete-index error;
- a malformed/unknown `GcArtifact` preserves evidence and disables destructive
  GC until a fresh valid mark state is published; it can never make an entity
  eligible for deletion;
- malformed low-volume policy/control state invokes that control's already
  approved last-known-good or capability-scoped disablement policy; and
- all decoders validate lengths, counts, arithmetic, recursion/depth, canonical
  ordering, reserved-zero bytes, checksums, identity/key agreement, and trailing
  bytes before allocating or accepting a record.

Unknown future artifact kinds are accepted only by binaries advertising the
corresponding capability. A known capability with an unknown kind is corruption,
not an invitation to skip correctness-bearing state.

Do you approve this malformed/unknown-data policy as the rule the exact codecs
must implement?

**Your answer: Yes**

### 5. How should golden fixtures be created before the first writer?

**Recommendation:** Check in hand-audited, spec-derived binary fixtures and a
machine-readable manifest before implementing production serialization. Each
fixture records its schema/type/kind, semantic field values, exact byte length,
hex dump, expected outer identity hash, expected CRC, and expected decode
result. Corrupt fixtures cover each individual validation boundary.

The implementation order is:

1. freeze the byte tables in the plan;
2. create fixture bytes independently from the production writer and review the
   offsets/checksums by hand plus a tiny standalone verifier;
3. implement bounded readers against those fixtures;
4. implement writers that must reproduce the fixture bytes exactly;
5. run the same fixtures on Linux, macOS, and Windows CI; and
6. add property tests, mutation tests, fuzz targets, and crash/publication tests
   without replacing the golden tests.

Tests generated by the serializer under test are not golden tests. Legacy
fixtures should include bytes extracted from verified real v3/v0 databases as
well as synthetic boundary cases.

Do you approve this fixture-first process and require all supported platforms
to produce identical bytes before enabling an artifact writer?

**Your answer: Yes**

## Round 1 Test Protocol

The hypothesis is that every supported platform and every engine entry point
interprets the same valid bytes identically, rejects the same invalid bytes
before unsafe allocation, and applies the correct family-specific degradation
policy.

Minimum proof cases for this round are:

- given every existing and newly assigned type/tag, encode/decode and KV
  rebuild preserve the exact permanent numeric mapping and flags;
- given an unknown type, version, capability, artifact kind, or nonzero reserved
  byte, open/read/verify/repair fail at the documented boundary without writing;
- given truncated, overflowing, checksum-invalid, identity-mismatched,
  noncanonical, duplicated, or trailing bytes, every reader rejects the same
  fixture with a stable error class;
- given corrupt derived-index state, queries never report partial results as
  complete; given corrupt GC state, sweep never executes;
- given v3/v0 legacy fixtures, the new binary reads them byte-for-byte without
  rewriting them merely because they were opened; and
- given Linux, macOS, and Windows builds, fixture serialization and all hash/CRC
  values are byte-identical.

Unit tests cover each field and boundary, fixture integration tests cover whole
records and mixed databases, property/fuzz tests cover parser state space, and
crash-injection tests cover A/B and pointer-last publication. A real `/tmp`
database test remains required once writers exist, but no live test can replace
the checked-in binary contract.

---

## Round 2: Common Entity Framing and Database Header V4

**Status:** Answered. Question 2 was corrected and approved in Round 2A.

### Decisions Locked From Round 1

1. `EntryType::IndexArtifact = 0x09` and
   `KV_TYPE_INDEX_ARTIFACT = 0xA`.
2. `EntryType::GcArtifact = 0x0A` and `KV_TYPE_GC_ARTIFACT = 0xB`.
3. KV tags `0xC` through `0xF` remain reserved and historical tags are never
   recycled.
4. Low-volume controls remain strictly versioned system records unless their
   physical behavior later justifies a capability-gated entry type.
5. The freeze includes database/entity framing, both artifact families,
   control schemas, key preimages, algorithms, limits, failure behavior, and
   cross-platform fixtures.
6. Malformed state fails closed according to its family: index corruption
   degrades index use; GC corruption disables destructive deletion.
7. Golden fixture bytes precede production readers and writers.

### Audit Correction: V3 and V4 Slots Cannot Be Mixed In Place

The older refactor plan proposed writing one 512-byte v4 slot and then replacing
the remaining 256-byte v3 slot in the same database. That is physically
impossible without relocating the data region first:

~~~text
v3 slot A:   0..255
v3 slot B: 256..511
v3 data:    512..

v4 slot A:   0..511
v4 slot B: 512..1023  <-- overlaps v3 data
v4 data:   1024..
~~~

This is not a minor implementation detail. Attempting the proposed mixed-slot
transition would overwrite the first 512 bytes of live database data.

## Direct Questions

### 1. Should v4 exist only in the side-by-side migration destination?

**Recommendation:** Yes. Replace the older mixed v3/v4 transition with:

1. A compatibility release can open v3 and v4 as two distinct complete
   layouts, but never treats differently sized slots as one A/B pair.
2. The online source remains an untouched v3 database while a separate v4
   destination is created with two 512-byte slots and a 1,024-byte data start.
3. The destination writes and verifies both v4 slots before copying entities.
4. The migration converts live entities to the approved v1 framing, reserves
   write-sequence ranges, and captures concurrent source mutations.
5. Required capability bits are durably present in both v4 slots before the
   destination may append `IndexArtifact` or `GcArtifact` entries.
6. Final delta drain, validation, and cutover use the already-approved short
   exclusive write window.
7. Rollback uses the retained v3 source. There is no partially shifted file and
   no ordinary in-place v4 downgrade.

The migration may support an offline clone mode for smaller databases, but it
uses the same v3-reader/v4-writer pipeline. Header repair must never "upgrade"
v3 by shifting a production database in place.

Do you approve removing the mixed in-place transition and making the
side-by-side clone the only v3-to-v4 format migration?

**Your answer: Yes**

### 2. Do you approve this exact whole-entity v1 header?

Every multibyte integer is little-endian. `H` is the selected database hash
width, currently 32 bytes for Blake3 and at most 64 bytes for the enumerated
algorithms.

| Offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic` | Existing `0x0AE012DB` |
| 4 | 1 | `entity_version` | Exactly `1` for this codec |
| 5 | 1 | `entry_type` | Permanent `EntryType` byte |
| 6 | 2 | `allocated_header_length` | Exactly `77 + H` in v1 |
| 8 | 1 | `flags` | Only defined v1 bits may be set |
| 9 | 2 | `hash_algorithm` | Existing permanent algorithm ID |
| 11 | 1 | `compression_algorithm` | Existing permanent codec ID |
| 12 | 1 | `encryption_algorithm` | `0` in v1 |
| 13 | 4 | `key_length` | Checked `u32` |
| 17 | 4 | `value_length` | Stored-value bytes, checked `u32` |
| 21 | 8 | `timestamp_ms` | Nonnegative Unix epoch `u64` |
| 29 | 4 | `total_length` | Header + key + stored value exactly |
| 33 | 8 | `write_sequence` | Nonzero, globally reserved `u64` |
| 41 | H | `integrity_hash` | Defined in Question 3 |
| `41 + H` | 32 | `reserved` | All zero in v1 |
| `73 + H` | 4 | `header_crc32` | Last four header bytes |
| `77 + H` | key length | key | Exact bounded key bytes |
| after key | value length | stored value | May be streamed |

This gives a 109-byte v1 header for Blake3-256 and a 141-byte header for a
future 512-bit database hash. The first eight bytes are the canonical prefix:
they identify type/version and tell a bounded reader exactly how many header
bytes to fetch. The payload begins at `allocated_header_length`, not at a
process-global constant.

V1 retains 32-bit entity lengths deliberately. Chunks and artifact pages are
already bounded far below 1 GiB, while user files are represented by many
chunks rather than one multi-gigabyte WAL entity. Supporting a larger single
entity would increase corruption/allocation risk without helping large files.

A later entity version may consume its 32 reserved bytes while retaining the
same allocated header length. If it needs more, it must use the approved
copy-on-write relocation path.

Do you approve this field order, widths, and exact v1 header sizes?

**Your answer: I like the header length, but if we read _only_ a header length, then we would then have to read the whole header, to finally understand the length of the entity (1x read header prefix, 2x read header, 3x read read entity). Shouldn't we have a header length, plus a full entity length? Or maybe even just an entity length (the header length should be known by the codec)?**

### 3. Do you approve these exact integrity algorithms and coverage domains?

**Recommendation:** Use two deliberately different checks:

#### Header CRC

`header_crc32` is CRC-32/ISO-HDLC, matching the current `crc32fast` behavior:

~~~text
polynomial: 0x04C11DB7 (reflected 0xEDB88320)
init:       0xFFFFFFFF
reflect:    input and output
xorout:     0xFFFFFFFF
coverage:   header bytes [0, allocated_header_length - 4)
encoding:   resulting u32 little-endian
~~~

It covers the integrity hash and all reserved bytes. Its purpose is quick torn
or malformed header detection before trusting lengths or allocating payload
memory.

#### Entry Integrity Hash

The `integrity_hash` uses the database's selected `HashAlgorithm` over this
unambiguous byte sequence:

~~~text
ASCII "aeordb-entry-v1\0"
entity_version             u8
entry_type                 u8
flags                      u8
hash_algorithm             u16 LE
compression_algorithm      u8
encryption_algorithm       u8
key_length                 u32 LE
value_length               u32 LE
key                        exact key_length bytes
stored_value               exact value_length bytes
~~~

The hash excludes physical-incarnation metadata (`timestamp_ms`,
`write_sequence`, header/total lengths, reserved bytes, and CRC). Therefore a
verified relocation of the same stored representation has the same integrity
hash while the higher write sequence selects the newer physical incarnation.
Changing type, flags, compression, encryption, key, or stored bytes changes the
hash. Verification happens before decompression/decryption.

V0 keeps its exact historical hash behavior. Artifact-level identity hashes and
payload checksums will be frozen in their own rounds; they do not silently
replace either common-header check.

Do you approve these algorithms and exact coverage domains?

**Your answer: Sure!**

### 4. Do you approve this exact 512-byte DatabaseHeader v4 slot?

Each database begins with two independent 512-byte slots. All multibyte values
are little-endian. Hash fields are fixed 64-byte slots; only the width selected
by `hash_algorithm` is meaningful and every unused tail byte must be zero.

| Offset | Size | Field | V4 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic` | ASCII `AEOR` |
| 4 | 1 | `header_version` | Exactly `4` |
| 5 | 2 | `slot_length` | Exactly `512` |
| 7 | 1 | `header_flags` | Zero until assigned by a later version |
| 8 | 2 | `hash_algorithm` | Permanent algorithm ID |
| 10 | 8 | `slot_sequence` | Monotonic A/B publication sequence |
| 18 | 8 | `created_at_ms` | Nonnegative Unix epoch `u64` |
| 26 | 8 | `updated_at_ms` | Nonnegative Unix epoch `u64` |
| 34 | 16 | `database_id` | Random stable 128-bit ID; all-zero invalid |
| 50 | 8 | `write_sequence_high_water` | Durable reserved-range upper bound |
| 58 | 32 | `required_reader_capabilities` | Fixed 256-bit little-indexed bitset |
| 90 | 8 | `kv_block_offset` | Checked file offset |
| 98 | 8 | `kv_block_length` | Checked byte length |
| 106 | 1 | `kv_block_version` | Explicit supported version |
| 107 | 1 | `kv_block_stage` | Checked stage ID |
| 108 | 1 | `resize_in_progress` | Canonical boolean 0 or 1 |
| 109 | 1 | `resize_target_stage` | Checked stage ID |
| 110 | 8 | `nvt_offset` | Checked file offset |
| 118 | 8 | `nvt_length` | Checked byte length |
| 126 | 1 | `nvt_version` | Explicit supported version |
| 127 | 1 | `backup_type` | Existing checked enum ID |
| 128 | 8 | `hot_tail_offset` | Checked file offset |
| 136 | 8 | `buffer_kvs_offset` | Checked or zero |
| 144 | 8 | `buffer_nvt_offset` | Checked or zero |
| 152 | 8 | `entry_count` | Diagnostic/reconciled count |
| 160 | 64 | `head_hash` | Selected-width bytes, then zero padding |
| 224 | 64 | `base_hash` | Selected-width bytes, then zero padding |
| 288 | 64 | `target_hash` | Selected-width bytes, then zero padding |
| 352 | 32 | `required_writer_capabilities` | Fixed 256-bit little-indexed bitset |
| 384 | 2 | `system_family_registry_version` | Permanent nonzero registry schema |
| 386 | 6 | reserved | Zero |
| 392 | 64 | `system_family_registry_fingerprint` | Selected-width hash, zero-padded |
| 456 | 8 | `writer_fence_epoch` | Nonzero monotonic local writer epoch |
| 464 | 44 | `reserved` | All zero in v4 |
| 508 | 4 | `slot_crc32` | CRC-32/ISO-HDLC over bytes 0..507 |

Each 256-bit capability field is a fixed byte array, not a platform integer.
Bit zero is the least-significant bit of byte zero. Exact assignments and
reader/writer admission rules are frozen in Round 14. The registry fingerprint
uses the database hash algorithm and zeroes the unused tail of its 64-byte slot.

Readers validate CRC before interpreting offsets. They then validate all
booleans/enums/reserved bytes, checked offset-plus-length arithmetic, file
bounds, region overlap, hash padding, capability support, and cross-field state
invariants. A/B selection is:

- one valid slot: use it and report redundancy degraded;
- two valid slots with different sequences: use the higher sequence;
- equal sequence and byte-identical slots: use either;
- equal sequence with different semantic bytes: ambiguous corruption, refuse
  writable open and require diagnostics/repair; and
- no valid slot: refuse open and require repair.

Ordinary runtime may set additional required capability bits but never clears
them. Clearing capabilities is an explicit offline format-downgrade operation
that first proves no dependent bytes remain.

Do you approve this v4 layout, including fixed 64-byte hash slots and the
156-byte extension region whose final v4 capability/registry allocation is
frozen in Round 14?

**Your answer: Yes**

### 5. Do you approve these common decoder and allocation invariants?

**Recommendation:** Freeze the following before family-specific limits:

- `allocated_header_length` must equal the exact length defined by the selected
  `(EntryType, EntityVersion)` codec and may never exceed 4,096 bytes.
- V1 `key_length` and `value_length` each retain the current hard ceiling of
  1 GiB, but each entity family applies a much smaller limit before allocation.
- Arithmetic uses checked widening to `u64`; exact equality with
  `total_length` is required. Saturating arithmetic is not format validation.
- `total_length` must fit `u32`, remain within the containing WAL/file region,
  and be at least the exact header length.
- V1 `write_sequence` and timestamps must be valid; zero sequence, sequence
  above the durable high-water mark, or timestamp arithmetic overflow is
  corruption.
- Unknown flag bits, noncanonical booleans/enums, nonzero v1 reserved bytes,
  invalid hash padding, and trailing bytes are rejected.
- A decoder reads the fixed prefix and exact header first, validates all
  structural bounds, then reads a bounded key and family envelope. It never
  allocates from an unvalidated length.
- Full values need not be allocated. Hash verification, copying, backup,
  migration, and large-value reads support bounded streaming.
- Header-only scans may omit payload integrity verification only when their
  caller explicitly accepts that weaker result. They still validate framing,
  header CRC, file bounds, type/version, and sequence range.
- Error classes distinguish unsupported format/capability, malformed input,
  checksum mismatch, identity mismatch, truncation, and I/O failure. Callers do
  not parse error strings to determine policy.

The 1 GiB generic ceiling is not permission for a 1 GiB artifact. Round 3 and
Round 4 will assign strict per-kind maxima and count/depth limits.

Do you approve these common validation and streaming invariants?

**Your answer: Yes**

## Round 2 Proof Obligations

- Hand-authored Blake3 and 512-bit-width header fixtures prove the exact 109-
  and 141-byte v1 layouts, CRC vectors, integrity-hash domains, and payload
  starts.
- V3 fixtures remain byte-identical and dispatch only to the v3 reader; v4
  fixtures dispatch only to the v4 reader.
- A migration fixture proves no v4 slot is ever written over a v3 file and that
  the destination data starts at byte 1,024.
- Every single v1/v4 field, reserved byte, length boundary, hash-padding byte,
  selector, capability byte, CRC byte, and integrity-hash byte is independently
  mutated and rejected with the expected typed error.
- Sequence tests cover range reservation, skipped ranges, restart, exhaustion,
  torn slots, equal-slot ambiguity, and a complete valid newer incarnation with
  an incomplete KV publication.
- Streaming tests prove exact boundaries across short reads, EOF, interrupted
  reads, adjacent entities, bounded read-ahead, and values near family limits.
- Cross-platform tests compare fixture bytes and hashes, not merely decoded
  semantic values.

---

## Round 2A: Put Total Entity Length In The Canonical Prefix

**Status:** Approved. Common entity and database framing are closed.

Your I/O concern is correct. My proposed field order needlessly delayed
`total_length` until byte 29. A generic sequential reader would first discover
the header length, then fetch the header to discover the entity length, and
only then know the complete read boundary.

We should keep **both** lengths but place both in the prefix. This adds no bytes:
`total_length` already existed and is only moving. Keeping
`allocated_header_length` remains useful because it:

- gives the exact payload start rather than forcing generic code to duplicate
  every version's header-size calculation;
- detects disagreement between the codec and persisted framing;
- permits a later entity version to consume some reserved bytes while retaining
  its original payload offset; and
- lets bounded recovery tooling describe/skip framing even before it decodes
  all version-specific fields.

The corrected 12-byte canonical prefix is:

| Offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic` | Existing `0x0AE012DB` |
| 4 | 1 | `entity_version` | Exactly `1` for this codec |
| 5 | 1 | `entry_type` | Permanent `EntryType` byte |
| 6 | 2 | `allocated_header_length` | Exactly `77 + H` in v1 |
| 8 | 4 | `total_length` | Header + key + stored value exactly |

The remaining fields become:

| Offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 12 | 1 | `flags` | Only defined v1 bits may be set |
| 13 | 2 | `hash_algorithm` | Existing permanent algorithm ID |
| 15 | 1 | `compression_algorithm` | Existing permanent codec ID |
| 16 | 1 | `encryption_algorithm` | `0` in v1 |
| 17 | 4 | `key_length` | Checked `u32` |
| 21 | 4 | `value_length` | Stored-value bytes, checked `u32` |
| 25 | 8 | `timestamp_ms` | Nonnegative Unix epoch `u64` |
| 33 | 8 | `write_sequence` | Nonzero, globally reserved `u64` |
| 41 | H | `integrity_hash` | Already approved coverage domain |
| `41 + H` | 32 | `reserved` | All zero in v1 |
| `73 + H` | 4 | `header_crc32` | Last four header bytes |
| `77 + H` | key length | key | Exact bounded key bytes |
| after key | value length | stored value | May be streamed |

The header remains exactly 109 bytes for Blake3-256 and 141 bytes for a 512-bit
hash. The approved integrity-hash domain and CRC coverage do not change.

Read behavior is now:

- A KV lookup already has the expected total length and issues one exact read,
  then requires the prefix to agree.
- A sequential reader reads 12 bytes, validates both lengths against hard/file
  bounds, then reads exactly the remaining entity bytes into the same bounded
  buffer. It does not fetch a separate header and then re-read the entity.
- A streaming reader reads the 12-byte prefix and remaining header, validates
  its CRC and family limits, then streams key/value bytes without allocating or
  re-reading the complete entity.
- Bounded read-ahead may satisfy the prefix, header, and adjacent payload from
  one physical I/O while still handing each codec an exact slice.

This gives the generic scanner both boundaries immediately while preserving the
future header reserve.

Do you approve this corrected 12-byte prefix and revised offsets?

**Your answer: Yes, approved**

## Round 2A Proof Additions

- Fixture tests prove `total_length` is available from the first 12 bytes and
  that all later offsets remain byte-exact for both hash widths.
- Instrumented read tests prove a buffered sequential entity needs at most one
  prefix read plus one remainder read, with no duplicate full-header/entity
  fetch.
- KV reads reject disagreement between indexed length, prefix total length,
  header/key/value arithmetic, and actual available bytes.
- Streaming tests prove prefix/header bytes are retained rather than re-read and
  that payload chunks never cross the declared entity boundary.

---

## Round 3: IndexArtifact Taxonomy, Envelope, and Addressing

**Status:** Closed. The endianness and snapshot/replication clarifications were
approved in Rounds 3A and 3B.

The producer/consumer audit found that the original twelve-kind enum cannot
implement its own later plan. It omitted scope and value manifests plus durable
journal segments, while incorrectly putting `MigrationLease` in the artifact
family even though the plan says existing system task records are the mutable
control plane.

This round freezes the common artifact contract. The next index rounds will
freeze exact pointer/manifest bodies, followed by directory/page/journal bodies.
Trying to put every page entry and manifest field into this round would make it
too easy to approve inconsistent byte tables as one large blob.

### Complete Territory Map

`IndexArtifact` producers are:

- the unified indexing mutation pipeline for store, blob commit, embedded
  batch, merge, copy, rename, restore, delete, and reindex;
- active/frozen memtable flush and emergency spill;
- page split, page merge, and bounded compaction;
- shared scope-catalog allocation/tombstoning and canonical-value publication;
- NVT hint healing, resizing, and optional rebuild;
- authoritative v1 build and v0-to-v1 migration;
- mutation catch-up journals and bounded Merkle reconciliation;
- index repair/rebuild and optional compatible-index restore; and
- pointer-last generation publication.

Its consumers are:

- query planning, point/range/fuzzy scans, boolean composition, sorting,
  aggregation, pagination, and authoritative recheck;
- `IndexManager` registry/format resolution and visibility reconciliation;
- startup pointer/manifest validation and lazy page loading;
- page/value/catalog/NVT caches, prefetch, and physical-read coalescing;
- migration resume, shadow validation, rollback, and finalization;
- request/task generation pins and derived-artifact reachability;
- GC marking, retirement, quarantine, and Void reuse/compaction;
- physical copy and optional validated index export/restore;
- verify, repair, scrub, corruption evidence, and rebuild tooling;
- KV dirty rebuild, counters, metrics, health, dashboard, and diagnostics; and
- graceful shutdown/cancellation checkpointing.

Missing any one of these consumers could make a valid artifact invisible,
immortal, prematurely reclaimable, falsely current, or eagerly loaded at
startup.

## Direct Questions

### 1. Do you approve this corrected permanent kind registry?

**Recommendation:** Use a `u16` kind field with grouped permanent IDs:

~~~text
0x0000  Invalid / never emitted

0x0001  FieldIndexActivePointer
0x0002  FieldNvtActivePointer
0x0003  ScopeCatalogActivePointer

0x0010  FieldIndexManifest
0x0011  FieldNvtManifest
0x0012  ScopeCatalogManifest
0x0013  ValueStoreManifest

0x0020  ArtifactDirectoryNode

0x0030  PostingPage
0x0031  ValuePage
0x0032  NvtTile
0x0033  ScopeCatalogPage
0x0034  DocumentStatePage

0x0040  MutationJournalSegment
0x0041  IndexTaskCheckpoint
~~~

All unassigned values remain reserved and are never reused after publication.
The grouped gaps are intentional and cost no disk space.

Corrections from the original enum:

- `Manifest` becomes the explicit `FieldIndexManifest`.
- `NvtManifest` becomes `FieldNvtManifest`.
- `ScopeCatalogManifest` is added because a scope active pointer needs a
  generation root containing both catalog directions and the next ordinal.
- `ValueStoreManifest` is added because field manifests already reference one,
  and value pages need their own immutable directory root/counts.
- `MutationJournalSegment` is added for the bounded migration/reconciliation
  and emergency-spill journals required later in the plan.
- `MigrationCheckpoint` becomes `IndexTaskCheckpoint` so the same immutable,
  bounded format can checkpoint migration, authoritative rebuild,
  reconciliation, and compaction without inventing nearly identical kinds.
- `MigrationLease` is removed. Leases, task status, registry/configuration, and
  mutable repair latches remain low-volume versioned system records. Those
  records point to immutable checkpoint/journal hashes and make them GC roots.
- Per-document extraction/strategy `unindexable` state is represented in
  bounded `DocumentStatePage` artifacts independently owned by a ValueStoreId
  or IndexId. Round 6 corrected and approved this omission before any writer
  existed.
- Persistent document-membership data lives in scope catalog pages; temporary
  query bitmaps are not persisted artifacts.

Do you approve these exact kind IDs and corrections?

**Your answer: Yes**

### 2. Do you approve this exact common IndexArtifact v1 envelope?

The `IndexArtifact` entity key is exactly `H` bytes. Its stored value begins
with this canonical envelope; every multibyte integer is little-endian:

<!-- User concern: Are all data points already little-endian? I don't actually know the answer, so this might warrent a double-check.-->

| Offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic` | ASCII `AIDX` |
| 4 | 2 | `artifact_schema_version` | Exactly `1` |
| 6 | 2 | `artifact_kind` | Permanent kind ID from Question 1 |
| 8 | 2 | `envelope_header_length` | Exactly `32` |
| 10 | 2 | `artifact_flags` | Zero in v1 |
| 12 | 4 | `artifact_value_length` | Envelope + identity + body + CRC exactly |
| 16 | 2 | `identity_length` | Exact kind-specific identity bytes |
| 18 | 2 | `reserved` | Zero in v1 |
| 20 | 4 | `body_length` | Exact kind-specific body bytes |
| 24 | 8 | `generation` | Nonzero kind-specific generation |
| 32 | identity length | `identity` | Canonical identity defined per kind |
| after identity | body length | `body` | Canonical kind body |
| final 4 | 4 | `artifact_crc32` | CRC-32/ISO-HDLC over all prior value bytes |

The required equality is:

~~~text
artifact_value_length = 32 + identity_length + body_length + 4
EntryHeader.value_length = artifact_value_length
~~~

`generation` has one consistent role: it names the immutable state generation
to which the artifact belongs. For active pointers it is the selected target
generation; for NVT it is the independent hint generation; for task artifacts
it is the task/run generation. Generation zero is reserved as "none" and is
never serialized as an artifact.

The common envelope intentionally does not repeat owner IDs, page IDs, counts,
or compression selectors. Those belong to exact kind identities/bodies. The
outer v1 `EntryHeader` remains authoritative for entity framing and requires
`compression_algorithm = none` and `encryption_algorithm = 0` for every v1
IndexArtifact.

Do you approve this 32-byte envelope, trailing CRC, and generation rule?

**Your answer: Yes**

### 3. Do you approve content keys for immutable artifacts and stable keys only for A/B pointers?

**Recommendation:** Make the three active-pointer kinds the only mutable
logical IndexArtifact keys.

For an immutable artifact, its KV/entity key is:

~~~text
H(
  ASCII "aeordb.index-artifact.immutable.v1\0" ||
  artifact_kind u16 LE ||
  complete artifact stored value, including trailing CRC
)
~~~

This key is also the `ArtifactHash` stored in manifests, directories,
checkpoints, and system task records. Because the value contains its canonical
identity, generation, and body, recomputing the key proves key/payload identity.

For an active A/B pointer, the stable KV/entity key is:

~~~text
H(
  ASCII "aeordb.index-artifact.pointer.v1\0" ||
  artifact_kind u16 LE ||
  canonical pointer identity
)
~~~

Pointer identity is exactly `owner_id || slot`, where `owner_id` is `IndexId`
for field/NVT pointers or `ScopeId` for scope pointers, and `slot` is one byte
(`0` for A, `1` for B). The payload repeats owner and slot. Its logical pointer
sequence lives in the body; the outer entity write sequence still orders
physical revisions of that stable key.

Readers always recompute the applicable key and compare it to the surrounding
entity/KV key. Immutable keys can never be overwritten. Pointer publication
rewrites only the older logical slot, writes dependencies first, verifies the
new pointer and closure, and keeps the other slot as fallback.

No generation/page/tile has a stable mutable key, and no textual identity is
passed directly into the fixed-width KV.

Do you approve these exact key domains and the three-pointer-only mutability
rule?

**Your answer: How does this impact snapshots, and replication?**

### 4. Do you approve these canonical primitive widths and encodings?

**Recommendation:** Freeze one shared artifact codec vocabulary:

- `ArtifactHash`, `IndexId`, `ScopeId`, `ValueStoreId`,
  `ConverterFingerprint`, `FileKey`, `RecordRevisionHash`, `MutationId`, and
  `SourceVersion.head_hash` are exactly the database hash width `H`.
- `TaskId` and `RuntimeVisibilityToken.boot_id` are exactly 16 raw bytes.
- generation, pointer sequence, page/tile ID, document ordinal, NVT
  resolution, normalized scalar, and counts that may exceed four billion are
  `u64` little-endian.
- value ordinal, bounded per-page counts, and byte/count prefixes are `u32`
  little-endian unless a kind table explicitly gives a smaller width.
- enum/discriminant widths are explicitly `u8` or `u16`; Rust enum layout is
  never serialized.
- booleans and options use a one-byte canonical tag: only `0` and `1` are
  accepted, and an absent option emits no value bytes.
- byte strings and metadata strings use a `u32` byte length followed by exact
  bytes. Metadata strings are valid UTF-8, NFC-normalized, and contain no NUL;
  normalized paths must also pass AeorDB's canonical absolute-path validator.
- converter-produced canonical field values remain opaque bytes and are not
  Unicode-normalized again by the artifact codec.
- vectors use a checked `u32` item count followed by canonical items. Maps and
  sets are strictly sorted by their canonical encoded key and reject duplicate
  or out-of-order entries.
- bitmaps declare logical bit count and byte count; unused high bits in the
  final byte are zero.
- fixed-point normalized coordinates are `u64`; persisted floating-point
  values, `usize`, native-endian values, implicit padding, and platform enums
  are forbidden.
- all reserved bytes are zero and all bodies consume their declared length
  exactly.

This makes definition IDs and artifact bytes dependent on the database hash
algorithm, which matches the existing engine and optional compatible-artifact
transfer contract. A database hash-algorithm migration is therefore a complete
side-by-side format migration, not an in-place reinterpretation.

Do you approve these canonical primitives?

**Your answer: I approve, still pending the endianness concern**

### 5. Do you approve these common hard bounds and codec rules?

**Recommendation:** Apply these family-wide limits before tighter per-kind
limits in the next rounds:

~~~text
IndexArtifact key length:       exactly H
envelope header length:         exactly 32 bytes in v1
identity length:                1..4,096 bytes
complete stored value:          at most 64 MiB
body length:                    at most value cap minus envelope/identity/CRC
artifact generation:            1..u64::MAX
outer entry compression:        none in v1
outer entry encryption:         none in v1
decoder nesting/recursion:      at most 16 levels
~~~

Pages should normally target roughly 64 KiB; 64 MiB is an absolute corruption
and allocation ceiling, not a production target. Oversized checkpoint/journal
state must segment rather than request a larger artifact. Pointer and manifest
caps will be much smaller.

The decoder validates common lengths, arithmetic, key width, envelope version,
kind, flags/reserved bytes, generation, trailing CRC, and outer/header agreement
before dispatching a kind body. Kind decoders then validate their exact identity
length, body cap, counts, ordering, references, and semantic invariants before
allocation.

Ordinary dirty startup validates only the outer EntityHeader framing/CRC,
fixed-width key, IndexArtifact envelope header, and file bounds; it records the
KV mapping without reading large bodies or trailing CRCs. Full artifact key,
body, and CRC verification occurs on first use, explicit verify/repair,
migration validation, backup-with-indexes, and sampled scrub. An active pointer
and its shallow manifest closure are always fully verified before an index is
advertised usable.

Do you approve these common bounds and lazy-verification boundary?

**Your answer: Yes**

## Round 3 Proof Obligations

- A permanent registry test proves every kind ID is unique, historical IDs
  cannot be reassigned, and unknown IDs fail as unsupported/corrupt according to
  the database capability state.
- Golden fixtures cover every kind's minimum envelope for both hash widths,
  exact CRC and key vectors, pointer A/B keys, and immutable key derivation.
- Mutation/property tests change each envelope byte, length, count, generation,
  flag, reserved byte, identity byte, body byte, CRC byte, and outer key and
  prove deterministic typed rejection.
- Crash tests interrupt every dependency/pointer write and sync boundary;
  readers choose one complete generation or degrade, never a partial closure.
- Duplicate immutable writes converge on one key; attempts to overwrite an
  immutable key with different bytes are rejected before publication.
- Dirty-startup instrumentation proves work is proportional to entry headers
  and envelope prefixes, not total IndexArtifact body bytes.
- GC, backup, verify, repair, metrics, and counters enumerate the new KV type
  without treating it as user namespace data or silently dropping it.
- Logical backup without indexes emits `needs_rebuild`; physical copy and
  validated include-indexes mode preserve only complete closures.
- Cross-platform golden/property tests prove canonical strings, maps, options,
  bitmaps, IDs, CRCs, and keys are byte-identical.

---

## Round 3A: Endianness and Snapshot/Replication Contract

**Status:** Answered by Wyatt. The two resulting format decisions continue in
Round 3B below.

### 1. Are existing AeorDB data points already little-endian?

Not all byte sequences are little-endian, and they should not be.

The code audit found that AeorDB's **persistent structural integers** are
consistently explicit little-endian in the database header, entity header, KV
block/pages, B-tree framing, hot tail, NVT framing, FileRecord metadata,
directory entries, symlink/deletion records, and v0 FieldIndex framing. No
native-endian persistent codec was found.

Big-endian conversions outside media-format parsers occur in a different
semantic layer:

- JSON numeric field values become big-endian bytes;
- virtual numeric values such as `@created_at`, `@updated_at`, and `@size`
  become big-endian bytes;
- query literals produce the same big-endian representation; and
- scalar converters read those bytes as big-endian.

That is intentional in the existing index path: fixed-width unsigned
big-endian bytes preserve numeric order under lexicographic byte comparison.
Media parsers also read whichever endianness their source file format requires;
that says nothing about AeorDB's storage framing.

**Recommendation:** Freeze this precise rule:

- every multibyte **artifact framing, metadata, length, count, ID component,
  offset, timestamp, sequence, coordinate, and checksum integer** uses explicit
  little-endian;
- hashes, IDs, UTF-8, bitmaps, and arbitrary byte strings are byte arrays and
  have no integer endianness;
- converter-produced canonical field values are opaque to the artifact codec
  and retain their converter-defined canonical representation, which may use
  big-endian specifically for sortable numerics;
- each converter version must independently freeze and test its canonical byte
  representation; and
- native-endian persistence remains forbidden.

Thus the Round 3 envelope and primitive tables remain little-endian, while the
already-approved opaque canonical-value exception is explicit rather than
buried in one bullet.

Do you approve this clarified endianness contract?

**Your answer: I'd like the endianness to be consistent across the entire DB. Anything that is currently writing big-endian (i.e. serde) we should see if we can flip to instead be little-endian. If we _struggle_ with this for any reason, then we should discuss this further.**

### 2. How do immutable and stable-pointer keys affect snapshots and replication?

The key design is compatible with both, but only if we distinguish namespace
history from database-local derived state.

#### Namespace Snapshots

- A snapshot remains a named immutable namespace root hash. Creating one does
  not copy or rewrite any IndexArtifact.
- IndexArtifacts are not directory children, so mutable active-pointer keys are
  never captured inside a namespace Merkle root. A later pointer publication
  cannot change what an older snapshot means.
- A historical query may use an immutable retained `FieldIndexManifest` only
  when its exact definition fingerprint and `SourceVersion.head_hash` match the
  selected snapshot root and its complete scope/value/page closure validates.
- Ordinary snapshots do not pin every matching derived generation. Doing so
  would make frequent snapshots retain potentially enormous duplicate index
  closures. Active/previous pointers, tasks, requests, and GC grace provide
  ordinary artifact retention.
- If no matching retained generation exists, the query performs an
  authoritative historical scan or reports the already-defined explicit
  historical-index-unavailable error. It never uses the current pointer against
  an older root.
- Snapshot restore changes HEAD. Existing pointers remain physically present,
  but their source version no longer matches; affected indexes enter
  reconciliation/rebuild and cannot advertise current results until caught up.

An explicit future "pin index acceleration for this snapshot" operation could
root a closed immutable generation, but that is storage policy, not ordinary
snapshot semantics and not required by this refactor.

#### Physical Database Copies

- A byte-for-byte/copy-on-write physical database copy retains both pointer
  revisions, immutable closures, capabilities, database ID, and all artifact
  keys unchanged.
- Startup still verifies pointer/manifest/source-version consistency; physical
  copying does not waive crash-recovery checks.

#### Logical Backup, Peer Replication, and Client Sync

- Standard logical export, patch, peer replication, and client sync transfer
  authoritative namespace data and index definitions, not database-local
  derived artifacts or active pointers.
- The destination allocates/retains its own scope ordinals, builds its own
  artifacts, and publishes its own pointer revisions. Omitted indexes are
  explicitly `needs_rebuild`, never silently current.
- Stable pointer keys are stable only as logical keys inside one database's KV
  state. A remote pointer payload is never accepted as authority or replayed as
  the destination's current state.

#### Optional Compatible-Artifact Transfer

- A root-only negotiated transfer may copy immutable artifacts unchanged
  because their keys are content-derived and do not contain the database ID.
- Transfer requires identical hash algorithm, artifact/capability versions,
  definition and converter fingerprints, source root, complete scope catalog
  including ordinal assignments, and a closed verified artifact closure.
- The destination verifies every immutable key/value and reference, then writes
  a **new local A/B pointer publication** with local entity write sequence and
  pointer sequence. Source pointer records are not copied as authority.
- Persisted boot-scoped visibility tokens from the source are never compared as
  local epochs. The destination reconciles from the durable SourceVersion before
  advertising the transferred generation current.
- If any prerequisite differs, transfer falls back to definitions plus local
  rebuild. It never rewrites a partially compatible graph or imports foreign
  ordinals optimistically.

The core safety property is that snapshots and replication name authoritative
namespace roots; mutable active pointers select only database-local derived
accelerators. Immutable content keys allow old generations and validated
transfer without letting a pointer redefine historical content.

Do you approve this snapshot, physical-copy, logical-replication, and optional
artifact-transfer contract for the proposed key model?

**Your answer: Yes, I think this all makes sense... however, it has me wondering if maybe it was a GOOD idea to store indexes as files in the directory tree? That way a snapshot is really just a root_hash, and EVERYTHING in the database is based on the file structure, and so a snapshot is always entirely complete? Food for thought.**

## Round 3A Proof Additions

- An endianness inventory test/golden manifest classifies every persisted
  numeric field as LE framing or converter-owned opaque bytes; lint/review gates
  reject native-endian persistence.
- Cross-platform fixtures prove structural LE bytes are identical while
  converter fixtures prove each intentionally big-endian canonical numeric
  representation independently.
- Creating many namespace snapshots writes no IndexArtifact and does not retain
  every transient index generation.
- Historical queries never use a manifest with a mismatched source root,
  including after pointer advancement, snapshot deletion, or restore.
- Physical copies retain and validate pointer/artifact closures unchanged.
- Standard logical sync cannot transmit or activate a source pointer/artifact
  accidentally.
- Compatible transfer accepts only an exact closed immutable graph, publishes a
  fresh local pointer, ignores foreign boot epochs, and falls back to rebuild on
  every mismatch or interrupted transfer.

---

## Round 3B: Little-Endian V1 Values and Snapshot Completeness

**Status:** Approved. V1 engine-owned numeric values are little-endian behind a
versioned migration boundary, and v1 derived artifacts remain outside the
namespace tree.

### 1. Should v1 canonical numeric field values also be little-endian?

Yes. The code audit supports making this consistent, with one migration
boundary that we must not blur.

The current big-endian index values are not produced by serde. They come from
explicit calls to `to_be_bytes()` in the JSON parser, virtual-field indexing,
and query-literal conversion. The corresponding scalar converters explicitly
read those bytes as big-endian. That representation let old code compare some
fixed-width numeric values lexicographically, but it is not a requirement of
the new NVT design.

**Recommendation:** Freeze the following v1 contract:

- Every engine-owned multibyte numeric representation is explicit
  little-endian. This includes artifact framing and metadata as already
  approved, plus canonical numeric field values produced for v1 indexes and
  queries.
- A v1 query literal and a v1 indexed value always pass through the same typed,
  versioned converter. Numeric comparison is semantic typed comparison, not
  raw byte-string ordering.
- Any v1 map or set that needs numeric order sorts by the converter's semantic
  order and then emits its canonical little-endian value. Generic
  lexicographic byte ordering must not be used as a substitute for numeric
  order.
- Floating-point converters define a deterministic total order, including
  signed zero, infinities, and NaN handling. Little-endian float bytes are not
  lexicographically sortable.
- Strings, hashes, UTF-8, bitmaps, and arbitrary user byte strings are byte
  sequences and do not have integer endianness. Parsers for external media
  formats continue to honor the source format's required endianness.
- Native-endian persistence remains forbidden.

We must not change the existing v0 `to_be_bytes()` calls in place and then
pretend old artifacts use the new representation. That would make existing
indexes and query literals disagree. V0 readers retain the frozen big-endian
converter behavior. The side-by-side index migration decodes each old value
with its v0 converter and emits the v1 little-endian canonical form through its
v1 converter. Only after the new generation verifies and publishes may the v0
generation retire.

<!-- TOTALLY agree! Obviously we must keep existing ordering _until after we migrate_. I understand this, and agree with you. -->

Golden fixtures will therefore include both:

- frozen v0 big-endian numeric values, proving backward-readable migration; and
- canonical v1 little-endian values, proving identical bytes and semantic order
  across supported platforms.

This gives us one little-endian rule for all new engine-owned numerics without
silently corrupting the meaning of bytes already stored in live databases.

Do you approve this versioned endianness and migration contract?

**Your answer: Yes, absolutely.**

### 2. Should v1 derived indexes be files inside the namespace tree?

No. The old approach makes snapshots look more self-contained, but it creates a
fundamental cycle:

1. An index generation is built for namespace root `H0`.
2. Writing its pages as directory files changes the namespace root to `H1`.
3. The generation's source root is now stale before publication.
4. Indexing the new root and writing that result changes the root again.

Excluding an internal path from search indexing does not solve this. Writing
that path still changes directory records and therefore the Merkle root.

It also couples high-churn derived state to authoritative namespace state:

- every page flush creates a new namespace HEAD even when no user data changed;
- snapshots pin partial or obsolete generations and can retain enormous index
  histories;
- millions of derived pages become directory entries, FileRecords, chunks,
  permission checks, listing/counter inputs, Merkle diffs, SSE changes, and GC
  graph nodes;
- an index lookup becomes directory traversal plus file/chunk assembly instead
  of a direct artifact lookup;
- logical replication transfers database-local ordinals and accelerators that
  may be invalid at the destination; and
- index corruption becomes namespace-tree corruption instead of a degradable,
  rebuildable accelerator failure.

The key distinction is **authoritative completeness** versus **acceleration
completeness**. A namespace snapshot is authoritatively complete when its root
retains all user data, metadata, and index definitions needed to reproduce
correct answers. Derived pages are disposable execution aids. A historical
query may use an exact retained generation whose `SourceVersion` matches the
snapshot root; otherwise it scans or rebuilds from the authoritative snapshot.
Its correctness does not depend on retaining the accelerator.

**Recommendation:** Keep IndexArtifacts out of the namespace and retain the
Round 3A snapshot/replication contract. Do not add snapshot index pinning to the
first refactor. If real usage later requires performance-complete historical
snapshots, add an explicit optional acceleration binding:

~~~text
namespace root hash -> immutable IndexRootSet
~~~

That binding would pin a closed, verified set of manifests without placing
mutable pointers or derived pages in the namespace. It would be opt-in because
its storage cost could be very large. The namespace root remains the
`SourceVersion`, avoiding the feedback cycle.

Do you approve keeping v1 derived artifacts outside the directory tree and
deferring optional snapshot acceleration pinning until there is a demonstrated
need?

**Your answer: Yes, and thank you for talking me through this, so I can understand your reasoning. Good thinking here. Thank you for the push-back.**

## Round 3B Proof Additions

- V0 fixtures continue to decode numeric canonical values as big-endian while
  v1 fixtures encode all engine-owned numerics little-endian.
- Migration tests rebuild mixed numeric/string indexes through versioned
  converters and prove v0 and v1 queries return identical logical results
  before v0 retirement.
- Static audit gates reject new native-endian persistence and unregistered
  big-endian engine-owned numeric encodings.
- Numeric index ordering tests use typed converter comparison and cover signed
  integers, unsigned integers, boundaries, signed zero, infinities, and the
  chosen NaN policy.
- Publishing or flushing an IndexArtifact does not change namespace HEAD.
- Creating a namespace snapshot does not copy, publish, or pin mutable index
  state.
- A historical query accepts an accelerator only when its exact source root and
  complete closure match; absence of one changes performance, never results.
- Logical replication and client sync reproduce authoritative data and index
  definitions without importing database-local active pointers or ordinals.

---

## Round 4: Exact Active-Pointer Format and Publication

**Status:** Approved. The pointer-version clarification was approved in Round
4A, so the complete active-pointer contract is closed.

This round freezes the only mutable records in the v1 IndexArtifact family.
The audit found that the earlier plan put too much state in active pointers:
source versions, runtime visibility, registry state, and previous manifests
already have authoritative owners elsewhere. Duplicating them would create
disagreement rules and make publication harder to reason about.

The pointer should instead behave like one small, typed, crash-safe reference.
The next round will freeze the four immutable manifest bodies and their exact
definition identities.

### 1. Do you approve this minimal active-pointer encoding?

All three pointer kinds use the same codec. The pointer kind determines the
owner type and required target manifest kind:

| Pointer kind | Owner | Required target |
| --- | --- | --- |
| `FieldIndexActivePointer` | `IndexId` | `FieldIndexManifest` |
| `FieldNvtActivePointer` | `IndexId` | `FieldNvtManifest` |
| `ScopeCatalogActivePointer` | `ScopeId` | `ScopeCatalogManifest` |

The common IndexArtifact envelope identity is exactly:

| Identity offset | Size | Field | Rule |
| ---: | ---: | --- | --- |
| 0 | `H` | `owner_id` | Exact `IndexId` or `ScopeId` |
| `H` | 1 | `slot` | `0` for A or `1` for B |

The pointer body is exactly:

| Body offset | Size | Field | Rule |
| ---: | ---: | --- | --- |
| 0 | 8 | `pointer_sequence` | Nonzero `u64` little-endian |
| 8 | `H` | `target_manifest_hash` | Exact immutable manifest artifact key |

The envelope `generation` is the target manifest generation. It must equal the
generation in the target manifest's identity and envelope. The pointer's
complete stored value is therefore exactly `45 + 2H` bytes: 109 bytes for a
32-byte database hash or 173 bytes for a 64-byte database hash.

The following fields are deliberately absent:

- No source root or runtime visibility token. Those belong to the immutable
  target manifest and must be checked there.
- No definition or converter fingerprint. The owner ID and target manifest
  prove those.
- No registry, build, degraded, or migration state. Low-volume versioned system
  records own control-plane state.
- No `previous_manifest_hash`. The other A/B slot is the previous publication;
  embedding predecessor links would retain extra generations and create
  conflicting fallback paths.
- No timestamp or outer write sequence. The EntityHeader already carries both;
  `pointer_sequence` orders publications across the two stable logical keys.

Adding pointer fields in the future requires a new pointer artifact schema. V1
does not reserve variable body space because every rewrite is tiny and old
pointer records remain readable through their exact schema.

Do you approve this exact identity, body, length, and ownership split?

**Your answer: There is one thing missing that I think it probably critical, but I'll let you push back if it isn't: a version. We will have a pointer "codec" to read pointers, but don't we also want a version, for future expansion/modification?**

### 2. Do you approve these A/B read and rewrite rules?

**Reader selection:**

1. Resolve the current KV revision for both stable A/B keys.
2. Structurally validate each pointer's entity framing, key, envelope, identity,
   slot, nonzero sequence, CRC, and target kind/generation.
3. Shallow-validate the target manifest and its required directory roots before
   calling that pointer usable. Page bodies remain lazy.
4. Choose the closure-valid pointer with the greatest `pointer_sequence`.
5. If only one closure validates, use it regardless of whether the rejected
   slot claimed a larger sequence.
6. Equal sequences with the same target are accepted as a redundant
   publication, emit a repair diagnostic, and choose slot A deterministically.
   Equal sequences with different targets are ambiguous corruption: neither
   wins, the index degrades, and authoritative fallback/rebuild policy applies.
7. If neither closure validates, no index generation is advertised usable.

The pointer record itself is **structurally valid** before its target closure is
known to be valid. This distinction matters to sequence allocation.

**Writer selection:**

1. Read both slots and retain every structurally valid sequence, even when its
   target closure is missing or corrupt.
2. Compute `next_pointer_sequence = max(structurally_valid_sequences) + 1`
   using checked arithmetic. Overflow is a fatal format-exhaustion error; it
   never wraps.
3. Overwrite a missing or structurally invalid slot first. Otherwise overwrite
   the lower-sequence slot, preserving the highest prior publication as
   fallback. For the accepted equal/same-target repair case, overwrite slot B.
4. On a brand-new pointer pair, write only slot A with sequence 1. Slot B
   remains absent until the second publication.
5. Write and validate every immutable dependency before appending the selected
   pointer revision and inserting its KV mapping last.

Counting a structurally valid but dependency-invalid pointer when allocating
the next sequence prevents sequence reuse after a crash persisted the pointer
but lost one of its soft-durable dependencies. The other closure-valid slot
still serves reads while the damaged slot is replaced.

Do you approve these deterministic selection, ambiguity, overflow, and rewrite
rules?

**Your answer: I do very much like this plan... with the exception of the outstanding "version" question.**

### 3. Do you approve soft and hard publication modes?

One durability mode cannot satisfy both high-throughput routine index flushing
and production migration cutover.

#### Soft Publication

Routine derived-index flush, hint healing, and NVT resize use a byte-bounded
ordered artifact batch:

1. Append immutable pages/directories.
2. Append the immutable manifest.
3. Append the inactive pointer revision last.
4. Insert their KV mappings in the same order while holding the writer/KV lock.
5. Expose the new pointer only after the complete in-memory batch succeeds.
6. Let the normal hot-tail timer make the batch durable.

The operation may return before an explicit filesystem sync. A crash can lose
all or part of the publication. Recovery therefore validates the selected
closure and falls back to the other slot or authoritative reconciliation. This
cannot lose authoritative user data because IndexArtifacts are derived.

The dedicated publication batch has configured entry and byte limits and
segments immutable dependencies before the final manifest/pointer batch. A
pointer is never split away from its manifest by a soft batch boundary.

#### Hard Publication

V0-to-v1 migration cutover/finalization, explicit administrative durable
publication, and clean shutdown use two barriers:

1. Append immutable dependencies and the manifest.
2. Force the hot tail/KV recovery state and database file through the existing
   durability latch. Return and latch any sync failure.
3. Append the inactive pointer alone.
4. Force the hot tail/KV recovery state and database file again.
5. Re-read and validate the pointer and complete shallow closure before
   reporting success or permitting retirement of the old format.

The first barrier prevents a durable pointer from preceding its dependencies.
The second makes the pointer itself recoverable. Neither path claims general
transaction rollback. Orphaned immutable artifacts are safe and later
collectable.

Normal namespace writes still retain their mutation overlays until every
affected active index has either published the mutation or entered explicit
degraded/reconciliation state. Soft publication therefore changes crash
recovery cost, not live-query correctness.

Do you approve this durability split and the listed operations that require
hard publication?

**Your answer: Yes**

### 4. Do you approve bounding retained generations through A/B roots?

Immutable manifests must **not** contain predecessor-manifest links. If GC
followed such a link recursively, every generation would retain the complete
history forever.

Instead:

- both closure-valid A/B pointer targets are derived GC roots;
- active request pins, migration/build tasks, repair operations, and optional
  future snapshot-acceleration bindings are additional temporary roots;
- each manifest traces only its own directory/value/scope dependencies;
- after a third publication replaces the older slot, that displaced manifest
  and its uniquely owned closure become ordinary pending-delete candidates;
- the configured pending-delete grace and required complete marks still apply;
  and
- a manifest shared by another pointer, task, or closure remains live through
  that independent reference.

This normally retains two published generations without baking a fixed
retention count into the immutable format. Administrators who later need more
rollback generations should use an explicit retention/pin policy, not hidden
manifest history links.

Index removal first changes the authoritative definition/registry state so no
new query selects it, then durably retires both stable pointer keys. A crash
halfway through pointer retirement may retain storage longer, but cannot make a
removed definition queryable; startup/reconciliation finishes the retirement.

Do you approve this reachability and retirement model?

**Your answer: Yes**

### 5. Do you approve the state-authority boundary?

The final ownership rules are:

| State | Authoritative owner |
| --- | --- |
| Selected immutable generation | Active pointer |
| Definition, source root, runtime visibility cutoff, roots, counts | Target manifest |
| Current/building/reconciling/degraded/removed state and reason | Versioned system registry/task record |
| Unpublished additions/removals/value changes | Bounded mutation overlay or spill journal |
| Request lifetime | In-memory generation/root pin |
| Durable format support | Database and artifact capability/schema fields |

A query may advertise an index usable only when all four relevant views agree:

1. the authoritative config/registry selects that `IndexId` and does not mark it
   unavailable;
2. one A/B pointer and its exact manifest closure validate;
3. the manifest source/visibility plus retained overlays cover the requested
   root and visibility; and
4. every required format/converter/plugin capability is available.

No layer silently repairs disagreement by copying fields from another layer.
It reports the typed mismatch, degrades that index, and invokes the defined
fallback/reconciliation policy.

Do you approve this single-owner state model?

**Your answer: Yes**

## Round 4 Proof Obligations

- Golden fixtures cover all three pointer kinds, both slots, both hash widths,
  exact stable keys, exact stored lengths, and pointer sequences 1 and
  `u64::MAX`.
- Mutation tests alter every owner, slot, sequence, generation, target hash,
  envelope byte, and CRC and prove deterministic rejection.
- A/B property tests generate arbitrary valid/invalid slot pairs and prove
  deterministic selection, equal-sequence handling, and no sequence reuse or
  wrap.
- Crash tests interrupt every soft and hard publication write, KV insertion,
  hot-tail flush, and sync boundary. Readers select one complete generation or
  degrade; they never expose a partial closure.
- Hard-cutover tests prove no v0 generation retires until the v1 pointer and
  complete closure survive close/reopen and full validation.
- GC graph tests prove two pointer slots retain only their two closures, shared
  dependencies survive, removed/replaced generations enter pending deletion,
  and no predecessor chain leaks history.
- Concurrency tests pin an old manifest across two newer publications and GC;
  the reader completes against one immutable closure before its temporary pin
  releases.
- Registry/pointer/manifest/overlay disagreement tests always produce a typed
  degraded state or authoritative fallback, never a partial or falsely current
  result.

---

## Round 4A: Pointer Codec Versioning

**Status:** Approved. Pointer codec version authority and side-by-side upgrades
are closed.

You are correct that every pointer codec needs an explicit version. The pointer
already has one, but Round 4 did not make that inheritance clear enough.

Every pointer is an IndexArtifact, so its complete stored value begins with the
approved common `AIDX` envelope:

| Envelope offset | Size | Field | Pointer v1 value |
| ---: | ---: | --- | --- |
| 0 | 4 | `magic` | ASCII `AIDX` |
| 4 | 2 | `artifact_schema_version` | `1` |
| 6 | 2 | `artifact_kind` | One of the three pointer kinds |
| 8 | 2 | `envelope_header_length` | `32` |

`artifact_schema_version` is a **per-kind complete codec version**, not one
global version that forces every IndexArtifact kind to advance together. The
decoder dispatch key is:

~~~text
(artifact_kind, artifact_schema_version)
~~~

For example:

~~~text
(FieldIndexActivePointer, 1) -> decode_field_index_pointer_v1
(FieldNvtManifest,          1) -> decode_field_nvt_manifest_v1
(FieldIndexActivePointer, 2) -> a future decode_field_index_pointer_v2
~~~

A future pointer v2 does not require unchanged manifest/page kinds to become
v2. Conversely, a future manifest v2 does not reinterpret a pointer v1.

I recommend **not** adding a second `pointer_codec_version` field to the body.
Two version fields could disagree, forcing an arbitrary authority rule. The
common envelope version is early, fixed-width, checksummed, available before
body allocation, and already controls the complete identity/body decoder.

The already-approved stable pointer key domain is also versioned:

~~~text
H(
  ASCII "aeordb.index-artifact.pointer.v1\0" ||
  artifact_kind u16 LE ||
  canonical pointer identity
)
~~~

That is important. A future incompatible pointer v2 uses a `pointer.v2` key
domain and therefore a separate A/B pair. It must not overwrite a v1 stable
key: an old reader would resolve that key's latest KV revision, encounter an
unknown body, and lose access to the older v1 revision hidden behind it.

Pointer-format migration therefore works side by side:

1. Continue serving the registry-selected v1 A/B pair.
2. Write and hard-validate the v2 immutable dependencies and v2 A/B pair under
   the v2 key domain.
3. Durably change the versioned registry/capability selection to v2.
4. Keep v1 as rollback state until migration validation and configured grace
   permit retirement.

An unsupported pointer schema is never guessed from body length. It is reported
as an unsupported capability/version; the engine may use a separately selected
supported pointer generation or authoritative fallback, but never reinterpret
the bytes.

Thus pointer v1 has three nested and intentionally different versions:

- `EntityHeader.entry_version = 1` describes universal entity framing;
- `IndexArtifact.artifact_schema_version = 1` describes the complete
  kind-specific pointer codec; and
- the pointer key domain `pointer.v1` keeps incompatible stable-key revisions
  side by side during migration.

The exact v1 identity/body and `45 + 2H` length from Round 4 remain unchanged.

Do you approve this version authority and side-by-side pointer-upgrade
contract?

**Your answer: Ah, yes, I see and understand. I approve.**

## Round 4A Proof Additions

- Golden fixtures prove the schema version appears in the common envelope even
  though the pointer body itself contains no duplicate version.
- Dispatch tests independently mix every registered kind and schema version;
  no decoder is selected by body length or Rust type layout.
- Mutation tests change the envelope version while retaining a valid v1 body
  and prove deterministic unsupported-version rejection.
- Side-by-side migration tests prove v1 and v2 stable keys coexist, old readers
  retain the v1 pair, new readers do not select v2 before registry cutover, and
  interrupted cutover preserves one supported pair.

---

## Round 5: Semantic Identities and Manifest Ownership

**Status:** Approved. Semantic IDs, definition ownership, reference/coverage
graphs, NVT non-pinning, and manifest limits are closed.

Before assigning manifest byte offsets, we need to freeze what each stable ID
means and which manifest owns each fact. The original plan duplicated
`owner_path`, field name, converter, scope-manifest reference, source version,
and predecessor state in several places. That creates circular or contradictory
validation rules.

This round defines the semantic identity graph. Round 6 will turn the approved
graph into exact binary tables for `ScopeCatalogManifest`,
`ValueStoreManifest`, `FieldIndexManifest`, and `FieldNvtManifest`.

### 1. Do you approve layered semantic IDs and generation allocation?

**Recommendation:** Derive IDs from canonical semantic definitions:

~~~text
ScopeId =
  H("aeordb.index.scope-definition.v1\0" ||
    canonical ScopeDefinitionV1)

ValueStoreId =
  H("aeordb.index.value-store-definition.v1\0" ||
    canonical ValueStoreDefinitionV1)

ConverterFingerprint =
  H("aeordb.index.converter-definition.v1\0" ||
    canonical ConverterDefinitionV1)

IndexId =
  H("aeordb.index.field-definition.v1\0" ||
    canonical FieldIndexDefinitionV1)
~~~

`ValueStoreDefinitionV1` contains its `ScopeId`, and
`FieldIndexDefinitionV1` contains its `ValueStoreId` and canonical
`ConverterDefinitionV1`. `ConverterFingerprint` is derived from those converter
bytes rather than independently authored. Semantic ownership is therefore
transitive rather than repeated independently.

The definitions include every behavior that can change membership, extracted
values, canonical equality/order, token expansion, or query results. They
exclude physical artifact schema, page packing, compression, cache policy, NVT
resolution, and tile size. Those change storage/performance, not index
semantics.

This corrects one line in the earlier plan that included the physical
"index-format version" in `IndexId`. Canonical-value and converter semantic
versions belong in the ID; physical manifest/page codec versions do not.
Keeping `IndexId` semantic lets the same index lineage migrate from pointer/page
v1 to v2 without pretending it is a different configured index. Versioned
pointer key domains and artifact schemas provide physical coexistence.

No definition ID contains namespace HEAD, runtime visibility, generation,
database ID, or physical offset. The chosen database hash algorithm determines
`H` and the resulting ID width, as already approved.

Every immutable manifest identity remains:

~~~text
owner_id H || generation u64 LE
~~~

where the owner is `ScopeId`, `ValueStoreId`, or `IndexId`. A generation is a
nonzero database-local token reserved from the v4 durable global write-sequence
allocator. Reserving a generation consumes one sequence value even though that
value need not be an EntityHeader's own write sequence. Gaps are valid; reuse
and wrap are forbidden. All pages/directories directly owned by one manifest
generation carry that manifest's generation where their kind requires it.
Referenced scope, value, and field manifests retain their own independent owner
generations.

Compatible cross-database artifact transfer preserves foreign generation
values only when no conflicting owner/generation identity exists locally.
Otherwise it rebuilds locally; it never rewrites content-addressed artifacts to
force a generation remap.

Do you approve these ID domains, the semantic/physical distinction, and
generation allocation?

**Your answer: Yes**

### 2. Do you approve this exact ownership boundary for `ScopeDefinitionV1`?

A scope definition answers only: **which namespace files share one document
ordinal universe?** Its canonical bytes contain:

1. `scope_definition_version = 1`;
2. normalized absolute owner path;
3. membership entity class (`regular_files` in v1);
4. matching mode: direct children or relative-path glob;
5. the optional canonical glob pattern;
6. path normalization/case semantics version;
7. glob matcher semantics version;
8. effective-config precedence/resolver semantics version;
9. FileKey derivation schema version; and
10. system/internal-path inclusion policy version.

The definition does **not** contain:

- the current namespace root or current matching files;
- configured field/index lists;
- parser, mapper, converter, or plugin behavior;
- logging, parser memory, cache, flush, or query budgets;
- physical scope-page/directory codec versions; or
- child configuration contents.

Changing owner path, glob, file membership class, normalization, FileKey
semantics, or resolver precedence creates a new `ScopeId` and ordinal lineage.
Changing fields or parsers does not reset document ordinals when file
membership semantics remain the same.

Adding/removing a child config can move files between effective scopes without
changing either scope's definition bytes. That is a namespace/config mutation:
reconciliation tombstones documents in the old scope and allocates them in the
new one. Ordinals remain never-reused within each lineage.

The `ScopeCatalogManifest` embeds the complete canonical
`ScopeDefinitionV1`; decoding recomputes and verifies the identity `ScopeId`.
Human-readable owner path is therefore stored once as part of its semantic
owner, not repeated in every field manifest.

Do you approve this scope definition and the rule for child-config changes?

**Your answer: Yes**

### 3. Do you approve a strategy-neutral canonical value store?

One extracted field can feed several strategies, such as string, trigram, and
phonetic. Duplicating the original extracted value in every strategy index
wastes disk and memory and complicates exact recheck.

**Recommendation:** `ValueStoreDefinitionV1` owns extraction semantics and
contains:

1. `value_store_definition_version = 1`;
2. `ScopeId`;
3. canonical query field name;
4. canonical typed source-selector/extractor definition;
5. parser/mapper resolution semantics and a sorted dependency graph containing
   relevant native/plugin IDs, versions, checksums, and semantic arguments;
6. strategy-neutral canonical source-value codec version;
7. missing/null/error, multi-value ordering, and duplicate-value semantics;
8. maximum source values and canonical source bytes per document;
9. deterministic unindexable-state policy; and
10. every parser/mapper timeout or memory/output limit that can change whether
    a document produces values.

Operational logging, cache sizes, flush cadence, and query-time budgets are
excluded because they cannot change stored values.

The value pages store typed, strategy-neutral `CanonicalSourceValue` records,
not trigram tokens, phonetic codes, NVT coordinates, or a strategy converter's
private byte representation. Each strategy converter consumes those source
values, creates its own canonical posting values/tokens, and uses the shared
source values for exact recheck, scoring, sorting, grouping, and aggregation.

Two strategies may share one `ValueStoreManifest` only when their complete
`ValueStoreId` is identical. A source selector, parser/plugin implementation,
dependency checksum, limit, null/multi-value rule, or canonical source-value
schema change creates a new ValueStoreId and shadow build.

Global parser selection is captured as a canonical resolution-policy and
dependency-set fingerprint, not as whichever plugin happens to be installed
when a document is later read. Historical execution still requires the named
dependencies to be available; absence produces the approved explicit
dependency error.

Do you approve this value-store boundary and strategy-neutral source-value
representation?

**Your answer: Yes**

### 4. Do you approve this ownership boundary for `FieldIndexDefinitionV1`?

A field-index definition answers: **how does one strategy turn the shared
source values into ordered postings and query semantics?** Its canonical bytes
contain:

1. `field_index_definition_version = 1`;
2. `ValueStoreId`;
3. permanent strategy ID plus canonical strategy name;
4. complete canonical `ConverterDefinitionV1`; its
   `ConverterFingerprint` is derived and verified rather than independently
   authored;
5. canonical posting-value/coordinate schema versions;
6. Unicode normalization, case folding, collation, tokenizer, and expansion
   semantics relevant to that strategy;
7. supported equality/range/fuzzy operation semantics;
8. expanded-token deduplication and value-ordinal semantics;
9. maximum expanded tokens/postings/canonical posting bytes per document; and
10. deterministic over-limit/unindexable behavior.

It excludes owner path, glob, source selector, parser, and source-value limits
because those are owned transitively by the ValueStore and Scope definitions.
It also excludes page size, split threshold, directory fanout, page
compression, cache policy, NVT resolution, and tile size.

Any semantic converter bound, canonicalization, collation, tokenizer,
expansion, posting limit, or supported-operation change creates a new
`IndexId`. A physical repack, page-codec migration preserving semantics, NVT
resize, or cache tuning does not.

Migration from v0 creates a canonical v1 definition that explicitly names the
adapter preserving the v0 converter's **effective** behavior, including
historical lossy numeric-bound casting. Corrected typed bounds are a separate
new definition and shadow build, never silently folded into format migration.

Do you approve this field-index definition boundary?

**Your answer: Yes**

### 5. Do you approve this manifest reference and coverage graph?

The correctness-bearing closure is:

~~~text
FieldIndexActivePointer
  -> FieldIndexManifest
       -> posting ArtifactDirectory root
       -> exact ValueStoreManifest
            -> value ArtifactDirectory root
            -> exact ScopeCatalogManifest
                 -> ordinal->document ArtifactDirectory root
                 -> FileKey->ordinal ArtifactDirectory root
~~~

There is no direct `FieldIndexManifest -> ScopeCatalogManifest` edge. The exact
ValueStoreManifest owns that reference, preventing two copies from disagreeing.
There is no ValueStore active pointer; value generations become usable only
through a field manifest, while build/task state retains unpublished value
generations.

`ScopeCatalogManifest`, `ValueStoreManifest`, and `FieldIndexManifest` each
carry an exact `CoverageVersionV1`:

~~~text
source_head_hash       H bytes
runtime_boot_id        16 raw bytes
runtime_sequence       u64 LE
~~~

All three coverage values in one usable field closure must be identical.
Duplication here is an intentional cross-layer consistency assertion: each
manifest describes which source/overlay cutoff its own data represents. A
mismatch invalidates the closure; no reader chooses one value as "more
authoritative" and combines mixed generations.

After restart, a persisted foreign boot token is never compared to the new
boot's sequence. If `source_head_hash` equals current HEAD and the closure
validates, it becomes the current boot's sequence-zero disk base. Otherwise the
index reconciles from the manifest source root before it can be advertised
current.

The non-authoritative NVT closure stays separate:

~~~text
FieldNvtActivePointer
  -> FieldNvtManifest
       -> tile ArtifactDirectory root
~~~

The NVT manifest stores `basis_posting_generation` and
`basis_source_head_hash` as freshness metadata, but **not** a
`FieldIndexManifest` artifact hash. Making the basis a GC edge would let a stale
but still useful NVT pin an obsolete posting/value/scope closure indefinitely.
At query time every hinted PageId is resolved and validated against the
currently pinned FieldIndexManifest. Missing, retired, or incompatible pages
fall back to predecessor/forward scan and may enqueue hint healing.

Do you approve this reference graph, exact coverage equality, restart rule, and
non-pinning NVT basis?

**Your answer: Yes**

### 6. Do you approve embedded definitions and removal of ambiguous history
fields?

Each correctness-bearing manifest embeds its one canonical semantic definition:

- `ScopeCatalogManifest` embeds `ScopeDefinitionV1`;
- `ValueStoreManifest` embeds `ValueStoreDefinitionV1`; and
- `FieldIndexManifest` embeds `FieldIndexDefinitionV1`.

This lets a manifest validate its owner ID without consulting mutable registry
state, preserves diagnostics and compatible-transfer semantics, and avoids
adding another physical artifact kind. The NVT manifest embeds no definition;
its `IndexId` and runtime comparison against a selected field manifest are
sufficient because hints are disposable.

V1 manifests contain neither `previous_manifest` nor `logical_checksum`:

- A/B pointers and explicit task/request pins already own bounded history.
  Manifest predecessor links would leak every generation through GC.
- CRC, entity integrity hash, immutable artifact key, and exact directory roots
  already prove byte/closure identity.
- A layout-independent logical checksum has no safe unspecified meaning.
  Defining one as a full sorted posting digest would add substantial write cost;
  a cheap commutative checksum would not be strong enough to bless migration.
- Migration validation uses authoritative source evaluation as its oracle and
  records a separate validation report. It does not treat v0 equality or one
  ambiguous digest as proof.

Freeze these v1 limits:

~~~text
canonical ScopeDefinitionV1:       at most 64 KiB
canonical ValueStoreDefinitionV1:  at most 512 KiB
canonical FieldIndexDefinitionV1:  at most 256 KiB
complete manifest stored value:    at most 1 MiB
normalized owner path:             at most 64 KiB
canonical query field name:        at most 4 KiB
canonical strategy name:           at most 256 bytes
canonical converter definition:    at most 64 KiB
dependency descriptor set:         at most 256 KiB
~~~

Lengths are UTF-8 byte lengths where applicable. The exact manifest tables in
Round 6 will impose smaller fixed-body and count limits. Exceeding a definition
limit rejects/degrades that index definition explicitly; it never truncates,
hashes an incomplete prefix, or falls back to unstable serialization.

Do you approve embedding definitions, omitting predecessor/logical-checksum
fields, and these hard limits?

**Your answer: Yes**

## Round 5 Proof Obligations

- Domain fixtures prove a one-byte semantic-definition change changes the
  correct ID and that physical page/tile/cache changes do not.
- Cross-platform fixtures prove canonical definitions and all four ID domains
  are byte-identical for both hash widths.
- Generation tests reserve, skip, crash, reopen, exhaust, and reject reuse or
  wrap without requiring one durable header write per generation.
- Scope tests cover direct/glob membership, normalization, child overrides,
  cross-scope move, tombstone/no-reuse, and changes that do or do not create a
  new ScopeId.
- Value-store tests prove multiple strategies share source values only under
  one exact ValueStoreId and that every parser/extractor/dependency/limit
  semantic change creates a shadow identity.
- Field-definition tests distinguish semantic converter/tokenizer/limit
  changes from physical page/NVT/cache changes and preserve effective v0
  semantics during migration.
- Closure tests mutate every reference and coverage component, reject mixed
  source/visibility generations, and never consult an active scope pointer to
  repair a field closure.
- Restart tests ignore foreign boot sequences, accept only a validated
  same-HEAD disk base, and reconcile every source mismatch.
- NVT/GC tests prove stale hint generations do not pin posting closures and
  cannot return an unvalidated PageId.
- Manifest history tests prove A/B/task/request roots bound retention and no
  embedded predecessor or ambiguous logical checksum becomes authority.
- Limit/property tests reject oversize, noncanonical, duplicate, unsorted,
  unstable-JSON, native-endian, or trailing definition bytes before allocation.

---

## Round 6: Exact Immutable Manifest Layouts

**Status:** Approved. The permanent `DocumentStatePage` correction, all four
manifest layouts, validation/GC rules, codec IDs, and proof obligations are
closed.

This round freezes the four immutable manifest records. All offsets below are
relative to the start of the IndexArtifact **body**, after the approved 32-byte
`AIDX` envelope and the `owner_id || generation` identity.

Every manifest identity is exactly `H + 8` bytes:

| Identity offset | Size | Field | Rule |
| ---: | ---: | --- | --- |
| 0 | `H` | owner ID | `ScopeId`, `ValueStoreId`, or `IndexId` |
| `H` | 8 | generation | Nonzero `u64` little-endian |

The identity generation and common-envelope generation must be equal. The
immutable artifact key covers the complete stored value, including identity,
body, and trailing CRC, under the already-approved immutable v1 key domain.

### 1. Should the artifact registry add `DocumentStatePage = 0x0034`?

The exact-layout pass exposed one omission in the Round 3 registry.
Per-document extraction and strategy failures can be unbounded, must survive
restart, and must be distinguishable from a genuinely missing field. They
cannot safely live in:

- a manifest, because millions of document ordinals would violate its 1 MiB
  bound;
- `ScopeCatalogPage`, because extraction/strategy state is not shared scope
  identity and would couple every field update to the catalog;
- `ValuePage` alone, because strategy-specific expansion/converter failures do
  not belong to the strategy-neutral value store;
- `PostingPage`, because a document with no valid posting has no scalar-ordered
  position and a sentinel coordinate would corrupt ordering/hotspot behavior;
  or
- an in-memory overlay/journal, because this is durable generation state rather
  than transient mutation state.

**Recommendation:** Permanently add:

~~~text
0x0034  DocumentStatePage
~~~

to the IndexArtifact kind registry. The shared page codec will support two
owner classes:

~~~text
ValueStoreId owner  -> deterministic extraction/unindexable state
IndexId owner       -> deterministic strategy/converter/expansion state
~~~

Each state page is bounded, keyed by `DocumentOrdinal`, immutable, and reached
through its own `ArtifactDirectory` root in the corresponding manifest. Its
exact identity, status/reason registry, and page body will be frozen with the
other page artifacts before a writer exists.

Ordinary missing values require no state row: they are the live scope universe
minus documents with canonical values or unindexable state. A valid source
value that expands to zero strategy postings remains present in the value
store and is not mislabeled missing.

This is one additional logical artifact kind under the already-approved
`EntryType::IndexArtifact`/KV tag; it consumes no physical EntryType or KV type
and does not alter the common envelope.

Do you approve correcting the permanent registry with
`DocumentStatePage = 0x0034` and giving value/field manifests independent
bounded state roots?

**Your answer: Yes**

### 2. Do you approve these common manifest encodings?

The three correctness-bearing manifests begin with one exact shared prefix:

| Body offset | Size | Field | Rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `manifest_flags` | Zero in v1 |
| 4 | 32 | `required_reader_capabilities` | 256-bit little-indexed bitset |
| 36 | 4 | `definition_length` | Exact trailing definition bytes |
| 40 | `H` | `source_head_hash` | Nonzero namespace root hash |
| `40 + H` | 16 | `runtime_boot_id` | Nonzero raw boot ID |
| `56 + H` | 8 | `runtime_sequence` | `u64` little-endian; zero allowed |
| `64 + H` | ... | kind-specific fields | Defined below |

`FieldNvtManifest` is not correctness-bearing and therefore does not use this
coverage/definition prefix. It has its own exact fixed body in Question 6.

Freeze these physical codec IDs independently within each field's codec
namespace:

~~~text
0x0000  invalid / never emitted
0x0001  canonical uncompressed v1
~~~

Thus `posting_page_codec`, `value_page_codec`, `document_state_page_codec`,
`scope_page_codec`, `nvt_tile_codec`, and `artifact_directory_codec` are all
`u16` little-endian and equal `1` in v1. A future codec gets a permanent
nonzero ID plus a required capability; IDs are never reused.

Root presence is explicit rather than inferred from an all-zero hash:

- a clear presence bit requires the corresponding `H`-byte root slot to be all
  zero;
- a set presence bit requires a nonzero hash;
- unknown presence bits and nonzero reserved bytes are corruption; and
- an absent root means a canonical empty collection, not "unknown" or
  "failed to load."

All count and byte-total fields are `u64` little-endian. They describe the exact
immutable generation, are checked for arithmetic consistency, and are planner/
diagnostic metadata. They may guide costing and bounded traversal, but a query
still validates every page it actually consumes.

Do you approve this shared prefix, codec registry, empty-root encoding, and
count policy?

**Your answer: Yes**

### 3. Do you approve this exact `ScopeCatalogManifestV1` body?

Let `D` be the canonical `ScopeDefinitionV1` byte length:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `manifest_flags` | Zero |
| 4 | 32 | `required_reader_capabilities` | Checked bitset |
| 36 | 4 | `definition_length` | `D`, at most 64 KiB |
| 40 | `H` | `source_head_hash` | Coverage root |
| `40 + H` | 16 | `runtime_boot_id` | Coverage boot |
| `56 + H` | 8 | `runtime_sequence` | Coverage cutoff |
| `64 + H` | 2 | `scope_page_codec` | `1` |
| `66 + H` | 2 | `artifact_directory_codec` | `1` |
| `68 + H` | 1 | `root_presence` | Bit 0 ordinal map; bit 1 reverse map |
| `69 + H` | 3 | reserved | Zero |
| `72 + H` | 8 | `next_document_ordinal` | Nonzero; ordinal zero is reserved |
| `80 + H` | `H` | `ordinal_directory_root` | Ordinal to descriptor |
| `80 + 2H` | `H` | `reverse_directory_root` | FileKey to current ordinal |
| `80 + 3H` | 8 | `live_document_count` | Equals reverse live entries |
| `88 + 3H` | 8 | `retained_tombstone_count` | Retained ordinal tombstones |
| `96 + 3H` | 8 | `ordinal_page_count` | Reachable scope pages |
| `104 + 3H` | 8 | `reverse_page_count` | Reachable reverse pages |
| `112 + 3H` | `D` | `scope_definition` | Exact canonical bytes; consumes body |

The exact body length is `112 + 3H + D`. The complete stored manifest value is
`156 + 4H + D`: 284 + D bytes for a 32-byte hash, or 412 + D for a 64-byte
hash.

The two roots may be independently absent. A catalog that retains ordinal
tombstones can have an ordinal root while its reverse/live map is empty.
Canonical initial state has both roots absent, counts zero, and
`next_document_ordinal = 1`.

The ordinal directory contains every retained live/tombstoned descriptor. The
reverse directory contains only the current live FileKey-to-ordinal mapping.
`next_document_ordinal` must exceed every retained or historically allocated
ordinal and never decreases, even after tombstone compaction.

Decoding recomputes `ScopeId` from the definition and requires it to equal the
manifest identity owner.

Do you approve this exact scope-manifest layout and empty/tombstone invariants?

**Your answer: Yes**

### 4. Do you approve this exact `ValueStoreManifestV1` body?

Let `D` be the canonical `ValueStoreDefinitionV1` byte length:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `manifest_flags` | Zero |
| 4 | 32 | `required_reader_capabilities` | Checked bitset |
| 36 | 4 | `definition_length` | `D`, at most 512 KiB |
| 40 | `H` | `source_head_hash` | Coverage root |
| `40 + H` | 16 | `runtime_boot_id` | Coverage boot |
| `56 + H` | 8 | `runtime_sequence` | Coverage cutoff |
| `64 + H` | 2 | `value_page_codec` | `1` |
| `66 + H` | 2 | `document_state_page_codec` | `1` |
| `68 + H` | 2 | `artifact_directory_codec` | `1` |
| `70 + H` | 1 | `root_presence` | Bit 0 values; bit 1 document state |
| `71 + H` | 1 | reserved | Zero |
| `72 + H` | `H` | `scope_catalog_manifest` | Exact immutable artifact hash |
| `72 + 2H` | `H` | `value_directory_root` | Page directory or zero |
| `72 + 3H` | `H` | `document_state_directory_root` | Extraction state or zero |
| `72 + 4H` | 8 | `next_page_id` | Shared nonzero page-ID high water |
| `80 + 4H` | 8 | `value_page_count` | Reachable value pages |
| `88 + 4H` | 8 | `state_page_count` | Reachable document-state pages |
| `96 + 4H` | 8 | `value_document_count` | Documents with live source values |
| `104 + 4H` | 8 | `unindexable_document_count` | Deterministic extraction failures |
| `112 + 4H` | 8 | `live_value_count` | Canonical source values |
| `120 + 4H` | 8 | `value_tombstone_count` | Retained value tombstones |
| `128 + 4H` | 8 | `state_tombstone_count` | Retained state tombstones |
| `136 + 4H` | 8 | `live_canonical_value_bytes` | Sum of live value payload bytes |
| `144 + 4H` | `D` | `value_store_definition` | Exact canonical bytes; consumes body |

The exact body length is `144 + 4H + D`. The complete stored manifest value is
`188 + 5H + D`: 348 + D bytes for a 32-byte hash, or 508 + D for a 64-byte
hash.

`scope_catalog_manifest` is always nonzero and must resolve to a
`ScopeCatalogManifest` whose owner equals the definition's `ScopeId` and whose
coverage is byte-identical.

The value root may be absent only when value-page, value-document, live-value,
value-tombstone, and canonical-byte counts are zero. The state root may be
absent only when state-page, unindexable-document, and state-tombstone counts
are zero. Either root may exist independently. Deterministic unindexable state
lives only under the state root; it is never smuggled into the scope catalog.

`next_page_id` starts at 1, is shared across both page roles, exceeds every
allocated value/state page ID, never decreases, and is not reset when all pages
compact away.

Decoding recomputes `ValueStoreId` from the definition and requires it to equal
the manifest identity owner.

Do you approve this exact value-store-manifest layout and reference/count
invariants?

**Your answer: Yes**

### 5. Do you approve this exact `FieldIndexManifestV1` body?

Let `D` be the canonical `FieldIndexDefinitionV1` byte length:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `manifest_flags` | Zero |
| 4 | 32 | `required_reader_capabilities` | Checked bitset |
| 36 | 4 | `definition_length` | `D`, at most 256 KiB |
| 40 | `H` | `source_head_hash` | Coverage root |
| `40 + H` | 16 | `runtime_boot_id` | Coverage boot |
| `56 + H` | 8 | `runtime_sequence` | Coverage cutoff |
| `64 + H` | 2 | `posting_page_codec` | `1` |
| `66 + H` | 2 | `document_state_page_codec` | `1` |
| `68 + H` | 2 | `artifact_directory_codec` | `1` |
| `70 + H` | 1 | `root_presence` | Bit 0 postings; bit 1 document state |
| `71 + H` | 1 | reserved | Zero |
| `72 + H` | `H` | `value_store_manifest` | Exact immutable artifact hash |
| `72 + 2H` | `H` | `posting_directory_root` | Page directory or zero |
| `72 + 3H` | `H` | `document_state_directory_root` | Strategy state or zero |
| `72 + 4H` | 8 | `first_page_id` | Zero iff posting root absent |
| `80 + 4H` | 8 | `last_page_id` | Zero iff posting root absent |
| `88 + 4H` | 8 | `next_page_id` | Shared nonzero page-ID high water |
| `96 + 4H` | 8 | `posting_page_count` | Reachable posting pages |
| `104 + 4H` | 8 | `state_page_count` | Reachable document-state pages |
| `112 + 4H` | 8 | `live_posting_count` | Live postings |
| `120 + 4H` | 8 | `posting_tombstone_count` | Retained posting tombstones |
| `128 + 4H` | 8 | `posting_document_count` | Documents with live postings |
| `136 + 4H` | 8 | `unindexable_document_count` | Strategy-specific failures |
| `144 + 4H` | 8 | `state_tombstone_count` | Retained state tombstones |
| `152 + 4H` | 8 | `live_canonical_posting_bytes` | Live posting-value bytes |
| `160 + 4H` | `D` | `field_index_definition` | Exact canonical bytes; consumes body |

The exact body length is `160 + 4H + D`. The complete stored manifest value is
`204 + 5H + D`: 364 + D bytes for a 32-byte hash, or 524 + D for a 64-byte
hash.

`value_store_manifest` is always nonzero and must resolve to a
`ValueStoreManifest` whose owner equals the definition's `ValueStoreId` and
whose coverage is byte-identical.

When the posting root is absent, first/last IDs and every posting-page/posting/
posting-tombstone/posting-document/byte count are zero. When present, its
directory root and first/last page IDs are nonzero,
`posting_page_count > 0`, and page links from first to last must match the
directory exactly during full verification.

The state root may be absent only when state-page, unindexable-document, and
state-tombstone counts are zero. It may exist independently of the posting
root, so an index with no valid postings can still identify the exact documents
that require authoritative fallback.

`next_page_id` starts at 1, is shared across posting/state page roles, exceeds
every allocated page ID, never decreases, and is not reset by merge/compaction.

Decoding recomputes the converter fingerprint and `IndexId` from the definition
and requires both to match all referenced page/descriptor metadata and the
manifest identity owner.

Do you approve this exact field-index-manifest layout and chain/count
invariants?

**Your answer: Yes**

### 6. Do you approve this exact `FieldNvtManifestV1` body?

The NVT manifest has no semantic definition and no correctness coverage prefix:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `manifest_flags` | Zero |
| 4 | 32 | `required_reader_capabilities` | Checked bitset |
| 36 | 2 | `nvt_tile_codec` | `1` |
| 38 | 2 | `artifact_directory_codec` | `1` |
| 40 | 4 | `tile_cells` | Nonzero power of two |
| 44 | 1 | `root_presence` | Bit 0 tile directory |
| 45 | 3 | reserved | Zero |
| 48 | 8 | `resolution` | Nonzero logical NVT cell count |
| 56 | 8 | `basis_posting_generation` | Nonzero freshness hint |
| 64 | `H` | `basis_source_head_hash` | Nonzero freshness hint |
| `64 + H` | `H` | `tile_directory_root` | Tile directory or zero |
| `64 + 2H` | 8 | `tile_count` | Reachable nonempty tiles |
| `72 + 2H` | 8 | `populated_cell_count` | Populated hint cells |
| `80 + 2H` | 8 | `approximate_live_posting_count` | Cost hint only |

The exact body length is `88 + 2H`. The complete stored manifest value is
`132 + 3H`: 228 bytes for a 32-byte hash or 324 bytes for a 64-byte hash.

The identity owner is `IndexId`; the envelope/identity generation is the
independent nonzero hint generation. `basis_posting_generation` and
`basis_source_head_hash` are metadata, never ArtifactHash references or GC
edges.

`tile_cells <= resolution`. V1 additionally requires resolution to be exactly
divisible by `tile_cells`; partial terminal tiles are reserved for a future
codec. `tile_count <= ceil(resolution / tile_cells)` and
`populated_cell_count <= resolution`.

An absent tile root requires tile and populated-cell counts to be zero. A set
root requires both counts nonzero; empty tiles are never persisted. The
approximate posting count may be zero or stale in either state and is never a
correctness predicate.

Every hinted PageId is checked against the query-pinned FieldIndexManifest.
IndexId compatibility is necessary but never sufficient to trust a hint.

Do you approve this exact NVT-manifest layout and non-authoritative count/root
rules?

**Your answer: Yes**

### 7. Do you approve these decode, shallow-validation, and GC rules?

For all four manifests:

1. Validate EntityHeader, common envelope, identity, body length, CRC, immutable
   key recomputation, generation equality, flags, capability bits, codec IDs,
   reserved bytes, presence/hash canonicality, and checked count arithmetic
   before allocating a definition or following a root.
2. For correctness manifests, validate `definition_length` against the
   kind-specific cap and exact remaining body bytes, decode the definition
   canonically, and recompute the owner ID.
3. Pointer selection fully verifies the target manifest and exact referenced
   correctness manifests, then fully verifies each immediate directory-root
   node's key, kind, owner, generation, CRC, and internal bounds. It does not
   traverse every descendant page at startup.
4. A query fully verifies every directory node/page/tile it consumes and
   cross-checks page descriptors, owner, generation, converter fingerprint,
   bounds, and links against its pinned manifest.
5. Explicit verify, repair, migration cutover, compatible transfer, and sampled
   scrub traverse the complete closure and recompute counts/bounds.
6. Counts that disagree during full traversal corrupt/degrade that generation.
   They are never silently corrected in an immutable manifest.
7. GC traces only typed ArtifactHash fields: directory roots and exact
   correctness-manifest references. It does not treat source HEAD hashes,
   owner IDs, converter fingerprints, basis metadata, FileKeys, or arbitrary
   definition bytes as artifact edges.
8. Unknown capability/codec/schema state degrades only a referenced index
   closure according to the approved policy. Unreferenced orphan artifacts
   remain evidence/GC candidates and do not prevent ordinary database reads.

Complete stored manifest values retain the approved 1 MiB cap. Definitions are
read with bounded streaming/slices after fixed fields validate; no decoder
allocates the common 64 MiB artifact maximum for a manifest.

Do you approve these validation depths, count handling, and exact GC edge
rules?

**Your answer: Yes**

## Round 6 Proof Obligations

- Registry fixtures permanently assign `DocumentStatePage = 0x0034`, reject
  reassignment/unknown owner classes, and prove no EntryType/KV tag changed.
- Hand-authored fixtures cover all four manifests, empty and nonempty roots,
  minimum/maximum definitions, generations, counts, codec/capability fields,
  and both hash widths with exact lengths, keys, and CRCs.
- Offset tests prove every symbolic `H`/`D` boundary and complete-value formula
  for 32- and 64-byte databases.
- Mutation tests independently alter every fixed field, presence bit, reserved
  byte, root/reference hash, count, definition byte, identity, generation,
  capability bit, codec ID, and CRC.
- Empty/nonempty property tests prove canonical zero slots, root/count
  implications, first/last/next-ID rules, and no ordinal/page-ID reuse.
- Closure tests construct every wrong-kind, wrong-owner, wrong-generation,
  wrong-coverage, missing-root, mixed-definition, and cyclic reference and
  produce deterministic typed degradation.
- Lazy-read instrumentation proves pointer selection reads only manifests and
  immediate directory roots, while page/tile bodies remain bounded and lazy.
- Full verification recomputes every page/link/count/bound and cannot rewrite
  immutable disagreement in place.
- GC graph tests trace exactly the approved roots/references and prove source
  hashes, NVT basis metadata, definitions, and fingerprints cannot pin an
  unrelated artifact accidentally.
- Unsupported capability/codec tests distinguish active-closure degradation
  from harmless orphan presence.
- Cross-platform fixtures produce identical bytes, owner IDs, artifact keys,
  and CRCs on Linux, macOS, and Windows.

---

## Round 7: Canonical Definition Framing and ScopeDefinitionV1

**Status:** Approved. Common definition framing, canonical typed configuration
values, ScopeDefinitionV1, path/glob/FileKey semantics, scope resolution, and
migration/error boundaries are closed. Question 2 was approved after the
length clarification recorded below.

The exact-manifest pass is closed, but those manifests embed semantic
definitions whose bytes determine `ScopeId`, `ValueStoreId`,
`ConverterFingerprint`, and `IndexId`. We must freeze those bytes before a
writer exists.

The source audit exposed enough complexity that I do **not** recommend freezing
all four definitions as one enormous round:

- current source arrays mix object keys, array indexes, wildcards, and
  regex-looking strings;
- a regex-looking string with an invalid regex currently falls back to a
  literal key;
- parser selection can depend on an explicit plugin, a mutable global
  content-type registry, native JSON, or a native parser;
- plugin arguments currently pass through ordinary `serde_json`
  serialization;
- JSON null, an empty string, and some conversion failures currently collapse
  to the same empty byte sequence;
- legacy converters serialize their bounds little-endian but consume source
  numbers big-endian; and
- existing aliases and lossy numeric casts must migrate as their effective
  behavior, not be silently "cleaned up."

Trying to answer all of those while also checking exact offsets would invite a
plausible but internally inconsistent format. I recommend this bounded order:

1. common definition framing, canonical parameter values, and
   `ScopeDefinitionV1` in this round;
2. `ValueStoreDefinitionV1`, source selectors, parser resolution, and
   dependencies in the next round;
3. `ConverterDefinitionV1` and `FieldIndexDefinitionV1` after that; and
4. only then freeze the page/directory/state/journal bodies that consume those
   definitions.

This is still one persistent-format freeze. The decomposition only gives us a
chance to reject contradictions before they become disk bytes.

### 1. Do you approve this common canonical-definition envelope?

Every semantic definition begins with this exact 32-byte envelope. All
multibyte integers are little-endian:

| Definition offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `definition_magic` | Kind-specific ASCII magic |
| 4 | 2 | `definition_schema_version` | Exactly `1` |
| 6 | 2 | `definition_header_length` | Exactly `32` |
| 8 | 4 | `definition_total_length` | Complete definition bytes |
| 12 | 4 | `definition_flags` | Zero in v1 |
| 16 | 16 | reserved | All zero |
| 32 | ... | kind-specific body | Must consume total length exactly |

The permanent v1 magics are:

~~~text
ASCP  ScopeDefinitionV1
AVST  ValueStoreDefinitionV1
ACNV  ConverterDefinitionV1
AFIX  FieldIndexDefinitionV1
~~~

These are not EntityHeaders and do not receive another EntityHeader-sized
reserve. They are bounded semantic values embedded inside checksummed,
content-addressed manifests. The 16 reserved bytes allow additive header use
within a future schema, while an incompatible body change advances the
kind-specific definition schema and ID hash domain.

`definition_total_length` is validated against the enclosing manifest's exact
`definition_length` before body allocation. A definition has no independent
CRC: its bytes are already covered by the enclosing IndexArtifact CRC,
immutable artifact hash, and semantic owner-ID recomputation. Adding another
checksum would create no new failure boundary.

The manifest's `required_reader_capabilities` remains the single authority for
capabilities required by the complete closure. A definition does not duplicate
that bitset. Its permanent semantic IDs/tags determine which capability bits
the writer must place in the manifest.

Do you approve this exact 32-byte framing, magics, reserve, length authority,
and no duplicate checksum/capability fields?

**Your answer: Yes**

### 2. Do you approve this canonical typed-value subcodec?

Plugin/parser semantic arguments and future structured definition parameters
must not be hashed from ordinary JSON text, map iteration order, native
`usize`, or unstable `serde` layout.

**Recommendation:** Freeze `CanonicalConfigValueV1`. Every value is:

| Relative offset | Size | Field | Rule |
| ---: | ---: | --- | --- |
| 0 | 1 | `value_tag` | Permanent tag below |
| 1 | 4 | `payload_length` | Little-endian; exact bytes following |
| 5 | `payload_length` | payload | Tag-specific canonical payload |

Permanent v1 tags are:

~~~text
0x00  invalid / never emitted
0x01  null       payload length 0
0x02  false      payload length 0
0x03  true       payload length 0
0x04  i64        payload length 8; little-endian two's-complement
0x05  u64        payload length 8; little-endian
0x06  f64        payload length 8; little-endian IEEE-754 bits
0x07  utf8       raw UTF-8 bytes
0x08  bytes      arbitrary bytes
<!-- Note from user: How do we know the length of the utf8 and bytes types? shouldn't these also have a u32 "count"? -->
0x09  array      u32 count, then complete child values
0x0A  map        u32 count, then key/value entries
~~~

A map entry is exactly:

~~~text
key_length u32 LE
key_utf8[key_length]
CanonicalConfigValueV1 value
~~~

Map keys are unique and strictly increasing by their raw UTF-8 byte strings.
Arrays retain input order. Maps are sorted before encoding. Strings and keys
are valid UTF-8 but receive no implicit Unicode normalization or case folding;
those transformations occur only when an explicitly versioned semantic
operation requires them. This avoids treating two namespace/plugin strings as
equal when the current engine treats their bytes as different.

JSON-to-canonical-number rules are:

1. a negative integral token that fits `i64` becomes `i64`;
2. a nonnegative integral token through `i64::MAX` also becomes `i64`, so `1`
   has one representation;
3. a larger integral token through `u64::MAX` becomes `u64`;
4. a fractional or exponent token becomes finite `f64`;
5. negative zero is canonicalized to positive zero; and
6. overflow, NaN, infinity, and non-JSON numbers are rejected.

Thus `1`, `1.0`, and `"1"` remain observably different values, while alternate
integer spellings cannot create different IDs. Runtime plugin envelopes are
reconstructed from this typed tree; plugins may depend on JSON value semantics,
not the whitespace, member order, or numeric spelling of the original config.

The canonical config parser must detect duplicate object keys **before**
building a `serde_json::Value`; silently keeping the first or last duplicate is
forbidden. V1 bounds one value to 256 KiB, nesting to 32 containers, a container
to 65,535 members, and any one key/string/byte value to 64 KiB. Tighter
definition-specific budgets still apply.

Do you approve these exact tags, container order, numeric rules, Unicode
policy, duplicate-key rejection, and bounds?

**Your answer: I agree with all of this... but it appears to me that the "utf8" (string) and "bytes" types should have a "u32 count" that is currently missing... correct me if I am wrong.**

**Resolution:** The common `payload_length u32` at relative bytes 1-4 is already
the exact byte count for `utf8` and `bytes`. For example, an `utf8` value
containing five encoded bytes has `payload_length = 5` followed by those five
bytes. A second count inside that payload would duplicate the same fact and
would require a new rule for disagreeing lengths.

Arrays and maps additionally carry a logical child/member count because their
payload consists of multiple self-framed values and because the count is useful
for bounded preflight and exact validation. UTF-8 scalar or grapheme counts are
not wire lengths, are derivable when needed, and vary with Unicode semantics;
they are therefore not persisted. Question 2 is approved with this
clarification and its original wire layout is unchanged.

### 3. Do you approve this exact `ScopeDefinitionV1` body?

Let `P` be the normalized owner-path byte length and `G` the canonical glob
byte length. The body immediately after the 32-byte definition envelope is:

| Definition offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 32 | 4 | `owner_path_length` | `P`; nonzero |
| 36 | 4 | `glob_length` | `G`; zero only for direct mode |
| 40 | 2 | `membership_entity_class` | `1` in v1 |
| 42 | 2 | `matching_mode` | `1` direct or `2` relative glob |
| 44 | 2 | `path_normalization_semantics` | `1` |
| 46 | 2 | `path_case_semantics` | `1` |
| 48 | 2 | `glob_matcher_semantics` | `1` |
| 50 | 2 | `config_resolver_semantics` | `1` |
| 52 | 2 | `file_key_derivation_semantics` | `1` |
| 54 | 2 | `internal_path_policy` | `1` |
| 56 | 8 | reserved | Zero |
| 64 | `P` | `owner_path` | Canonical absolute UTF-8 path |
| `64 + P` | `G` | `glob` | Canonical relative glob or absent |

The complete definition length is exactly `64 + P + G`, with no terminators or
trailing bytes. The already-approved ScopeDefinition limit remains 64 KiB, so
`P + G <= 65,472`. This resolves a small ambiguity in the earlier wording:
"owner path up to 64 KiB" is subject to the definition's total budget; the
64-byte framing cannot be ignored. Direct mode requires `G = 0`. Glob mode
requires `G > 0`.

Permanent enum IDs are:

~~~text
membership_entity_class:
  0 invalid
  1 regular_files

matching_mode:
  0 invalid
  1 direct_children
  2 relative_path_glob

all six semantic-version fields:
  0 invalid
  1 exact v1 behavior defined in Question 4
~~~

The definition contains no configured field list, parser, logging flag,
runtime root, matching-file list, physical codec, or hash-algorithm enum. The
database-selected hash algorithm is already the algorithm used for every
`H(...)` operation and determines the width of both `ScopeId` and FileKey.

Do you approve this exact body, offsets, enum IDs, combined variable-byte
budget, and exclusions?

**Your answer: Yes**

### 4. Do you approve these exact v1 path, glob, FileKey, and exclusion semantics?

The six `= 1` semantics in Question 3 mean:

**Path normalization v1**

1. Remove every NUL byte, matching the current public normalizer.
2. Trim leading/trailing Rust Unicode whitespace from the complete string.
3. Split on `/` and discard empty segments.
4. Discard `.` segments.
5. Resolve `..` by removing the preceding retained segment; at root it is a
   no-op.
6. Emit `/` for no retained segments, otherwise `/` plus segments joined by one
   `/`.

The Scope compiler normalizes first and stores only the result. Decoding
normalizes again and requires byte equality, which rejects a noncanonical
stored definition. A future stricter policy can reject NUL rather than remove
it, but that is a new semantic version and ScopeId.

**Path case v1**

Paths, field names, and glob literals are case-sensitive exact UTF-8 bytes.
There is no OS-dependent case folding, Unicode normalization, or locale.

**Glob matcher v1**

- Both pattern and relative candidate are split on `/`; empty segments are
  ignored.
- A complete `**` segment matches zero or more complete path segments.
- `*` matches zero or more bytes within one segment.
- `?` matches exactly one byte within one segment.
- Every other byte is literal. V1 has no escape syntax or character classes.
- Matching is case-sensitive.

The compiler canonicalizes a glob by removing empty segments and joining the
remaining segments with `/`. It rejects an empty result, absolute meaning
(leading `/` is removed as semantically irrelevant), `.`/`..` segments, NUL,
and invalid UTF-8. It does not rewrite repeated `*`, because doing so would
complicate future syntax evolution for negligible benefit.

The explicit byte-oriented `?` rule preserves current behavior for non-ASCII
UTF-8. A future Unicode-scalar glob is a new matcher semantic version; it must
not change existing ScopeIds in place.

**FileKey derivation v1**

~~~text
FileKey = H(ASCII "file:" || canonical absolute path UTF-8)
~~~

There is no separator after `file:` because the canonical path always begins
with `/`, preserving the current key domain exactly.

**Internal-path policy v1**

A regular file is excluded from every index scope when:

- its first canonical path segment is `.aeordb-system`; or
- any canonical path segment is `.aeordb-config`, `.aeordb-indexes`, or
  `.aeordb-logs`.

This predicate is frozen. A future engine-owned namespace does not silently
join the list; adding one advances this policy version and reconciles affected
scopes. User-visible metadata files such as `.aeordb-permissions` are not
implicitly excluded by v1.

Do you approve these exact semantic algorithms, including preservation of the
current byte-oriented/NUL behavior and the explicit internal-path set?

**Your answer: Yes**

### 5. Do you approve these exact scope winner and equivalence rules?

`config_resolver_semantics = 1` preserves the current effective-config winner
model, made explicit:

1. Normalize the candidate file path and reject it under the internal-path
   policy.
2. Inspect its immediate parent.
3. An immediate-parent config with no glob wins for every direct regular-file
   child.
4. An immediate-parent config with a glob wins only when that glob matches the
   filename.
5. If no immediate config wins, walk ancestors from nearest to root.
6. Only ancestor configs with a glob are eligible; match each against the
   candidate path relative to that config's owner directory.
7. The nearest matching ancestor wins. At most one config exists at each owner
   directory, so no same-depth tie breaker exists.
8. An ancestor config without a glob does not apply recursively.
9. The file belongs to the winning config's ScopeId and no other effective
   scope.

Scope enumeration and incremental write/move/delete handling must call one
shared resolver implementing this algorithm. They may not each approximate it.
Adding, removing, or changing a child config causes reconciliation of affected
membership but does not mutate either unchanged definition.

Equivalent user spellings compile to identical bytes:

- owner paths normalize before encoding;
- a no-glob config becomes direct mode;
- glob empty segments and a leading/trailing `/` are removed;
- aliases outside the scope definition do not affect ScopeId; and
- defaults are materialized as their permanent IDs rather than encoded as
  "missing."

The exact `ScopeId` remains:

~~~text
H(ASCII "aeordb.index.scope-definition.v1\0" ||
  complete canonical ScopeDefinitionV1)
~~~

Do you approve this one-winner resolver, direct/recursive boundary, canonical
equivalence, and ID input?

**Your answer: Yes**

### 6. Do you approve this malformed-input, migration, and freeze policy?

- User configuration is compiled into typed definitions. Invalid config
  produces a path/config-specific validation error; it never emits a partial
  definition or silently skips an invalid scope.
- Stored definitions with wrong magic/version/header length/total length,
  nonzero flags/reserve, invalid UTF-8, noncanonical path/glob, unknown enum,
  zero/oversize length, or trailing bytes fail deterministically.
- A well-framed definition requiring a known-but-unavailable semantic
  capability reports `UnsupportedIndexCapability` and degrades only that index
  closure. Structurally impossible/noncanonical bytes report corruption.
- No decoder guesses a schema from length, host architecture, current config,
  or installed plugin.
- V0 migration compiles the **effective** legacy behavior into explicit
  canonical definitions. When corrected v1 semantics differ, migration uses a
  named legacy adapter semantic ID; adopting corrected behavior requires a
  separate shadow build.
- Scope definitions that already have exactly the approved v1 membership
  behavior need no artificial legacy wrapper. Their canonical identity is
  derived from behavior, not from the old config's textual spelling.
- No persistent definition writer lands until hand-authored Linux/macOS/
  Windows fixtures and independent decoder tests agree on every byte and ID.

Do you approve this error boundary, behavioral migration rule, and
golden-fixture gate?

**Your answer: Yes**

## Round 7 Proof Obligations

- Hand-authored fixtures cover all four definition magics/envelopes even though
  only ScopeDefinition's body is frozen in this round.
- Length/property tests reject every truncated, overlong, trailing, nonzero
  reserved, wrong-magic, wrong-version, and allocation-amplifying input before
  variable allocation.
- Canonical-config-value fixtures cover every tag, integer boundary, finite
  float edge, signed zero, nested array/map, UTF-8 key ordering, duplicate key,
  depth/member/byte bound, and both native endiannesses.
- Scope fixtures cover root/non-root owners, maximum combined bytes, direct and
  glob modes, every enum, normalization aliases, non-ASCII case/`?` behavior,
  internal exclusions, and exact 32-/64-byte ScopeIds.
- Resolver model tests compare full enumeration and every incremental
  store/move/copy/delete/config-change path against one reference resolver.
- Child-config tests prove membership moves between scope lineages while
  unchanged definition bytes and never-reused ordinals remain stable.
- Migration fixtures compile representative v0 path/glob behavior into
  deterministic v1 definitions and require an explicit adapter wherever
  behavior differs.

---

## Round 8: ValueStoreDefinitionV1 and SourceSelectorV1

**Status:** Self-decided by Codex on 2026-07-31 after a `planning-cap` and
AGIS adversarial pass. The original recommendations remain below as the audit
record. None are approved exactly as written; the binding corrected contract is
Round 8A.

Round 7 froze the common definition envelope and the document universe. This
round freezes the `ValueStoreDefinitionV1` fixed body and the selector that
turns one document into candidate source values.

The parser-resolution plan and dependency-table bodies are length-delimited
children referenced here, but their internal records are deliberately deferred
to Round 9. Parser registry snapshots, native semantic dependencies, plugin
IDs/versions/checksums, MIME normalization, and historical dependency
availability need one focused consistency pass. This round defines exactly
where those children live and what the rest of the ValueStore owns.

### 1. Do you approve this exact `ValueStoreDefinitionV1` body?

Let:

~~~text
H = database hash width
F = canonical query-field-name byte length
S = complete SourceSelectorV1 byte length
P = complete ParserResolutionPlanV1 byte length
D = complete DependencyTableV1 byte length
~~~

After the approved 32-byte `AVST` definition envelope:

| Definition offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 32 | `H` | `scope_id` | Nonzero approved ScopeDefinition ID |
| `32 + H` | 4 | `field_name_length` | `F`; 1..4096 |
| `36 + H` | 4 | `source_selector_length` | `S`; complete selector |
| `40 + H` | 4 | `parser_plan_length` | `P`; complete plan, including `none` |
| `44 + H` | 4 | `dependency_table_length` | `D`; complete table, including empty |
| `48 + H` | 2 | `source_value_codec` | `1` |
| `50 + H` | 2 | `metadata_source_semantics` | `1` |
| `52 + H` | 2 | `source_selector_semantics` | `1` |
| `54 + H` | 2 | `parser_resolution_semantics` | `1` |
| `56 + H` | 2 | `missing_semantics` | `1` |
| `58 + H` | 2 | `null_semantics` | `1` |
| `60 + H` | 2 | `extraction_error_semantics` | `1` |
| `62 + H` | 2 | `multi_value_ordering` | `1` |
| `64 + H` | 2 | `duplicate_value_semantics` | `1` |
| `66 + H` | 2 | `unindexable_semantics` | `1` |
| `68 + H` | 4 | `max_source_values_per_document` | Nonzero |
| `72 + H` | 8 | reserved | Zero |
| `80 + H` | 8 | `max_canonical_source_bytes_per_document` | Nonzero |
| `88 + H` | 8 | `max_document_input_bytes` | Rule below |
| `96 + H` | 8 | `parser_memory_bytes` | Rule below |
| `104 + H` | 8 | `parser_output_bytes` | Rule below |
| `112 + H` | 8 | `parser_fuel` | Rule below |
| `120 + H` | 8 | `mapper_memory_bytes` | Rule below |
| `128 + H` | 8 | `mapper_output_bytes` | Rule below |
| `136 + H` | 8 | `mapper_fuel` | Rule below |
| `144 + H` | `F` | `field_name` | Canonical UTF-8 |
| `144 + H + F` | `S` | `source_selector` | Exact nested record |
| `144 + H + F + S` | `P` | `parser_plan` | Exact nested record |
| `144 + H + F + S + P` | `D` | `dependency_table` | Exact nested record |

The complete definition length is exactly:

~~~text
144 + H + F + S + P + D
~~~

It remains bounded by the approved 512 KiB ValueStoreDefinition cap. Nested
children are always present as self-framed records: metadata extraction uses a
canonical `none` parser plan, and a definition without dependencies uses a
canonical empty dependency table. Zero-length ambiguity is not permitted.

Recommended child bounds are:

~~~text
field name          <=   4 KiB
source selector     <=  64 KiB
parser plan         <= 128 KiB
dependency table    <= 256 KiB
complete definition <= 512 KiB
~~~

The complete bound remains authoritative, so maximum children cannot all occur
simultaneously.

All ten semantic/codec IDs are permanent `u16` namespaces: zero is invalid and
one means the exact behavior frozen by this and the next round. Future
semantics use a new nonzero ID and capability; they do not reinterpret `1`.

Do you approve this exact body, variable order, always-present children,
semantic-ID fields, and bounds?

**DECIDED (self, 2026-07-31): No, not exactly as written.** The parent body
duplicates parser/mapper invocation limits that must be owned by the concrete
invocation records inside the parser plan and mapper selector. It also lacks
bounded JSON-path work limits. Round 8A Question 1 replaces the fixed body and
length formula while preserving the approved transitive ValueStore ownership.

### 2. Do you approve canonical field names and metadata source IDs?

Field names are exact UTF-8 query identifiers, 1..4096 bytes, with no NUL.
Ordinary field names are case-sensitive and receive no Unicode normalization.
The default source for an ordinary field is compiled to an explicit one-segment
JSON path using that exact field name. Therefore omitted source and
`source: ["same-field-name"]` produce the same ValueStoreId.

Metadata aliases compile to these permanent canonical names and selector IDs:

| Metadata ID | Canonical field | Accepted config spelling | Canonical source value |
| ---: | --- | --- | --- |
| 1 | `@path` | `@path` | canonical absolute path as UTF-8 |
| 2 | `@filename` | `@filename`, `@file_name` | final path segment as UTF-8 |
| 3 | `@extension` | `@extension` | suffix after final dot as UTF-8; empty if absent |
| 4 | `@content_type` | `@content_type` | stored content type as UTF-8, or null if absent |
| 5 | `@size` | `@size` | `u64` logical byte length |
| 6 | `@created_at` | `@created_at` | `u64` Unix milliseconds |
| 7 | `@updated_at` | `@updated_at` | `u64` Unix milliseconds |
| 8 | `@hash` | `@hash` | raw complete content hash as `bytes` |

`@hash` is the full file content hash stored at write/commit time, not the
first chunk hash and not a re-read of file content. Raw bytes avoid storing two
hex characters per hash byte. Query literal handling in the later
FieldIndexDefinition converts the documented hexadecimal API value into those
raw bytes before equality lookup.

The extension rule preserves case and uses the current final-dot behavior:
`file.tar.gz -> "gz"`, `file -> ""`, and `.profile -> "profile"`.
Content-type normalization for parser selection is independent; the metadata
field exposes the stored value. Absent content type is typed null rather than
colliding with an explicitly stored empty string.

Any unknown `@`-prefixed field is a configuration error. It is not silently
ignored. A metadata selector requires the field's canonical name and metadata
ID to agree; disagreement is noncanonical/corrupt.

Do you approve these names, aliases, permanent metadata IDs, typed values, and
unknown-field behavior?

**DECIDED (self, 2026-07-31): No, not exactly as written.** `FileRecord`
persists `created_at` and `updated_at` as signed `i64`, not `u64`. The v1 source
types must reflect that fact. Round 8A Question 2 also defines the missing
full-content-hash migration gate and separates corrected metadata semantics
from explicit legacy adapters.

### 3. Do you approve this exact `SourceSelectorV1` framing and kinds?

Every source selector has this exact 32-byte header:

| Selector offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 2 | `selector_schema_version` | `1` |
| 2 | 2 | `selector_kind` | Permanent kind below |
| 4 | 4 | `selector_total_length` | Complete selector bytes |
| 8 | 4 | `selector_flags` | Zero |
| 12 | 4 | `item_count` | Kind-specific |
| 16 | 2 | `regex_semantics` | Kind-specific |
| 18 | 2 | `mapper_contract` | Kind-specific |
| 20 | 12 | reserved | Zero |
| 32 | ... | payload | Must consume total length |

Permanent kinds are:

~~~text
0  invalid / never emitted
1  metadata
2  json_path
3  plugin_mapper
~~~

Canonical kind rules are:

- `metadata`: `item_count = 0`, regex/mapper semantics zero, and an exact
  eight-byte payload:

  ~~~text
  metadata_id u16 LE
  metadata_value_semantics u16 LE = 1
  reserved u32 = 0
  ~~~

  Its total length is exactly 40 bytes.

- `json_path`: `item_count` equals the number of complete path segments,
  `regex_semantics = 1`, `mapper_contract = 0`, and the payload is exactly the
  concatenated segment records from Question 4. A zero-segment path is valid
  and selects the complete parsed document.

- `plugin_mapper`: `item_count = 0`, `regex_semantics = 0`,
  `mapper_contract = 1`, and its payload is defined in Question 5.

The selector kind must agree with the field and parser plan:

- a metadata field uses exactly the matching metadata selector and a `none`
  parser plan;
- an ordinary field uses JSON path or plugin mapper and a non-`none` parser
  plan; and
- plugin dependencies referenced by a selector must exist in the same
  ValueStore dependency table.

Do you approve this exact selector header, permanent kinds, canonical metadata
payload, root-document path, and cross-field invariants?

**DECIDED (self, 2026-07-31): No, not exactly as written.** The framing is
sound, but a permanently always-empty migrated selector should be one canonical
selector kind rather than a path segment that can appear in many semantically
equivalent paths. The mapper contract also needs separate legacy-single-byte
and corrected typed-plural IDs. Round 8A Question 3 is binding.

### 4. Do you approve this exact JSON-path segment codec and traversal?

Each JSON-path segment is:

| Segment offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 1 | `segment_tag` | Permanent tag below |
| 1 | 1 | `segment_flags` | Tag-specific |
| 2 | 2 | reserved | Zero |
| 4 | 4 | `segment_payload_length` | Exact payload bytes |
| 8 | ... | payload | Tag-specific |

Permanent tags are:

~~~text
0  invalid
1  object_key       payload is raw UTF-8 key bytes; flags zero
2  numeric_index    payload is u64 LE; flags zero
3  fan_out          payload empty; flags zero
4  regex            payload is raw UTF-8 pattern body;
                    flags bit 0 = case-insensitive, all others zero
5  never_match_v0   payload empty; flags zero; migration compiler only
~~~

The config compiler preserves current effective source-array behavior:

- a nonempty ordinary string becomes `object_key`;
- an empty string becomes `fan_out`;
- a nonnegative integral JSON number becomes `numeric_index`;
- a string with current `/pattern/flags` syntax becomes `regex` only if the
  pattern compiles; its last `/` separates the body from flags, `i` enables
  case-insensitive matching, and currently ignored flags are discarded;
- a malformed or uncompilable regex-looking string remains an exact
  `object_key`; and
- a legacy negative/fractional number, bool, null, array, or object segment
  becomes `never_match_v0`, preserving its existing always-empty result.

New configuration rejects values requiring `never_match_v0`; only migration of
an already-effective legacy config may emit it.

Traversal semantics are:

1. Begin with the parsed document as one candidate.
2. `object_key` performs exact key lookup.
3. `numeric_index` indexes an array, or looks up the canonical unsigned-decimal
   number as an object key.
4. `fan_out` visits array elements in array order or object values in strictly
   increasing raw UTF-8 key order.
5. `regex` filters object keys in that same key order. For arrays it filters
   elements by canonical compact JSON text: strings use their string contents;
   numbers use canonical JSON number text; bool/null use JSON literals; and
   arrays/maps use recursively canonical compact JSON with sorted map keys.
6. After each segment, candidate groups are concatenated in prior-candidate
   order. An empty candidate set remains empty.
7. Terminal values retain this exact traversal order for value ordinals.

`regex_semantics = 1` names AeorDB's frozen Rust-regex-compatible syntax and
matching contract, not "whatever the newest regex crate does." A crate upgrade
that changes observable accepted patterns or matches must preserve this
contract or introduce a new semantic ID.

Do you approve these segment bytes, legacy compilation, deterministic object
ordering/stringification, and traversal rules?

**DECIDED (self, 2026-07-31): No, not exactly as written.** The traversal and
ordering direction is correct, but the `never_match_v0` segment creates
noncanonical equivalent paths, and the proposal permits fan-out/regex work to
grow without a semantic work bound. Round 8A Question 4 removes that segment,
defines canonical always-missing compilation, and freezes bounded streaming
work accounting.

### 5. Do you approve this exact plugin-mapper selector?

A `plugin_mapper` selector payload is:

| Payload offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `dependency_ordinal` | Nonzero one-based dependency-table ordinal |
| 4 | 4 | `arguments_length` | `A`; complete CanonicalConfigValueV1 |
| 8 | `A` | `arguments` | Null when omitted |

Its selector total length is exactly `40 + A`. The dependency ordinal is
resolved only after the dependency table has validated and must name a plugin
with mapper capability. Dependency-table records are sorted canonically in
Round 9, so the one-based ordinal is deterministic; zero always means "no
dependency" in records that permit absence.

`mapper_contract = 1` means:

1. Input is the complete parsed JSON document plus the canonical arguments,
   reconstructed into the documented mapper JSON envelope.
2. Output is one arbitrary byte string.
3. A zero-byte output is a present `bytes` source value, not missing.
4. The mapper does not implicitly parse its output as JSON or fan it into
   multiple values.
5. A future typed or multi-value mapper contract receives another permanent
   ID.

The selector stores no mutable plugin path alias. The dependency record freezes
the resolved plugin ID, version, checksum, and ABI; runtime must locate that
exact dependency rather than invoke whichever plugin later occupies the
original path.

An unavailable dependency, request cancellation, wall-clock timeout, or host
I/O failure is an incomplete/retryable build condition and does not create a
durable document `unindexable` row. Deterministic mapper rejection, fuel
exhaustion, memory/output limit, or invalid contract result uses the approved
document-state policy.

Do you approve this exact payload, one-value bytes contract, exact-dependency
binding, and transient/deterministic failure boundary?

**DECIDED (self, 2026-07-31): No, not exactly as written.** A new
strategy-neutral typed value store must not freeze the current mapper's
single-raw-byte limitation as its corrected default. The selector also needs
to embed the invocation policy that controls that exact mapper call. Round 8A
Question 5 retains the current behavior only as a migration contract and makes
ordered typed plurality the corrected contract.

### 6. Do you approve these source-value, limit, and failure semantics?

The ten `= 1` semantics in Question 1 mean:

- `source_value_codec = 1`: typed null/bool/i64/u64/f64/UTF-8/bytes/array/map
  values use the approved canonical structural rules from Round 7, but with
  ValueStore limits rather than the smaller config-value limit. Map keys are
  sorted raw UTF-8 and duplicates are impossible after parsing.
- `missing_semantics = 1`: no selector result means no value row and no error
  row. Missing remains distinguishable as the scope universe minus documents
  with values or extraction state.
- `null_semantics = 1`: JSON null is one present typed null value. It never
  aliases missing, empty UTF-8, or empty bytes.
- `extraction_error_semantics = 1`: only deterministic extraction failures
  create a ValueStore-owned DocumentState row with a permanent reason.
- `multi_value_ordering = 1`: preserve the deterministic selector traversal
  order and assign zero-based value ordinals.
- `duplicate_value_semantics = 1`: preserve duplicate source values and their
  distinct value ordinals. A strategy may deduplicate expanded postings under
  its separately versioned FieldIndex semantics.
- `unindexable_semantics = 1`: exceeding a semantic input/value/byte/memory/
  output/fuel bound marks the **whole document** unindexable for that
  ValueStore generation. It never truncates values and silently creates false
  negatives.

`max_canonical_source_bytes_per_document` counts complete canonical value
records, including their tags/lengths/containers, not just user payload bytes.

Resource fields use:

~~~text
0         not applicable for this selector/plan
u64::MAX  explicitly unlimited legacy behavior
other     exact finite semantic limit
~~~

New corrected-v1 definitions use finite nonzero source-value, source-byte, and
document-input limits. An explicit migration adapter may use `u32::MAX` source
values and `u64::MAX` source bytes/document input to preserve legacy unbounded
behavior; it remains a distinct semantic identity and is not the corrected-v1
default.

Metadata selectors require all parser/mapper resource fields to be zero.
Plugin parser/mapper memory, output, and fuel values are nonzero when that role
is present. Native parsers use zero for inapplicable WASM memory/fuel fields,
but `parser_output_bytes` remains finite and applicable to their parsed output.

**Correction to Round 5:** wall-clock timeout is not stored as a semantic limit
and cannot create durable unindexable state. Identical bytes indexed on a busy
and idle machine must not produce different durable contents. Wall-clock
timeouts, cancellation, process shutdown, dependency absence, and transient
I/O/OOM are operational failures: keep the document/generation incomplete,
retry with bounded backoff, and do not publish coverage until resolved.

For sandboxed WASM, deterministic fuel and declared linear-memory/output limits
are semantic because identical input and implementation exhaust them at the
same point. For native parsers, deterministic input/output/structural limits
are semantic; scheduling time is not.

Initial user-facing defaults will be frozen in the later configuration/API
round. Persistent definitions never encode "default" or "missing"; the compiler
materializes concrete values before deriving ValueStoreId.

Do you approve these typed-value distinctions, ordering/duplicate policy,
whole-document over-limit behavior, resource encodings, and the wall-clock
timeout correction?

**DECIDED (self, 2026-07-31): No, not exactly as written.** The typed
missing/null/order/duplicate and all-or-nothing rules are approved, as is the
wall-clock correction. Invocation limits move to concrete invocation policies;
fuel is durable semantics only behind a frozen execution-meter capability; and
generic host OOM remains operational rather than document-semantic. Round 8A
Question 6 is binding.

## Round 8 Proof Obligations

- Exact-offset fixtures cover 32-/64-byte hashes, minimum children, every
  semantic field, maximum legal combined size, and the
  `144 + H + F + S + P + D` formula.
- Field fixtures prove ordinary/default-source equivalence, metadata alias
  equivalence, unknown metadata rejection, typed absent content type, and full
  raw content hash extraction without reading chunks.
- Selector fixtures cover all kinds, zero-segment root selection, every segment
  tag, canonical regex spellings, invalid-regex key fallback, migration-only
  never-match, nested fan-out, and Unicode byte-order traversal.
- A reference traversal model compares selector results and value ordinals
  across JSON map implementations and Linux/macOS/Windows.
- Mapper fixtures bind exact dependency ordinals and arguments, distinguish
  missing from empty bytes, and reject alias substitution, wrong role, or
  dependency drift.
- Failure injection distinguishes every deterministic state reason from
  dependency absence, cancellation, timeout, I/O, OOM, and shutdown; no
  transient condition publishes false coverage or durable unindexable state.
- Limit tests exercise exact boundary and boundary-plus-one input, value,
  canonical-byte, memory, output, and fuel cases and prove all-or-nothing
  document publication.
- Migration fixtures preserve legacy source-array no-match/regex behavior and
  typed-value collisions only through explicit legacy semantics rather than
  contaminating corrected v1 definitions.

## Round 8A: Codex Self-Decision and Corrected Contract

**Status:** Closed at the ValueStore/selector semantic boundary. Round 9 must
freeze the complete parser plan, dependency records, invocation-policy body,
and plugin request/result ABI before any persistent writer may land.

I did not self-approve the six drafts above. A source audit found several
places where they froze an implementation accident or assigned one fact to two
owners:

- `FileRecord.created_at` and `updated_at` are signed `i64` values, so the
  proposed unsigned metadata types were wrong.
- `ParserResolutionPlanV1` may resolve more than one concrete parser. One
  parent-level parser memory/output/fuel triple cannot describe each
  invocation without becoming a second authority.
- current mapper output is one raw byte string, but the approved ValueStore is
  typed and plural; making the legacy limitation contract `1` for new indexes
  would force an immediate second persistent format.
- a `never_match_v0` path segment permits many byte-distinct selectors with
  the same always-missing behavior.
- current fan-out builds intermediate vectors and has no bound on work that is
  later filtered away; terminal value/byte limits alone do not bound it.
- the current WASM memory limit is not yet proof of a portable semantic limit:
  the runtime creates a bounded imported memory but can select a module's own
  exported memory without a store resource limiter, and fuel classification is
  partly based on trap-message text.

Those are engineering decisions, not product-policy questions, so I am
resolving them here rather than escalating them to Wyatt.

### 1. Corrected `ValueStoreDefinitionV1` body

**DECIDED (self):** Invocation-specific limits live exactly once, beside the
invocation they govern. ValueStore still owns them transitively because the
complete parser plan and mapper selector are embedded in its hashed bytes.
The fixed body owns only limits common to complete document extraction and
bounded selector traversal.

Let `H`, `F`, `S`, `P`, and `D` retain their definitions from Round 8. After
the 32-byte `AVST` envelope, the binding body is:

| Definition offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 32 | `H` | `scope_id` | Nonzero approved ScopeDefinition ID |
| `32 + H` | 4 | `field_name_length` | `F`; 1..4096 |
| `36 + H` | 4 | `source_selector_length` | `S`; complete selector |
| `40 + H` | 4 | `parser_plan_length` | `P`; complete plan, including `none` |
| `44 + H` | 4 | `dependency_table_length` | `D`; complete table, including empty |
| `48 + H` | 2 | `source_value_codec` | Known compatibility-table ID |
| `50 + H` | 2 | `metadata_source_semantics` | Zero when inapplicable; otherwise known ID |
| `52 + H` | 2 | `source_selector_semantics` | Known nonzero ID |
| `54 + H` | 2 | `parser_resolution_semantics` | Known nonzero ID |
| `56 + H` | 2 | `missing_semantics` | Known nonzero ID |
| `58 + H` | 2 | `null_semantics` | Known nonzero ID |
| `60 + H` | 2 | `extraction_error_semantics` | Known nonzero ID |
| `62 + H` | 2 | `multi_value_ordering` | Known nonzero ID |
| `64 + H` | 2 | `duplicate_value_semantics` | Known nonzero ID |
| `66 + H` | 2 | `unindexable_semantics` | Known nonzero ID |
| `68 + H` | 4 | `max_source_values_per_document` | Nonzero |
| `72 + H` | 8 | reserved | Zero |
| `80 + H` | 8 | `max_canonical_source_bytes_per_document` | Nonzero |
| `88 + H` | 8 | `max_document_input_bytes` | Context rule below |
| `96 + H` | 8 | `max_selector_work_items_per_document` | Context rule below |
| `104 + H` | 8 | `max_selector_examined_bytes_per_document` | Context rule below |
| `112 + H` | `F` | `field_name` | Canonical UTF-8 |
| `112 + H + F` | `S` | `source_selector` | Exact nested record |
| `112 + H + F + S` | `P` | `parser_plan` | Exact nested record |
| `112 + H + F + S + P` | `D` | `dependency_table` | Exact nested record |

The complete length is exactly:

~~~text
112 + H + F + S + P + D
~~~

The existing child caps remain 4/64/128/256 KiB and the complete definition
cap remains 512 KiB. All four child maxima actually fit simultaneously beneath
that cap, so the earlier claim that they could not coexist was false. The
complete cap remains independently authoritative.

Every concrete parser candidate carries one exact `InvocationPolicyV1` inside
`ParserResolutionPlanV1`. A plugin mapper carries one inside its selector.
Dependency records identify executable code and ABI; they do not own call-site
limits because one exact dependency may legitimately be invoked under two
different semantic policies.

### 2. Corrected canonical fields and metadata values

**DECIDED (self):** Keep the canonical names, aliases, IDs, extension rule,
unknown-`@` rejection, and raw full-content-hash direction. Correct the two
timestamp types and make legacy metadata behavior explicit rather than
silently changing migrated query semantics.

`metadata_source_semantics = 1` is the corrected typed contract:

| ID | Canonical field | Accepted spelling | Canonical source value |
| ---: | --- | --- | --- |
| 1 | `@path` | `@path` | canonical absolute path as UTF-8 |
| 2 | `@filename` | `@filename`, `@file_name` | final path segment as UTF-8 |
| 3 | `@extension` | `@extension` | final-dot suffix as UTF-8; empty when absent |
| 4 | `@content_type` | `@content_type` | stored content type as UTF-8, or typed null when absent |
| 5 | `@size` | `@size` | `u64` logical byte length |
| 6 | `@created_at` | `@created_at` | `i64` Unix milliseconds |
| 7 | `@updated_at` | `@updated_at` | `i64` Unix milliseconds |
| 8 | `@hash` | `@hash` | exactly `H` raw full-content-hash bytes |

`metadata_source_semantics = 2` is migration-only `legacy_bytes_v0`. It emits
the current effective v0 byte inputs: UTF-8 fields as raw bytes, absent content
type as empty bytes, `u64`/`i64` numerics in big-endian bytes, and `@hash` as
lowercase ASCII hexadecimal of the **full** content hash. It never resurrects
the ratified first-chunk-hash defect.

A FileRecord with an absent or wrong-width full content hash cannot enter a v1
`@hash` generation. Preflight reports `file_record_migration_required`; the
separate versioned FileRecord migration must complete first. The indexer must
not stream the file, derive a hash from only the first chunk, or rewrite the
authoritative FileRecord as a hidden side effect of index-format migration.
This preserves the formal plan's no-FileRecord-rewrite boundary while still
requiring correct full hashes.

Ordinary field names remain exact case-sensitive UTF-8 without normalization
or NUL. Omitted source still compiles to the same one-key selector as an
explicit source containing that field name.

### 3. Corrected `SourceSelectorV1` kinds

**DECIDED (self):** Keep the 32-byte selector header, with these permanent
kinds and canonical rules:

~~~text
0  invalid / never emitted
1  metadata
2  json_path
3  plugin_mapper
4  always_missing_v0   migration compiler only
~~~

- `metadata` has `item_count = 0`, zero regex/mapper fields, and an exact
  eight-byte payload: `metadata_id u16`, `reserved u16`, `reserved u32`.
  Metadata value semantics are owned only by the parent definition; they are
  not duplicated in this payload. Total length is exactly 40 bytes.
- `json_path` has at most 1,024 segments, `item_count` equal to the segment
  count, `regex_semantics = 1`, `mapper_contract = 0`, and the Question 4
  records. Zero segments select the parsed root.
- `plugin_mapper` has `item_count = 0`, `regex_semantics = 0`, and
  `mapper_contract = 1` for migrated legacy single-byte behavior or `2` for
  corrected typed plural behavior. New definitions use `2`.
- `always_missing_v0` is exactly the 32-byte header with no payload and all
  kind-specific fields zero. New configuration cannot request it.

Metadata fields require their matching selector plus the canonical `none`
parser plan. JSON path, plugin mapper, and always-missing ordinary fields
require a non-`none` parser plan. Every dependency ordinal must resolve inside
the same ValueStore dependency table.

### 4. Corrected JSON-path codec and bounded traversal

**DECIDED (self):** Segment records retain the original eight-byte framing but
only these tags:

~~~text
0  invalid
1  object_key      nonempty raw UTF-8 payload
2  numeric_index   exactly eight payload bytes, u64 LE
3  fan_out         empty payload
4  regex           raw UTF-8 pattern body; flag bit 0 is case-insensitive
~~~

If any migrated legacy path contains a negative/fractional number, bool, null,
array, object, or another segment that always failed resolution, the **whole
selector** canonicalizes to `always_missing_v0`. Prefixes and suffixes are not
retained because they cannot change its result. New configuration rejects that
input. Malformed/uncompilable regex-looking strings remain literal object keys,
matching current behavior.

Traversal semantics remain ordered and platform-independent:

1. Exact keys use byte-exact object lookup.
2. A numeric segment uses checked array indexing without narrowing the stored
   `u64`; against an object it uses the full canonical unsigned-decimal key.
3. Array fan-out/regex uses array order. Object fan-out/regex uses strictly
   increasing raw UTF-8 key order regardless of map implementation.
4. Regex matching is substring/search matching. Array candidates use canonical
   compact JSON text, with strings matched by their contents and maps sorted by
   raw UTF-8 key bytes.
5. The `i` flag is the only effective flag. Ignored legacy flags canonicalize
   away. `regex_semantics = 1` freezes the exact regex crate version, enabled
   features, Unicode tables, and AeorDB conformance corpus as one permanent
   AeorRegexV1 capability. A dependency upgrade must emulate that contract or
   use another semantic ID.
6. Evaluation uses an explicit heap stack and streams depth-first in
   prior-candidate order. This yields the same terminal order as breadth
   materialization without an unbounded intermediate vector or path-depth call
   stack.

Work accounting is exact:

- one work item is charged for each `(candidate, segment)` evaluation;
- fan-out and regex additionally charge one item per array element or object
  member inspected;
- object-regex charges examined UTF-8 key bytes;
- array-regex charges the complete canonical candidate-text bytes; and
- every addition uses checked arithmetic and is charged before the work occurs.

Array-regex computes and charges the canonical text size before allocating the
text buffer. A candidate larger than the remaining byte budget therefore fails
without first allocating the over-limit representation.

Exceeding either selector budget or a terminal value/byte budget marks the
whole document deterministically unindexable for that ValueStore generation.
It never truncates and never publishes partial source values.

### 5. Corrected plugin-mapper selector and contracts

**DECIDED (self):** A plugin mapper payload is:

| Payload offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `dependency_ordinal` | Nonzero, one-based exact dependency |
| 4 | 4 | `arguments_length` | `A`; complete CanonicalConfigValueV1 |
| 8 | 4 | `invocation_policy_length` | `I`; complete InvocationPolicyV1 |
| 12 | 4 | reserved | Zero |
| 16 | `A` | `arguments` | Canonical null when omitted |
| `16 + A` | `I` | `invocation_policy` | Exact mapper call-site policy |

Its selector total length is exactly `48 + A + I`.

Permanent mapper contracts are:

- `1 = legacy_raw_bytes_single_v0`: migration only. It preserves the current
  JSON envelope and one arbitrary-byte-string output. Empty output is one
  present empty `bytes` value.
- `2 = canonical_typed_values_v1`: corrected default. Input is the complete
  parsed document plus canonical arguments through the frozen pure mapper ABI.
  Output is either explicit missing, deterministic rejection, or an ordered
  nonempty list of complete CanonicalSourceValueV1 values. Null, empty UTF-8,
  empty bytes, arrays/maps, order, and duplicates remain distinguishable. No
  implicit JSON parse or fan-out occurs after return.

Round 9 must freeze the exact request/result wire records and
`InvocationPolicyV1` bytes together with the plugin ABI. It may not change the
semantics above. Contract `2` is intentionally plural now; postponing plurality
would bake a known dead end into the first v1 writer.

Corrected parser/mapper invocations use a deterministic pure capability
profile: no database reads/writes, filesystem, network, clock, randomness, or
mutable registry lookup is available to plugin code. A migration-only legacy
ABI may expose the exact v0 deterministic "unavailable" stubs required to
instantiate an old module, but it never grants mutable I/O. The dependency
record freezes plugin ID, version, checksum, ABI, runtime/meter capability, and
role. A mutable path is only a deployment lookup aid and is never semantic
identity.

Exact parser/mapper dependencies are required to build, update, or perform an
authoritative fallback. They are **not** required merely to read an already
complete immutable value/posting closure. Round 9 must therefore distinguish
reader capabilities from builder/executor dependencies rather than degrading
historical indexed reads because a mapper was later uninstalled.

### 6. Corrected source values, limits, and failures

**DECIDED (self):** The corrected semantic compatibility table is:

- `source_value_codec = 1`: typed null/bool/i64/u64/f64/UTF-8/bytes/array/map
  canonical values using Round 7 structural framing and ValueStore-specific
  bounds. A canonical map decoder rejects duplicate or unsorted keys. The
  parser plan separately freezes how duplicate keys in an input document are
  handled before a canonical source value exists.
- `source_value_codec = 2`: migration-only `legacy_bytes_v0`, preserving
  `json_value_to_bytes`: big-endian numeric bytes, UTF-8 strings, one-byte
  booleans, null as empty bytes, and canonicalized compact JSON text for
  arrays/maps.
- `missing_semantics = 1`: no selector result creates neither value nor state.
- `null_semantics = 1`: typed present null. `null_semantics = 2` is the
  migration-only legacy empty-byte collapse and is valid only with codec `2`.
- `extraction_error_semantics = 1`: deterministic failure creates one
  ValueStore-owned versioned DocumentState reason.
- `multi_value_ordering = 1` and `duplicate_value_semantics = 1`: preserve
  selector/mapper order, duplicates, and zero-based value ordinals.
- `unindexable_semantics = 1`: every deterministic semantic bound is
  whole-document/all-or-nothing.

Only canonical combinations may be emitted. Corrected ordinary definitions use
typed codec/null semantics and `metadata_source_semantics = 0`. Corrected
metadata definitions use typed codec/null semantics and metadata semantics `1`.
Migration adapters use only the explicitly compatible legacy combination; a
decoder rejects nonsensical mixtures.

`max_canonical_source_bytes_per_document` includes complete canonical records.
For fixed ValueStore limits, zero means not applicable, `u32::MAX` or
`u64::MAX` means no semantic limit only for an explicit migration adapter, and
another value is an exact finite limit. New corrected definitions use finite
source count/bytes and content-input limits. `max_document_input_bytes` is zero
for metadata/`none`. The selector work fields are zero when no JSON-path
traversal occurs, finite for corrected JSON paths, and may be `u64::MAX` only
for a migration adapter preserving v0's lack of such a semantic bound.

Inside an `InvocationPolicyV1`, zero means not applicable, `u64::MAX` means no
**semantic** limit for an explicit migration adapter, and every other value is
an exact finite limit. `u64::MAX` never promises infinite machine resources:
ordinary memory admission can pause/degrade the build without publishing
coverage. New WASM definitions materialize finite request/output/linear-memory
limits and finite fuel under a frozen meter. New native-parser definitions use
finite input/output/structural limits; WASM memory and fuel fields are zero as
inapplicable. Native temporary memory is charged to the operational memory
coordinator and cannot become a machine-dependent durable document result.

Durable document-semantic failures include malformed input under the exact
parser contract, explicit deterministic plugin rejection, output-contract
violation, semantic byte/value/work limits, and guest-defined WASM semantic
traps/fuel/linear-memory limits only when the exact runtime/meter capability
and a real store resource limiter make those results portable. Host/runtime
internal traps remain operational failures.

Operational failures include cancellation, deadline, shutdown, missing exact
dependency/runtime capability, host I/O, host OOM, admission pressure, and
engine failure. They leave the document/generation incomplete, retry with
bounded backoff, and prevent coverage publication. A busy host must never turn
the same document into a different durable value or unindexable state.

### Round 8A proof and implementation gates

The Round 8 proof list is replaced/extended by these gates:

- Hand-authored 32-/64-byte-hash fixtures prove every offset and the
  `112 + H + F + S + P + D` formula on Linux, macOS, and Windows.
- Decoder property/fuzz tests reject truncation, trailing bytes, wrong lengths,
  unknown/inapplicable semantic combinations, allocation amplification, and
  every child/combined boundary before allocation.
- V0 characterization fixtures preserve current source arrays, regex fallback,
  ignored flags, numeric object keys, null/empty collapse, plural ordering, and
  metadata bytes. Known defects such as first-chunk `@hash` are target tests,
  not preserved behavior.
- A reference traversal model compares materialized and bounded-streaming
  evaluation across randomized documents, paths, map implementations, Unicode
  keys, nested fan-out, exact budget boundaries, and cancellation.
- Adversarial selectors prove giant fan-out later filtered to one value cannot
  exceed work/memory budgets or publish a partial result.
- Metadata fixtures cover signed timestamp boundaries, absent/empty content
  type, exact hash width, alias equivalence, unknown metadata rejection, and
  pre-v1 FileRecords that require separate migration.
- Mapper fixtures cover exact dependency binding, canonical arguments,
  missing versus empty bytes, typed plural values, duplicate/order retention,
  legacy contract `1`, corrected pure-host import rejection, frozen legacy
  unavailable-host stubs, and contract/result drift.
- WASM tests cover imported and module-owned memory growth, output-length
  rejection before response allocation, exact output limits,
  fuel boundary and boundary-plus-one, runtime capability mismatch, generic
  traps, host OOM, cancellation, and removal of trap-string classification as
  persistent authority.
- Failure-injection tests prove deterministic failures create stable reason
  codes while every operational failure withholds coverage and resumes safely
  after restart.
- Migration tests prove v0 remains active until a complete v1 shadow closure is
  published, missing full hashes block only affected `@hash` migration, and
  index-format migration never rewrites FileRecords as a hidden side effect.
- All bounded/concurrency tests have explicit timeouts; no plugin or traversal
  test is allowed to hang as its failure mode.

No Round 8 owner-authority decision remains. The next decision round must not
reopen these semantics merely because it is assigning the deferred parser,
dependency, invocation-policy, and plugin-ABI bytes.

---

## Round 9: Parser Resolution, Dependencies, and Plugin ABI

**Status:** Self-decided by Codex on 2026-07-31 after a `planning-cap`
territory pass and genuine AGIS `map_territory`, `test_protocol`, and
`thorough_review` passes. This round is binding. It closes every
parser/dependency/ABI item deferred by Round 8A. No owner-authority question
remains.

The source audit found several implementation facts that a plausible design
could easily have missed:

- The deliberate current parser order is explicit plugin, content-type
  registry plugin, raw JSON, then native parser. Commit `f56e0a2` introduced
  that order after native text parsing intercepted JSON and destroyed its
  field structure. Current plugin documentation still incorrectly says
  native-first.
- The engine sends v0 parser plugins only raw bytes, filename, path,
  content type, and size. `FileMeta` also declares hash and timestamps, but
  the engine never populates them; they deserialize as defaults.
- The parser registry is read and parsed for each document. Missing, I/O,
  UTF-8, and JSON errors are all collapsed to "no mapping," and registry
  changes do not have a complete engine-level reindex/invalidation path.
- Plugin aliases are mutable paths. Replacing or deleting one currently
  removes the only direct lookup for its old WASM bytes. A checksum in a
  ValueStore definition would describe the missing dependency without making
  it executable.
- The current parser request is JSON containing base64 file data, and the
  host writes it at guest address zero. That amplifies memory and can overwrite
  a module's own low memory. The runtime bounds an imported memory but may then
  select an unbounded module-owned exported memory.
- The current SDK turns parser errors into `{"error": ...}`, while the host
  accepts every JSON object as parser success. A parser error can therefore be
  indexed as ordinary data.
- The WASM cache is keyed by mutable plugin path, custom parser limits bypass
  it entirely, fuel exhaustion is classified partly by error-string matching,
  and the response length is trusted for allocation before a semantic output
  bound is applied.
- `list_plugins`, parser-registry reads, parser/index field failures, invalid
  memory-limit parsing, log writes, and cache invalidation contain separate
  error-squelching behavior. None may become the v1 authority.

These are implementation defects or compatibility facts, not product-policy
questions. The corrected contracts below preserve effective v0 behavior only
inside named migration adapters.

### 1. `ParserResolutionPlanV1` and candidate records

**DECIDED (self, 2026-07-31):** Snapshot a complete deterministic parser
decision program inside each ValueStore definition. Runtime indexing never
consults the mutable parser registry or a mutable plugin alias.

Every parser plan begins with this exact 48-byte header. All multibyte values
in this round are little-endian:

| Plan offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `plan_magic` | ASCII `APRP` |
| 4 | 2 | `plan_schema_version` | Exactly `1` |
| 6 | 2 | `plan_header_length` | Exactly `48` |
| 8 | 4 | `plan_total_length` | Complete plan bytes |
| 12 | 4 | `plan_flags` | Zero in v1 |
| 16 | 2 | `plan_kind` | Permanent kind below |
| 18 | 2 | `resolution_semantics` | Context rule below |
| 20 | 2 | `mime_semantics` | Context rule below |
| 22 | 2 | `no_match_semantics` | Context rule below |
| 24 | 4 | `candidate_count` | Exact concatenated records |
| 28 | 4 | `mime_dependency_ordinal` | One-based or zero when inapplicable |
| 32 | 16 | reserved | Zero |
| 48 | ... | candidates | Must consume total length exactly |

Permanent plan kinds are:

~~~text
0  invalid / never emitted
1  none
2  explicit_plugin
3  automatic
~~~

The canonical `none` plan is exactly 48 bytes, has zero candidates and semantic
fields, and is the only plan valid for metadata or `always_missing_v0`
selectors. `explicit_plugin` has exactly one explicit-plugin candidate,
`resolution_semantics` 1 or 2, and zero MIME/no-match semantics and MIME
dependency. `automatic` has nonzero resolution/MIME/no-match semantics, one
MIME dependency, zero to 512 registry candidates, exactly one raw-JSON
candidate, then exactly one native-suite candidate. Its MIME dependency must
resolve to the native `parser_resolution` component in the same dependency
table. Every candidate dependency ordinal must resolve to the role, ABI,
executor profile, and backend required by that candidate; a decoder rejects a
cross-role or native/WASM mismatch rather than trusting the ordinal alone.

Every candidate is:

| Candidate offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `candidate_total_length` | Exactly `32 + M + I` |
| 4 | 2 | `candidate_kind` | Permanent kind below |
| 6 | 2 | `match_semantics` | Kind-specific |
| 8 | 4 | `dependency_ordinal` | Nonzero one-based dependency |
| 12 | 4 | `invocation_policy_length` | `I`; exactly 128 in v1 |
| 16 | 4 | `match_length` | `M`; kind-specific |
| 20 | 4 | `candidate_flags` | Zero |
| 24 | 8 | reserved | Zero |
| 32 | `M` | `match` | Kind-specific bytes |
| `32 + M` | `I` | `invocation_policy` | Complete policy from Question 4 |

Candidate kinds are:

~~~text
0  invalid
1  explicit_plugin
2  registry_plugin
3  raw_json
4  native_suite
~~~

Explicit, raw-JSON, and native-suite candidates require `M = 0` and
`match_semantics = 0`. A corrected registry candidate has
`match_semantics = 1` and stores one canonical MIME essence from Question 2.
A legacy registry candidate has `match_semantics = 2` and stores the exact v0
content-type lookup bytes. Corrected registry records are strictly increasing
by raw match bytes. Duplicates are rejected. The raw and native candidates
always follow that sorted registry tier in that order.

`resolution_semantics = 1` is the corrected behavior:

1. An explicit parser claims every document. Success yields its parsed
   document; deterministic rejection/contract/resource failure stops. There
   is no fallback that could hide an explicitly selected parser failure.
2. In an automatic plan, an exact normalized registry match claims the
   document. Its success or failure stops; another parser is not tried.
3. Otherwise the raw-JSON candidate probes the document. Successful canonical
   JSON wins. `application/json` and any valid application subtype ending in
   `+json` are JSON media types and therefore claim the document before the
   probe; malformed UTF-8/JSON is a deterministic parser failure. For another
   or unknown media type, ordinary syntax/UTF-8 mismatch means `not_claimed`
   and advances to native parsing. Duplicate keys and semantic resource limits
   are deterministic rejection once a JSON root is recognized; they never
   fall through and become native text.
4. The native suite claims a document by corrected MIME/extension dispatch.
   A claimed parser's failure stops. No claim means the ordinary content field
   is not applicable and is missing, not unindexable.

`resolution_semantics = 2` is migration-only `effective_pipeline_v0`. It
preserves the current explicit/registry/raw/native order, exact v0 lookup and
native dispatch, current serde JSON behavior, object-only plugin parser output,
and stop/fallthrough outcomes through exact legacy dependencies. It also
preserves the current SDK error-object collision when an existing generation
must be rebuilt. Corrected definitions never use it.

`no_match_semantics = 1` means no candidate claim creates no source value and
no DocumentState. A deterministic failure after a candidate claims the
document uses Round 8A's whole-document state policy. `no_match_semantics = 2`
is the migration-only v0 silent-skip behavior. Parser order, candidate result,
and failure class are shared by every field using one ValueStore; field loops
must not independently rerun or reinterpret the parser.

### 2. MIME normalization and registry snapshots

**DECIDED (self, 2026-07-31):** Route corrected parsers by a strict normalized
media-type essence while preserving the original stored content type as parser
metadata. Do not hash mutable registry bytes or re-read the registry at
execution time.

`mime_semantics = 1` is `media_type_essence_v1`:

1. A stored content type is trimmed only for leading/trailing ASCII SP and HTAB
   while parsing. The FileRecord's original UTF-8 bytes are not rewritten.
2. Type and subtype are parsed under RFC 9110 media-type syntax, constrained to
   RFC 6838 restricted names of 1..127 ASCII bytes each. They are ASCII-lowercased
   and joined by one `/`. The essence is therefore at most 255 bytes.
3. Parameters must be syntactically valid, but do not participate in parser
   routing. The parser request receives both the original stored value and the
   canonical essence, so a format-specific parser may interpret parameters.
4. Missing, empty, or malformed content type has no valid essence and is
   treated as generic for native extension fallback. A valid
   `application/octet-stream` essence is also generic.
5. Corrected extension fallback uses the final nonempty suffix after the last
   dot in the final filename segment, ASCII-lowercased. It runs only for a
   generic MIME value. It performs no locale/Unicode folding and no percent
   decoding.
6. Native MIME dispatch uses the essence, including the existing `text/x-`
   prefix family. Parser output receives the original stored content-type
   value, not the lowercased essence substituted in its place.

`mime_semantics = 2` is migration-only `exact_content_type_v0`: exact
case/parameter-sensitive registry and native matching, exact
`application/json` skip, and case-sensitive extension fallback only for empty
or exact `application/octet-stream`.

The canonical corrected registry source remains
`/.aeordb-config/parsers.json`, but its v1 user schema is:

~~~json
{
  "$v": 1,
  "parsers": {
    "application/pdf": "pdf-parser-alias",
    "text/plain": "text-parser-alias"
  }
}
~~~

- The file contains at most 512 entries. Keys are parameter-free valid media
  types and compile to `media_type_essence_v1`; aliases are 1..4096 UTF-8 bytes
  without NUL or control characters.
- Duplicate source keys, duplicate normalized essences, parameters, wildcard
  keys, unknown top-level members, non-string aliases, and trailing/duplicate
  JSON members are validation errors.
- `application/json` is reserved for the raw-JSON candidate and is rejected in
  the global v1 registry with an error directing the administrator to an
  explicit per-scope parser. This preserves the deliberately fixed priority
  contract instead of silently ignoring the entry.
- A missing registry file compiles to an empty registry tier. I/O, integrity,
  UTF-8, schema, or JSON failure is not absence and blocks compilation with a
  path-specific error.
- The absent-version v0 registry is the current bare map. Migration parses its
  effective last-key-wins serde result, omits the currently ignored exact
  `application/json` entry, preserves exact match bytes, and reports malformed
  input that historically collapsed to an empty registry. New corrected
  compilation never uses those permissive rules.

The compiler resolves every alias to an exact dependency record while holding
one configuration/deployment snapshot. Equivalent registry key order produces
identical candidate and dependency bytes. A registry mutation creates new
ValueStore definitions for every automatic scope that uses it; old definitions
retain their embedded snapshot and exact dependencies.

The corrected built-in native suite retains the eight current format families:
text/code, HTML/XML, image, audio, video, PDF, MS Office, and ODF. Its exact
MIME/extension table and output fixtures are versioned as one semantic suite.
Changing a dispatch rule or observable parsed output creates a new native
dependency fingerprint and a shadow ValueStore build. A code refactor that
passes the same conformance corpus retains the same semantic fingerprint.

### 3. `DependencyTableV1`, identities, and artifacts

**DECIDED (self, 2026-07-31):** Separate immutable executable identity from
mutable deployment aliases, and separate builder/executor availability from
reader capability. Completed canonical values/postings remain readable after
a parser is uninstalled; updates, rebuilds, and authoritative fallback require
the exact executor.

The dependency table begins with:

| Table offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `table_magic` | ASCII `ADPT` |
| 4 | 2 | `table_schema_version` | Exactly `1` |
| 6 | 2 | `table_header_length` | Exactly `32` |
| 8 | 4 | `table_total_length` | Complete table bytes |
| 12 | 4 | `table_flags` | Zero |
| 16 | 4 | `record_count` | 0..1024 |
| 20 | 4 | `record_bytes_length` | Exactly total minus 32 |
| 24 | 8 | reserved | Zero |
| 32 | ... | records | Strict canonical order |

The canonical empty table is exactly 32 bytes. A dependency record is:

| Record offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `record_total_length` | Exactly `96 + I + V` |
| 4 | 2 | `dependency_kind` | Permanent kind below |
| 6 | 2 | `dependency_role` | Permanent role below |
| 8 | 4 | `dependency_flags` | Only defined migration bits |
| 12 | 2 | `abi` | Permanent ABI ID below |
| 14 | 2 | `executor_profile` | Permanent profile ID below |
| 16 | 2 | `fingerprint_semantics` | Permanent ID below |
| 18 | 2 | `artifact_kind` | Permanent artifact ID below |
| 20 | 4 | `dependency_id_length` | `I`; 1..4096 |
| 24 | 4 | `version_length` | `V`; 0..256 |
| 28 | 4 | reserved | Zero |
| 32 | 8 | `artifact_length` | Exact raw bytes or zero |
| 40 | 32 | `fingerprint` | Nonzero raw BLAKE3-256 |
| 72 | 24 | reserved | Zero |
| 96 | `I` | `dependency_id` | Canonical UTF-8 or explicit legacy opaque ID |
| `96 + I` | `V` | `version` | Canonical SemVer UTF-8 or absent-v0 |

Permanent IDs are:

~~~text
dependency_kind:
  0 invalid
  1 wasm_plugin
  2 native_semantic_component

dependency_role:
  0 invalid
  1 parser
  2 mapper
  3 parser_resolution
  4 selector

abi:
  0 no_cross_boundary_abi
  1 parser_json_base64_object_v0
  2 mapper_json_raw_bytes_v0
  3 parser_binary_canonical_v1
  4 mapper_binary_canonical_v1

executor_profile:
  0 none
  1 aeordb_native_deterministic_v1
  2 wasmi_0_42_1_aeordb_pure_v1
  3 wasmi_0_42_1_legacy_unavailable_stubs_v0

fingerprint_semantics:
  0 invalid
  1 blake3_raw_artifact_bytes
  2 blake3_semantic_conformance_manifest

artifact_kind:
  0 no_artifact
  1 system_wasm_module_raw_v1
~~~

Defined dependency flags are bit 0 `version_absent_v0`, bit 1
`opaque_id_v0`, and bit 2 `artifact_required`. Corrected dependencies may use
only bit 2 for a WASM artifact; native components have zero flags. Migration
may additionally use bits 0/1. Every other bit is zero.

A corrected dependency ID is a stable case-sensitive absolute namespace ID,
such as `/org/aeordev/aeordb/plugins/extract`: 1..4096 UTF-8 bytes, leading
slash, nonempty segments, no empty/`.`/`..` segment, NUL, or control character,
and no Unicode normalization. Corrected versions are canonical SemVer strings.
Name, author, mutable alias/path, created/updated timestamps, and HTTP route do
not participate in ValueStore identity. A v0 UUID or absent/noncanonical
version is retained only under the corresponding migration flag.

A WASM record uses its exact raw module-byte digest, exact byte length,
artifact kind 1, role-specific ABI, and one exact runtime profile. A native
record uses a BLAKE3 digest of a checked-in, canonical semantic specification
plus conformance fixture manifest; it has zero artifact length/kind and ABI
zero. At minimum, v1 has separately fingerprinted native components for the
MIME router, strict raw-JSON parser, native parser suite, and AeorRegexV1
selector. Literal native fingerprints are generated from hand-reviewed
fixtures and frozen before the first writer; source-file/build hashes are
forbidden because comments or behavior-preserving refactors are not semantic
changes.

Records are deduplicated, then sorted by this exact tuple using raw byte order:

~~~text
(dependency_kind,
 dependency_role,
 dependency_id,
 version-presence-tag,
 version,
 abi,
 executor_profile,
 fingerprint_semantics,
 fingerprint,
 artifact_kind,
 artifact_length,
 dependency_flags)
~~~

The sorted one-based position is the dependency ordinal. Duplicate canonical
records are rejected by a decoder and deduplicated by a compiler. The complete
table remains subject to the Round 8A 256 KiB child cap and the parent 512 KiB
cap; every length/count is preflighted before allocation.

An unknown dependency kind requires a new reader capability and cannot be
guessed. For a known kind, an unknown ABI, executor profile, or native
fingerprint is structurally retainable but executor-unavailable. It does not
invalidate an otherwise complete immutable value/posting closure. The
manifest's `required_reader_capabilities` covers definition/value/page codecs,
not optional builder executors. Build, mutation catch-up, migration, or
authoritative fallback checks the dependency table separately and reports the
approved typed unavailable-dependency state.

### 4. Exact `InvocationPolicyV1`

**DECIDED (self, 2026-07-31):** Freeze resource semantics per concrete call
site, not in the dependency or ValueStore parent. A dependency can be reused
under different policies without becoming two executable identities.

Every invocation policy is exactly 128 bytes:

| Policy offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `policy_magic` | ASCII `AIVP` |
| 4 | 2 | `policy_schema_version` | Exactly `1` |
| 6 | 2 | `policy_header_length` | Exactly `128` |
| 8 | 4 | `policy_total_length` | Exactly `128` |
| 12 | 4 | `policy_flags` | Zero |
| 16 | 2 | `execution_backend` | 1 native, 2 WASM32 |
| 18 | 2 | `host_profile` | 0 native, 1 pure v1, 2 legacy stubs v0 |
| 20 | 2 | `limit_semantics` | Exactly `1` |
| 22 | 2 | `structure_semantics` | Exactly `1` |
| 24 | 8 | `max_request_bytes` | Complete wire request or zero |
| 32 | 8 | `max_response_bytes` | Complete wire/native output |
| 40 | 8 | `max_linear_memory_bytes` | WASM only |
| 48 | 8 | `max_fuel` | WASM only |
| 56 | 8 | `max_table_elements` | WASM only |
| 64 | 8 | `max_structure_nodes` | Decoded result tree |
| 72 | 8 | `max_scalar_bytes` | One string/bytes/key/detail |
| 80 | 4 | `max_structure_depth` | Decoded result tree |
| 84 | 4 | `max_container_members` | One array/map |
| 88 | 4 | `max_wasm_instances` | WASM store limiter |
| 92 | 4 | `max_wasm_memories` | WASM store limiter |
| 96 | 4 | `max_wasm_tables` | WASM store limiter |
| 100 | 4 | `max_value_stack_height` | Frozen meter/profile units |
| 104 | 4 | `max_recursion_depth` | Frozen meter/profile units |
| 108 | 20 | reserved | Zero |

Round 8A's zero/finite/maximum migration sentinel rules apply. New corrected
WASM policies materialize finite request, response, memory, fuel, table,
structure, stack, instance, memory-count, and table-count values.
`max_linear_memory_bytes` is a nonzero multiple of 64 KiB and at most the
WASM32 address space; the remaining WASM values are checked against the exact
executor's representable ranges. Native/raw parsers use zero for request and
all WASM-only fields, but finite response/structure limits. The parent
ValueStore's finite `max_document_input_bytes` remains the one document-input
authority.

The executor profile fixes module validation, proposal/features, fuel cost
table, stack behavior, trap taxonomy, store limiter semantics, and result
metering. A wasmi upgrade must emulate profile 2/3 or introduce a new profile
and dependency. Runtime errors are classified through typed engine errors,
never message substrings.

No wall-clock timeout, scheduler budget, cache size, compile-cache decision,
logging flag, retry count, or host memory admission value appears here.
Cancellation, shutdown, host OOM/admission, and deadlines remain operational.
Policy limits are charged before allocation/work where possible. In
particular, a guest response length is checked with overflow-safe arithmetic
against `max_response_bytes` and linear-memory bounds before allocating a host
buffer.

### 5. Corrected parser and mapper WASM ABIs

**DECIDED (self, 2026-07-31):** Replace JSON/base64 and address-zero writes for
new plugins with role-specific binary records using the already-approved
canonical value codecs. Retain the current `handle` ABI only for exact v0
migration.

#### Embedded `AeorPluginManifestV1`

A corrected parser/mapper module contains exactly one custom section named
`aeordb.plugin.v1`. Its payload begins with this exact 64-byte header:

| Manifest offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `manifest_magic` | ASCII `APWM` |
| 4 | 2 | `manifest_schema_version` | Exactly `1` |
| 6 | 2 | `manifest_header_length` | Exactly `64` |
| 8 | 4 | `manifest_total_length` | Exactly `64 + I + N + V + A + 8R` |
| 12 | 4 | `manifest_flags` | Zero |
| 16 | 4 | `plugin_id_length` | `I`; 1..4096 |
| 20 | 4 | `name_length` | `N`; 1..4096 |
| 24 | 4 | `version_length` | `V`; 1..256 |
| 28 | 4 | `author_length` | `A`; 0..4096 |
| 32 | 2 | `role_count` | `R`; 1..8 |
| 34 | 2 | reserved | Zero |
| 36 | 28 | reserved | Zero |
| 64 | `I` | `plugin_id` | Canonical absolute namespace ID |
| `64 + I` | `N` | `name` | Display UTF-8 |
| `64 + I + N` | `V` | `version` | Canonical SemVer UTF-8 |
| `64 + I + N + V` | `A` | `author` | Display UTF-8 or absent |
| after strings | `8R` | roles | Strictly sorted role records below |

Each role record is `role u16`, `abi u16`, and zero `flags u32`. V1 permits
only `(parser, parser_binary_canonical_v1)` and
`(mapper, mapper_binary_canonical_v1)`, using the permanent dependency
role/ABI IDs above. Duplicate or unsorted records, duplicate manifest sections,
unknown flags/roles/ABIs, invalid UTF-8, or inconsistent lengths reject the
module as corrected v1. Other WASM custom sections are retained verbatim and
therefore affect the raw artifact digest, but do not add capabilities.

The embedded ID, version, and supported role/ABI pairs are executable
authority. Corrected deployment supplies only the mutable alias and optional
display fields; any supplied ID/version/name/author must exactly match the
manifest. The alias record copies the verified metadata and records its own
deployment timestamps and externally computed artifact digest. A checksum is
not embedded in the module because a digest over bytes containing that digest
would be self-referential. V0 modules without this section remain valid only
through explicit legacy dependency records.

#### Common WASM32 calling convention

A corrected module:

- imports no WASI, database, filesystem, network, clock, randomness, logging,
  memory, or other host function;
- defines and exports exactly one module-owned WASM32 memory named `memory`;
- exports `aeordb_alloc_v1(len: u32) -> u32`;
- exports `aeordb_handle_v1(ptr: u32, len: u32) -> u64`; and
- returns `(response_ptr << 32) | response_len`.

The host creates a fresh limited Store/instance for every invocation, charges
instantiation/allocator/handler fuel, asks the guest allocator for request
space, validates the pointer range, writes the request, invokes the handler,
then validates/copies the response. The Store resource limiter applies to the
module-owned memory and tables; a declared or grown resource cannot bypass the
policy merely because the module exported it. Zero pointer for a nonempty
allocation, zero-length response, overflow/out-of-bounds, wrong export type,
any import, or an over-limit response is a typed contract/resource failure.
The response may reuse or overlap the request allocation after the handler
returns because the host no longer needs the request; forbidding that would
waste memory without adding an invariant. The fresh Store makes guest
deallocation unnecessary and prevents state leakage between documents.
Compiled modules are cached by
`(raw_module_digest, executor_profile)`, never by mutable alias or invocation
policy; instances are never shared.

#### `ParserRequestV1`

The parser request header is exactly 112 bytes:

| Request offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `request_magic` | ASCII `APRQ` |
| 4 | 2 | `request_schema_version` | Exactly `1` |
| 6 | 2 | `request_header_length` | Exactly `112` |
| 8 | 4 | `request_total_length` | Complete request, fits `u32` |
| 12 | 4 | `request_flags` | Presence bits below |
| 16 | 4 | `path_length` | `P`; canonical absolute UTF-8 |
| 20 | 4 | `filename_length` | `F`; exact final path segment |
| 24 | 4 | `stored_content_type_length` | `C` |
| 28 | 4 | `mime_essence_length` | `M`; 0 or 3..255 |
| 32 | 2 | `content_hash_length` | `H`; zero when absent |
| 34 | 2 | `hash_algorithm` | Permanent database hash ID or zero |
| 36 | 4 | `data_length` | `D`; exact raw bytes |
| 40 | 8 | `logical_size` | Equal to `D` for a full parser call |
| 48 | 8 | `created_at_ms` | Signed `i64` |
| 56 | 8 | `updated_at_ms` | Signed `i64` |
| 64 | 48 | reserved | Zero |
| 112 | `P` | `path` | Exact bytes |
| `112 + P` | `F` | `filename` | Exact bytes |
| `112 + P + F` | `C` | `stored_content_type` | Exact original bytes |
| `112 + P + F + C` | `M` | `mime_essence` | Canonical ASCII or absent |
| `112 + P + F + C + M` | `H` | `content_hash` | Raw full-file hash or absent |
| after hash | `D` | `data` | Raw file bytes, never base64 |

Request flag bit 0 means stored content type is present, bit 1 means a valid
MIME essence is present, and bit 2 means full content hash is present. No other
bits are set. Presence, lengths, hash algorithm/width, path/filename, logical
size/data length, and total-length arithmetic must agree. This v1 ABI finally
supplies the metadata that the SDK had documented but the engine did not send,
without pretending those defaults were part of v0 behavior.

This is intentionally a bounded full-buffer WASM32 ABI, not a streaming ABI.
The compiler requires a corrected WASM policy's `max_request_bytes` to fit
`u32`, and invocation requires the complete header, metadata, and document to
fit both that limit and the WASM32 address space. A claimed document that
exceeds the ValueStore or ABI semantic limit receives the approved deterministic
resource state for that content ValueStore. The file write and independent
metadata ValueStores still succeed; the engine must not reject or stream a
multi-gigabyte file merely because an optional content parser cannot represent
it. Machine admission/OOM below an allowed semantic limit remains operational
and retryable, never durable document state.

#### `ParserResultV1`

The parser result is:

| Result offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `result_magic` | ASCII `APRS` |
| 4 | 2 | `result_schema_version` | Exactly `1` |
| 6 | 2 | `result_header_length` | Exactly `48` |
| 8 | 4 | `result_total_length` | Exactly `48 + V + D` |
| 12 | 4 | `result_flags` | Zero |
| 16 | 2 | `result_status` | 1 success, 2 rejected |
| 18 | 2 | `reason` | 0 success, 1 plugin_rejected |
| 20 | 4 | `value_length` | `V` |
| 24 | 4 | `detail_length` | `D` |
| 28 | 20 | reserved | Zero |
| 48 | `V` | `parsed_document` | One complete CanonicalSourceValueV1 |
| `48 + V` | `D` | `detail` | Optional bounded UTF-8 diagnostics |

Success requires one complete value, zero reason/detail, and permits any
canonical root type rather than only an object. Rejected requires no value and
may carry diagnostic detail. Detail is never durable identity/state text; the
host persists only the stable rejection class. There is no plugin `not_match`
status for an explicit/registry mapping that has already claimed a document.

#### `MapperRequestV1` and `MapperResultV1`

The mapper request has a 64-byte header:

| Request offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `request_magic` | ASCII `AMRQ` |
| 4 | 2 | `request_schema_version` | Exactly `1` |
| 6 | 2 | `request_header_length` | Exactly `64` |
| 8 | 4 | `request_total_length` | Exactly `64 + J + A` |
| 12 | 4 | `request_flags` | Zero |
| 16 | 4 | `document_length` | `J`; complete CanonicalSourceValueV1 |
| 20 | 4 | `arguments_length` | `A`; complete CanonicalConfigValueV1 |
| 24 | 40 | reserved | Zero |
| 64 | `J` | `parsed_document` | Complete parsed root |
| `64 + J` | `A` | `arguments` | Canonical null when omitted |

The mapper result has a 48-byte header:

| Result offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `result_magic` | ASCII `AMRS` |
| 4 | 2 | `result_schema_version` | Exactly `1` |
| 6 | 2 | `result_header_length` | Exactly `48` |
| 8 | 4 | `result_total_length` | Exactly `48 + V + D` |
| 12 | 4 | `result_flags` | Zero |
| 16 | 2 | `result_status` | 1 values, 2 missing, 3 rejected |
| 18 | 2 | `reason` | 0 except 1 plugin_rejected |
| 20 | 4 | `value_count` | Exact concatenated values |
| 24 | 4 | `values_length` | `V` |
| 28 | 4 | `detail_length` | `D` |
| 32 | 16 | reserved | Zero |
| 48 | `V` | `values` | Complete CanonicalSourceValueV1 records |
| `48 + V` | `D` | `detail` | Optional bounded UTF-8 diagnostics |

`values` requires a nonzero count and preserves exact order/duplicates.
`missing` has zero count/value/detail. `rejected` has no values and may carry
detail. Null, empty UTF-8, empty bytes, and missing remain distinct. Every
container/count/byte limit is validated while decoding rather than after a
large tree has already been allocated.

The SDK adds new v1 macros/types while retaining explicitly named v0 builders
for migration fixtures. It must never silently make the existing
`aeordb_parser!` macro emit a byte-incompatible ABI under the same SDK/API
version.

### 6. Historical availability, invalidation, migration, and errors

**DECIDED (self, 2026-07-31):** Persist exact WASM modules independently from
their aliases and retain them through every live builder dependency. Do not
put user-supplied plugin code under `IndexArtifact`: that family was approved
as rebuildable derived data, while plugin code is authoritative input.

#### Module archive and alias records

Every deployed WASM module is archived as its exact raw bytes at:

~~~text
/.aeordb-system/plugin-artifacts/blake3/<64 lowercase hex digest>
~~~

The digest is BLAKE3-256 over raw module bytes. Length is 1..64 MiB in v1.
The path is in the protected non-HEAD system namespace, so archiving a module
does not change a user namespace root. An existing path is read and
digest/length-verified **and compared byte-for-byte with the incoming module**;
any disagreement is corruption or a digest-collision alarm, never overwrite.
Deployment writes and durably publishes the module before publishing an alias.
A crash may leave an orphan module, but never a durable alias to a missing
module.

The mutable current alias becomes a versioned low-volume system control at:

~~~text
/.aeordb-system/plugin-aliases/<BLAKE3(alias UTF-8) lowercase hex>
~~~

`PluginAliasRecordV1` has this exact body plus trailing CRC-32/ISO-HDLC:

| Alias offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `alias_magic` | ASCII `APAL` |
| 4 | 2 | `alias_schema_version` | Exactly `1` |
| 6 | 2 | `alias_header_length` | Exactly `128` |
| 8 | 4 | `alias_total_length` | `128 + A + I + N + V + U + 4` |
| 12 | 4 | `alias_flags` | Presence/legacy bits below |
| 16 | 4 | `alias_length` | `A`; 1..4096 |
| 20 | 4 | `plugin_id_length` | `I`; 1..4096 |
| 24 | 4 | `name_length` | `N`; 1..4096 |
| 28 | 4 | `version_length` | `V`; 0..256 |
| 32 | 4 | `author_length` | `U`; 0..4096 |
| 36 | 2 | `plugin_type` | Exactly 1 (WASM) in v1 |
| 38 | 2 | `artifact_kind` | Exactly 1 |
| 40 | 32 | `artifact_fingerprint` | Raw module digest |
| 72 | 8 | `artifact_length` | Exact module bytes |
| 80 | 8 | `created_at_ms` | Signed `i64` |
| 88 | 8 | `updated_at_ms` | Signed `i64` |
| 96 | 32 | reserved | Zero |
| 128 | `A` | `alias` | Exact mutable lookup name |
| `128 + A` | `I` | `plugin_id` | Stable identity |
| after ID | `N` | `name` | Display metadata |
| after name | `V` | `version` | Canonical SemVer or migration absence |
| after version | `U` | `author` | Display metadata or absent |
| final 4 | 4 | `alias_crc32` | CRC over every prior alias byte |

Alias flag bit 0 means version absent, bit 1 author absent, and bit 2 opaque
legacy plugin ID. Presence flags must agree with lengths. Alias bytes are
case-sensitive exact UTF-8 without NUL/control characters. The key hash and
embedded alias must agree. For a corrected module, ID/version/name/author and
role/ABI support are read from and checked against its embedded manifest; the
route alias remains only a convenient invocation/config name. The alias
artifact digest and length must match those exact module bytes.

This v1 alias record deliberately covers executable WASM artifacts only.
Current `native` and `rule` PluginRecords remain versioned legacy controls
until their own executable/storage contracts are designed; they cannot satisfy
a corrected persistent parser/mapper dependency and are not misrepresented as
zero-length WASM artifacts. The transition catalog can read both record
families. Migrating NVT parser/mapper dependencies rewrites only the WASM
records it actually needs.

The current JSON `PluginRecord` is v0 migration input, not v1 authority.
Migration recomputes the digest from stored `wasm_bytes`; a stale nonempty
checksum field is reported but cannot overrule integrity-verified executable
bytes. It archives those bytes, then writes the canonical alias record. Module
replacement/removal changes only the mutable alias. Old modules remain
addressable by dependency fingerprint.

GC treats module artifacts as **builder-dependency edges**, not immutable index
read-validity edges. It retains a module while referenced by any current alias,
retained ValueStore definition/manifest, A/B generation, snapshot/export pin,
build/migration/repair task, checkpoint, or request requiring execution. Once
unreferenced, it enters the same pending-delete grace/two-complete-mark process
as other reclaimable data. A completed closure with a missing artifact remains
readable but reports builder dependency unavailable; GC/verify also reports the
broken soft edge.

Backup/export that includes index generations must include every reachable
module artifact and dependency descriptor even when ordinary system data is
excluded. Full system backup includes plugin aliases and module archives.
Import validates digest/length before making aliases or build dependencies
visible. The current hard-coded system-backup list omits plugins; implementation
must replace that omission with one centralized system-root registry shared by
GC, backup, verify, and repair.

#### Mutation and configuration behavior

- One engine-owned control-mutation coordinator serializes semantically
  relevant changes and increments a process-local monotonic `control_epoch`
  only after an indexes/parser config or plugin-alias mutation is durably
  published. The epoch is a race detector, not persistent authority, so this
  round does not smuggle in another durable record format. Orphan
  module-archive writes alone do not change it.
- A compiler reads `control_epoch`, captures the immutable namespace root plus
  exact config FileRecord identities, plugin-alias record identities, and
  module digests, resolves every dependency, then re-reads the epoch. A
  mismatch discards the capture and retries. The build task persists those
  exact source identities, not the process-local epoch. Before activation and
  after restart, the coordinator re-resolves and compares every affected
  identity; a stale completed build may remain as a historical artifact but
  cannot become active. There is no cross-root "atomically enough" exception
  and no long-lived lock across the shadow build.
- Every write path that can mutate `indexes.json`, `parsers.json`, plugin
  aliases, or module artifacts passes one engine-owned control-mutation
  observer. HTTP PUT, blob/batch, embedded writes, merge, copy/move, restore,
  sync/import, plugin host functions, and delete cannot each approximate the
  invalidation rules.
- A parser-registry change recompiles every automatic ValueStore. An alias
  digest/ID/version change recompiles every explicit/registry/parser-mapper
  config that resolves through that alias. Reverse dependencies are recorded
  in versioned control state so this is not a database-wide guess on every
  deploy.
- Old active generations and mutation catch-up continue using their archived
  exact dependency. A changed alias never mutates a ValueStore definition or
  causes old and new parser outputs to coexist under one ValueStoreId.
- Plugin removal prevents new compilation through the alias but does not
  delete archived dependencies or invalidate complete reads. If an affected
  index receives source mutations and its exact executor is unavailable, it
  enters explicit incomplete/degraded state; a stale index is never advertised
  current.
- Native component/profile support is advertised separately from reader
  codecs. Binaries retain old native semantic implementations as long as
  retained definitions may need execution. If one is unavailable, existing
  values remain readable and rebuild/fallback returns the typed historical
  dependency error.

#### V0 migration

Migration uses the same generation-checked immutable-root/catalog capture to
bind one exact effective world, archives and pins every needed module first,
and then compiles explicit legacy plans:

1. Existing explicit parser and mapper aliases bind to exact current bytes,
   actual plugin ID/version metadata, legacy ABI, legacy runtime profile, and
   the effective current memory-limit parse result.
2. The v0 registry preserves bare-map, exact-key, exact
   `application/json`-skip, and last-key-wins behavior. Missing is empty;
   malformed historically-empty behavior is preserved only with a migration
   diagnostic.
3. Raw JSON and native dispatch bind named v0 native semantic dependencies,
   including current duplicate-key/object/extension quirks. Corrected v1 is a
   separate shadow identity, never silently substituted during format
   migration.
4. Invalid configured memory strings compile to their exact current fallback,
   including unit-specific behavior; corrected configuration rejects malformed
   or overflowing limits rather than silently choosing 256 MiB/1 GiB.
5. Missing/corrupt plugin bytes or an unavailable native/profile implementation
   blocks only the affected ValueStore migration. V0 remains active and the
   migration report names every owner/config/dependency. No empty v1 closure is
   published as success.
6. V0 remains queryable until the v1 shadow closure is complete, validated,
   hard-published, and survives reopen. Module artifacts and build checkpoints
   are pinned across crash/restart.

#### Stable error boundary

- Missing registry file: canonical empty registry.
- Registry I/O/integrity/schema error: control/config failure, compilation
  blocked.
- Alias absent: config dependency unavailable.
- Module absent: builder dependency unavailable; completed reads unaffected.
- Module digest/length disagreement: corruption/evidence retained.
- Unknown executor/ABI/native fingerprint: executor unavailable, not closure
  corruption.
- Malformed dependency/plan/policy/request/result framing: corruption for
  persisted definitions; deterministic contract failure for guest output.
- Explicit plugin rejection, canonical output violation, and exact semantic
  policy exhaustion: stable per-document deterministic state.
- Cancellation, deadline, shutdown, host I/O/OOM/admission, compiler-cache
  failure, and unavailable dependency/runtime: operational/incomplete state,
  never durable document semantics.

Logging is diagnostic only. A failed log write cannot erase the primary error,
and a successful log is never the sole place a dependency/config failure is
reported. None of these paths may use `unwrap_or_default`, ignored `Result`, or
error-string parsing to select correctness behavior.

### Round 9 proof and implementation gates

The AGIS test protocol identified the following required proof spine. These
tests are written before production writers and use explicit per-test timeouts:

- Hand-authored golden fixtures cover empty/explicit/automatic plans, every
  candidate/dependency/policy/ABI/manifest/alias field, exact length formulas, both
  database hash widths where nested parent bytes differ, CRCs, canonical
  ordinals, and malformed/trailing/nonzero-reserve inputs on Linux, macOS, and
  Windows.
- Canonicalization property tests shuffle registry/dependency order, aliases,
  MIME case/parameters, equivalent SemVer spellings, and config JSON order.
  Equivalent semantics produce identical bytes/ValueStoreIds; every true
  parser, policy, ABI, fingerprint, or registry change produces a different
  identity.
- MIME tables cover every RFC name boundary, malformed/empty/whitespace input,
  parameters/quoted values, duplicate parameters, uppercase type/subtype,
  `application/octet-stream`, `application/json`, structured `+json`,
  non-ASCII/uppercase extensions, and every built-in MIME/extension rule.
- Resolution-model tests compare the explicit/registry/raw/native state
  machine against corrected and exact-v0 reference evaluators across success,
  no-claim, malformed JSON, duplicate keys, claimed parser failure, and exact
  resource boundaries. Native parser/MIME/regex implementations also run
  differential property tests against the frozen reference evaluators so the
  semantic fingerprint is not defended only by a pleasant but incomplete
  fixture list. No test merely asserts that indexing "did not panic."
- Parser/mapper codec fuzzing rejects truncation, integer overflow, allocation
  amplification, wrong presence/count/status/reason combinations, duplicate or
  unsorted canonical maps, over-depth trees, invalid UTF-8, and trailing bytes
  before publication. Manifest/custom-section scanning enforces module and
  section bounds before allocation and rejects duplicate semantic manifests.
- Adversarial WAT modules cover every malformed/duplicate embedded manifest,
  manifest/deploy/dependency disagreement, any imported memory/function,
  missing/wrong exports, allocator failure, start-function fuel, module-owned
  memory growth, table growth, stack/recursion limits, infinite loops, traps,
  overlapping/out-of-bounds pointers, `u32` wrap, zero/huge result lengths,
  safe response/request-buffer reuse, response-bound rejection before
  allocation, and typed fuel classification.
- SDK agreement fixtures prove Rust and any future SDK encode byte-identical v1
  requests/results, while v0 fixtures preserve the exact current JSON/base64,
  object-only, error-object, and raw-byte mapper behavior.
- Archive lifecycle integration tests deploy digest A, build a ValueStore,
  replace the alias with B, remove it, restart, run GC marks, export/import,
  and prove old builds execute A, new builds execute B, complete reads need
  neither, and artifacts are reclaimed only after every approved root/pin and
  grace condition disappears. A fault-injected same-path/same-digest but
  byte-different archive candidate must trigger the collision/corruption path
  and preserve both evidence inputs rather than overwrite.
- Configuration-race tests pause alias/registry/config writes at every capture
  and publication boundary. A compiled plan represents one epoch, dependency
  updates enqueue every reverse-dependent scope once, stale completed builds
  cannot activate after restart, unrelated orphan archive writes do not churn
  the runtime epoch,
  and no mutation path bypasses the engine observer.
- Failure-injection tests cover registry/plugin-record read errors, archive
  fsync failure, alias publication failure, cache lock poisoning, missing
  module/native profile, host admission/OOM, cancellation/shutdown, and log
  failure. They prove exact read-vs-builder degradation and no squelched primary
  error or false coverage.
- Migration fixtures from verified real v0 databases cover missing/incorrect
  checksum metadata, absent/opaque IDs/versions, malformed registry, invalid
  memory strings, SDK parser rejection encoded as data, registry JSON skip,
  native extension case, mid-migration alias replacement, crash/reopen, and
  per-ValueStore blocked migration without global destructive rollback.
- A real `/tmp/codex` AeorDB exercise deploys a corrected parser and mapper,
  indexes mixed JSON/text/binary files, changes aliases/registry, restarts,
  performs historical-root query fallback, exports/imports the database, and
  verifies archive/GC/diagnostic behavior through the actual CLI/HTTP/SDK
  surfaces.
- Grep/architecture gates require one parser state machine, one registry
  compiler, one dependency resolver, one plugin artifact catalog, one
  parser/mapper executor, and one control-mutation observer. They reject direct
  runtime reads of `parsers.json`, correctness decisions from mutable plugin
  paths, path-keyed compiled-module caches, address-zero request writes,
  base64 in v1 parser requests, trap-message classification, and ignored
  parser/registry/plugin-control errors.

False-confidence risks explicitly rejected by this test plan are serializer-
generated "goldens," only testing SDK-generated well-behaved modules, mocking
away persistence/GC/backup/restart, treating one successful E2E run as crash
proof, and using the same MIME/canonicalizer implementation as both system and
oracle.

### Round 9 parent-plan corrections

1. `required_reader_capabilities` must not include parser/mapper executor
   availability merely because the embedded definition names it. Builder and
   fallback readiness is a separate validated state.
2. The permanent system-root registry must include plugin aliases and module
   artifacts for GC, backup, verify, repair, and migration. Current hard-coded
   lists are incomplete.
3. Documentation must be corrected from native-first to the deliberate
   explicit/registry/raw/native order and from stale `/.config/...` paths to
   `/.aeordb-config/...`.
4. The v1 SDK/public deploy surface must accept a stable canonical plugin ID
   and expose ABI/role/runtime compatibility from the embedded v1 manifest. A
   route alias, display name, deploy-body claim, or generated UUID is not
   sufficient identity for a corrected persistent parser/mapper dependency.
5. Default numeric policy values remain for the later configuration-default
   round. That round may choose values, but may not alter these fields,
   ownership boundaries, sentinel rules, or deterministic/operational failure
   split.

No Round 9 owner decision remains. Literal native conformance fingerprints and
default numeric policy values are generated/selected at their already-defined
pre-writer gates; neither is an unresolved architecture question.

---

# Final Ratification Audit: AeorDB Spirit, Cross-Round Consistency, and Remaining Gates

**Date:** 2026-07-31
**Baseline inspected:** `5d3e284652f9fec7a5c843f1946132574af4d469`
**Method:** `planning-cap` evidence/territory/completeness passes plus genuine
AGIS `map_territory`, `test_protocol`, and adversarial `thorough_review`
passes against the current source, the July 16 parent plan, the original NVT,
storage, data-model, GC, B-tree, replication, backup, durability, parser, query,
pagination, and bot range-fetch plans.

## Ratification Verdict

Rounds 8A and 9 are internally strong and remain approved. The campaign as a
whole is **not yet ratifiable or implementation-ready**.

The problem is not that the design has drifted away from AeorDB. Most of its
important choices are unusually well aligned with AeorDB's original intent.
The remaining problems are narrower and more serious:

1. two approved correctness claims cannot be implemented from the currently
   specified identities;
2. the persistent-format freeze is materially incomplete;
3. the parent plan still contains superseded contracts and unsafe phasing; and
4. several recent production fixes and public ordering guarantees are not yet
   protected by the plan.

No v1 persistent writer may land until the blockers below are closed and the
formal parent plan is regenerated from the final decision record.

## AeorDB Spirit Ledger

| AeorDB principle | Final direction | Verdict |
| --- | --- | --- |
| Authoritative bytes and namespace first | File content, FileRecords, and the immutable namespace remain authority; indexes remain accelerators. | Aligned |
| NVT is an adaptive hint | Sparse scalar cells land near logical pages; gaps, stale anchors, lost resolution, and complete NVT loss change cost, never answers. | Strongly aligned |
| Converter produces the order | Values and query bounds use the same normalized fixed-point coordinate and typed comparator. The NVT is not separately sorted. | Aligned |
| KV remains unordered implementation machinery | The current KV format and lookup behavior remain unchanged; logical field order lives in page metadata plus NVT hints. | Aligned |
| Content-addressed, append-only publication | Immutable pages/manifests plus pointer-last A/B publication fit the WAL-filesystem model without claiming rollback transactions. | Aligned |
| Bounded at every scale | Page caches, memtables, GC work, query state, plugins, and KV residency enter one memory-admission model. | Aligned, pending exact budgets |
| Pluggable over monolithic | Parser/mapper semantics are versioned plugin dependencies behind one resolver and one pure ABI. | Aligned |
| Explicit and fail loud | Typed degradation, no partial coverage, no error-string policy, no ignored correctness errors. | Aligned in target; current squelch sites need migration gates |
| Zero ceremony externally | Defaults remain usable, while dangerous migration/GC actions are deliberate and observable. | Aligned if the configuration surface is consolidated |
| Everything is a path | Authoritative configs/plugins retain path-addressable identities; derived index pages do not pollute the user Merkle tree. | Acceptable evolution, but system-path versioning is not yet coherent |

### Accepted Design Evolutions

These are deliberate improvements, not spirit violations:

- `IndexArtifact` lives outside the namespace tree. Putting high-churn derived
  pages under `.indexes` would recursively change HEAD, inflate snapshots,
  produce SSE/listing noise, and turn index corruption into namespace
  corruption.
- Logical posting pages have bounded formats and stable IDs. The NVT still
  supplies approximate landing and changing resolution; fixed page framing is
  not equivalent to making the NVT authoritative.
- Persisted normalized coordinates are `u64` fixed-point rather than `f64`.
  The ratio remains conceptual while exact bytes and endpoint arithmetic become
  cross-platform.
- Correct cross-field composition uses document identity rather than
  intersecting unrelated scalar coordinates. This repairs an old NVT-plan
  mistake without weakening the NVT.
- Corrected WASM32 parser/mapper calls are bounded full-buffer operations. This
  is a scoped optional execution ABI, not permission for ordinary file reads
  to materialize arbitrary files. A later streaming guest ABI remains a valid
  roadmap item.

Old plans that say no default indexes, indexes are ordinary `.indexes` files,
cross-field NVT masks can be intersected directly, system data versions itself
automatically, or native parsing runs before explicit/registry parsing are
historical inputs, not current contracts. They must receive explicit
`superseded by` banners rather than remain apparently coequal documentation.

## Blocker 1: `root_hash` Does Not Yet Identify One Semantic Read View

Category 3 promises that one `root_hash` selects immutable namespace content
**and** the historical index/parser/config semantics used to interpret it.
Current AeorDB cannot satisfy that promise:

- user HEAD intentionally excludes absolute `/.aeordb-system` and
  `/.aeordb-config` state;
- the global parser registry, plugin aliases, and plugin module archive live
  outside HEAD;
- current sync code explicitly says historical system state cannot be
  reconstructed from a prior HEAD;
- changing a referenced parser alias or global registry can therefore leave
  namespace hash `H` unchanged while changing its semantic world from `S1` to
  `S2`; and
- `root_hash = H` then cannot tell a continuation whether to use `S1` or `S2`.

This is not only an accelerator-retention problem. Even an authoritative
fallback cannot know which parser registry/plugin binding to execute. A mutable
"latest semantics for H" rule would break stable paging and violate the
approved historical contract.

### Required correction

Freeze an immutable **Semantic State Root** containing only interpretation
authority:

- effective scope/index configuration identities;
- parser-registry snapshot identity;
- canonical Scope, ValueStore, Converter, and FieldIndex definitions;
- exact plugin/native dependency identities and archived module hashes; and
- semantic compatibility/profile IDs needed for exact fallback.

Do **not** include credentials, current users/groups, JWT state, lifecycle
scheduling, memory pressure, or other runtime/security policy. Authorization
continues to use current credentials and is reapplied before observables.

**Recommendation:** commit the Semantic State Root through one protected hidden
semantic anchor that participates in the namespace Merkle root but is excluded
from user file listing/search/count and ordinary file-change SSE surfaces. A
semantic mutation emits its own typed config/index event. A referenced semantic
change then naturally creates a new namespace root, while unrelated plugin
deployments do not. This preserves the simple public `root_hash` contract and
restores the original AeorDB principle that the root identifies all
authoritative state needed to reproduce content interpretation.

Alternatives that require a second public `semantics_hash`, choose mutable
"latest" semantics, or encode semantics in pagination tokens are rejected.
They either recreate the cursor object that Category 3 removed or make one root
mean different things over time.

The next round must freeze the semantic-anchor/catalog format, mutation order,
snapshot/fork behavior, v3-to-v4 migration mapping, GC edges, and exact
backup/replication rules.

## Blocker 2: The Mark Bitmap Does Not Identify Physical Incarnations

Category 2 says the liveness set tracks physical entity incarnations exactly,
but its bitmap is addressed by captured KV `(bucket, slot)`. One slot identifies
one active logical key/locator. It cannot distinguish:

- the current physical record from a lower-sequence record under the same key;
- an overwritten A/B pointer or low-volume control revision;
- an immutable append that became orphaned before KV publication; or
- a complete WAL record omitted from KV by a crash/rebuild defect.

Likewise, "stream unmarked occupied KV slots" enumerates only current KV
locators. It cannot discover every physical WAL incarnation that consumes disk.
The approved copy-on-write relocation record handles one special case, but no
general physical-retirement inventory is frozen for ordinary stable-key
updates and publication crashes.

### Required correction

Keep the KV-slot bitmap as the exact, compact oracle for **active logical
locator reachability**, but stop calling it physical-incarnation liveness.
Add one bounded physical-retirement path:

1. every successful stable-key locator replacement records the displaced
   `(key, offset, write_sequence, total_length, entry_type, replacement)` in a
   buffered retirement journal;
2. pointer/control publication uses that same path rather than inventing local
   cleanup;
3. dirty recovery scans its bounded recovery region and synthesizes any missing
   retirements for lower valid sequences;
4. a checkpointed physical WAL inventory/scrub discovers older orphans not
   represented by current KV state without materializing the WAL in memory;
5. each retired/orphan incarnation enters normal timestamped quarantine and is
   revalidated before Void reuse; and
6. missing retirement state leaks space and degrades diagnostics, but never
   authorizes early reuse.

The exact retirement journal, physical inventory checkpoint, crash boundaries,
and legacy-source treatment belong in the GcArtifact format round. A v4
side-by-side migration starts from a clean physical set by copying only proven
required source incarnations, but v4 runtime must remain complete afterward.

## Blocker 3: Persistent Format Freeze Is Incomplete

The Category 5 opening contract explicitly included both artifact families,
all low-volume controls, capability bits, limits, and fixtures. The completed
rounds currently freeze only part of that surface:

| Format family | Status |
| --- | --- |
| DatabaseHeader v4 and common EntityHeader v1 | Frozen |
| IndexArtifact common envelope, keys, A/B pointers | Frozen |
| Four immutable index manifests | Frozen |
| ScopeDefinitionV1 | Frozen |
| ValueStoreDefinitionV1, selectors, parser plans, dependency table, plugin ABI/archive/alias | Frozen |
| ConverterDefinitionV1 | **Missing** |
| FieldIndexDefinitionV1 and strategy registry | **Missing** |
| ArtifactDirectoryNode | **Missing exact body** |
| PostingPage and ValuePage | **Missing exact bodies** |
| NvtTile and ScopeCatalogPage | **Missing exact bodies** |
| DocumentStatePage | **Missing exact body/reason registry** |
| MutationJournalSegment and IndexTaskCheckpoint | **Missing exact bodies** |
| GcArtifact envelope, kinds, keys, controls, manifests, candidate pages/deltas, receipts, evidence | **Entire exact family missing** |
| Durable GC workspace records/checkpoints | **Missing exact formats** |
| Root provenance and semantic-state controls | **Missing exact formats** |
| Index registry/task/degraded state | **Missing exact format** |
| Lifecycle last-known-good identity | **Missing exact format** |
| RepairTicket/path latch and migration lease | **Missing exact formats** |
| Capability bit assignments | **Missing** |
| Logical position-token wire codec | **Missing exact codec/bounds** |

Round 7 itself records the required order: ConverterDefinition and
FieldIndexDefinition, then page/directory/state/journal bodies. Round 9 did not
supersede those remaining rounds.

The phrase "literal fingerprints/defaults will be selected at a pre-writer
gate" is not enough for an executable plan. Before the first writer, the plan
must also freeze:

- built-in converter/strategy IDs and complete numeric/string/trigram/phonetic
  semantics;
- finite corrected parser/mapper policy defaults;
- native semantic fingerprints and their reproducible conformance process;
- every new operational configuration field, default, range, source
  precedence, dynamic-versus-restart behavior, and whether it contributes to
  semantic identity; and
- GC audit retention, workspaces, checkpoints, memory shares, publication
  batches, page/tile/cache limits, and query-position bounds.

These are senior-engineering decisions that can be self-decided with evidence;
they do not all require owner questions. They cannot be left for workers to
guess.

## Blocker 4: Root Provenance Needs a Bounded Lifecycle

Root provenance is required to distinguish a former namespace root from an
arbitrary internal DirectoryIndex hash, but the current record has no exact
format or retention model. Recording every HEAD forever is unbounded; deleting
provenance with the root loses deterministic `ROOT_EXPIRED` behavior.

The format round must define a paged/compacted provenance catalog whose live
entries do not pin namespace closures. Pending roots remain classified through
quarantine state. After physical reclamation, compact expiry tombstones may be
retained through the approved summary/audit budget. Once that bounded evidence
expires, the API must explicitly define whether an ancient unknown hash returns
`INVALID_NAMESPACE_ROOT` rather than pretending AeorDB remembers it forever.

Category 4 also supersedes the earlier candidate body: a pending candidate now
stores `grace_at_pending_ms` in addition to `pending_since_ms` and
`first_unreachable_generation`. Every GcArtifact layout and parent-plan example
must use the three-field contract.

## Blocker 5: System Roots Need One Typed Transfer/Retention Registry

Round 9 correctly calls for one system-root registry, but lists only GC,
backup, verify, repair, and migration. Current code has divergent hard-coded
path lists in GC, tree walking/replication, and backup, and some walkers skip
errors. The final registry must classify every system family for:

- GC rooting and reference traversal;
- physical backup and logical database backup;
- peer replication, cluster join, and system sync;
- user/client sync exclusion;
- export/import and optional index transfer;
- verify, scrub, repair, migration, and emergency spill;
- credential sensitivity and redaction; and
- namespace/semantic-root participation.

Plugin modules referenced by authoritative semantic state must transfer with a
database-semantic backup or peer replication even when derived index pages are
omitted. A deliberately data-only export may omit them, but must declare the
restored semantic/index state unavailable rather than claim rebuildability.
Credentials remain node/database-local under their existing security policy.

Unknown or damaged registered system roots must not be silently skipped from a
successful backup, sync, mark, or migration. Each operation needs a typed
complete/degraded/failed result appropriate to its safety boundary.

## Blocker 6: Write Acknowledgement and Index Visibility Need One Contract

Recent production incidents showed that synchronous whole-index mutation can
make `/blobs/commit` wait tens of seconds. The target design says indexes are
derived and buffered, but the parent plan does not freeze the acknowledgement
boundary strongly enough.

The corrected contract should be:

1. a successful ordinary write means authoritative chunks/FileRecord/namespace
   publication met its durability contract;
2. before the new HEAD becomes query-observable, the index coordinator records
   in bounded memory that active generations no longer cover that root;
3. parsing, conversion, page mutation, and index soft publication run
   asynchronously unless a caller explicitly requests a future
   `wait_for_index` behavior;
4. a query may use an index only if manifest plus overlay covers its admitted
   root, otherwise it performs exact fallback or returns typed reconciling/
   unavailable state;
5. queue/admission failure marks reconciliation required and is visible in the
   write response/status, health, task state, metrics, and SSE where applicable;
6. restart detects source-root disagreement even if the volatile scheduling
   notice was lost; and
7. a derived flush failure degrades only affected indexes, while a real WAL/
   filesystem sync failure invokes the existing database-wide durability latch
   and emergency spill policy.

No route, embedded write, merge, restore, plugin host call, or blob/batch path
may maintain a second acknowledgement rule.

## Blocker 7: Directory Listing Order Was Accidentally Overwritten

Category 3 says the default order is canonical path ascending for query,
search, **and listing**. Current public directory listing behavior, backed by a
specific regression test, is:

- directories always precede files/symlinks;
- requested ascending/descending direction applies only within each category;
- default name comparison is case-insensitive; and
- pagination occurs after this categorical ordering.

This behavior fixed the exact "folders appear only after scrolling" incident
and preserves directories-first even for descending order. The stable-root
refactor must not regress it.

The final API contract should keep a common pagination parameter matrix but
route-specific total orders:

- directory listing: fixed directory category rank, then selected field/order,
  then canonical name/path and immutable incarnation tie-breakers;
- query/search: their declared order, defaulting to canonical path order plus
  immutable tie-breaker.

The logical position token's order fingerprint must include the route,
category policy, collation/case semantics, direction, null/multi-value policy,
and tie-breaker semantics.

## Required Campaign Re-Phasing

The July parent began as an NVT refactor. The decisions now encompass a whole
database format rewrite, side-by-side multi-terabyte migration, GC replacement,
semantic-state rooting, plugin ABI/archive, query API cutover, and index-engine
replacement. Keeping that as one linear "Phase 3" is not a credible revert or
ownership unit.

Regenerate it as an umbrella campaign with independently green plans:

1. **P0: Baseline and agreement oracles**
   - Capture current behavior, production incidents, memory/performance
     baselines, format fixtures, and recent-fix regression ledger.
2. **P1: Storage format compatibility**
   - V3/v4 database dispatch, v0/v1 entity dispatch, golden readers first,
     capabilities, sequence reservation, and no artifact writers.
3. **P2: Side-by-side v4 migration substrate**
   - Clone, mutation capture, progress/ETA/cancel, durability latch/spill,
     validation, cutover, rollback, Windows directory-sync proof, and copied
     production evidence tests.
4. **P3: Semantic read-view and system-root model**
   - Semantic state/catalog, structural root admission and lifecycle, plugin archive, authoritative
     backup/replication, snapshot/fork semantics, and historical resolver.
5. **P4: Bounded GC and physical retirement**
   - Exact GcArtifact/workspace codecs, logical mark bitmap, retirement
     inventory, quarantine, receipts, corruption policy, and repair controls.
6. **P5: Index semantic and artifact codecs**
   - Converter/FieldIndex definitions, every page/directory/journal format,
     readers, verify/repair, and all writers still feature-gated.
7. **P6: Bounded mutation, cache, and publication runtime**
   - One mutation-intent path, async acknowledgement, page cache/memtables,
     eviction, physical coalescing, and memory coordinator.
8. **P7: NVT and query execution**
   - Sparse hints, page scans, boolean planning, sorting, pagination, root-aware
     query/search/list/fetch, locators, SDK/UI, and exact fallbacks.
9. **P8: V0-to-v1 index migration and rollout**
   - Shadow build, live catch-up, validation, dual-format rollback, canary,
     production-copy migration, and explicit finalization.

Each child plan needs a baseline commit, owned/forbidden files, start gates,
TDD test files, exact commands, one-phase revert units, full-suite gates, and a
durable completion report. Multi-peer activation must either define a rolling
capability protocol or refuse v1 activation until every configured peer is
compatible; single-node assumptions cannot silently corrupt cluster behavior.

## Parent Plan Corrections Required Before Ratification

The formal July 16 plan currently contains at least 17 stale references to a
base64 root cursor/cursor TTL. It also still says the active pointer is the
format registry, even though Round 4 moved lifecycle/degraded state into a
separate versioned system registry. It must be regenerated, not patched one
sentence at a time.

The rewrite must also:

- replace root cursor with plain `root_hash`, response root metadata, and a
  separately frozen logical position token;
- distinguish physical v3/v4 framing dispatch from per-type EntityVersion
  dispatch, because legacy v3 entries may already carry payload version `1`;
- state that index-format migration does not semantically rewrite FileRecords,
  while the prerequisite whole-database v4 migration necessarily rewrites
  physical entity framing;
- remove old `maximum_cursor_ttl` fields and use frozen pending-candidate grace;
- replace all two-field candidate examples with the final three-field form;
- replace obsolete artifact kind names, migration leases in IndexArtifact,
  and pointer-owned registry state;
- include system semantic roots and plugin modules in snapshot/backup/
  replication reasoning;
- protect existing folders-first listing behavior and bot locator-to-range
  workflows; and
- add explicit `superseded by` links to old NVT, data-model, parser, pagination,
  GC, and storage documents whose literal statements no longer hold.

## AGIS Territory Map

### Producers

- every file mutation: PUT, streaming finalize, blob commit, batch/embedded
  store, merge, copy, move, rename, restore, import/sync, plugin host write,
  delete, reindex, snapshot restore, and fork promotion;
- config, parser-registry, plugin alias/module, and lifecycle mutations;
- index builds, memtable flush, page split/compaction, NVT healing, migration,
  repair, and reconciliation;
- HEAD, snapshot, fork, migration root, provenance, and quarantine publication;
- GC mark, mutation catch-up, sweep, receipt, evidence, retention, and Void
  reuse; and
- v3-to-v4 and v0-to-v1 migration/cutover paths.

### Consumers

- query, search, listing, direct file GET, batch/range fetch, locators,
  sorting, aggregation, pagination, EXPLAIN, SDK, web UI, and bots;
- startup, dirty rebuild, counters, dashboard, metrics, health, task status,
  cancellation, and shutdown;
- GC, backup/export/import, physical copy, replication/client sync, cluster
  join, snapshots/forks, verify, scrub, repair, and emergency spill;
- parser/plugin runtime and compiled-module cache; and
- Linux, macOS, and Windows codecs/installations.

Missing any one of these paths can produce false negatives, mixed-root results,
immortal disk usage, premature Void reuse, incomplete backup/replication, or a
format that only works on the machine that wrote it.

## AGIS Test Protocol and Plan Tests

Current source has roughly 40 directly relevant spec files covering v0
index/query/GC/backup/sync/plugin/header behavior. They are characterization
inputs, not proof of the proposed v1 formats.

### Core hypotheses

1. Given any missing, stale, corrupt, resized, or empty NVT, v1 query results
   exactly equal the authoritative reference evaluator.
2. Given any crash point during v3-to-v4 or v0-to-v1 migration with concurrent
   writes, reopening selects one valid state, loses no acknowledged data, and
   retains an explicit rollback/retry path.
3. Given a 16 GiB host with swap disabled and many large indexes, all engine
   work remains below configured RSS/memory limits without silently dropping
   coverage.
4. Given one namespace root across parser/config/plugin changes, a historical
   query uses exactly the semantic state committed by that root or returns the
   approved typed unavailable error, never current semantics.
5. Given incomplete/corrupt marks, layout changes, lower physical revisions,
   reader races, and restart, GC never reclaims a reachable or insufficiently
   proven incarnation.

### Proof spine

- Hand-authored golden fixtures and corrupt variants for every frozen byte on
  Linux, macOS, and Windows before production writers.
- Property/differential tests against independent converter, parser, NVT scan,
  query, pagination, GC, and migration reference models.
- Fault injection at every append, sync, pointer/control publication, KV
  locator replacement, retirement journal, workspace checkpoint, cutover,
  backup/import, and shutdown boundary.
- Authorization/non-observability tests before counts, root metadata, cursors,
  snippets, timing, and EXPLAIN.
- Memory and disk stress with swap disabled, cache churn, many unused indexes,
  parser amplification, GC, migration, and query concurrency.
- Crash/restart soak with small files, merges, blob commits, config/plugin
  changes, GC, B-tree damage, and forced process kills.
- Real `/tmp/codex` HTTP plus embedded SDK tests and a migration of a verified
  copy of the FS-Server1 database before canary activation.
- Mechanical architecture/grep gates for one mutation path, one read-view
  resolver, one system-root registry, one parser resolver/executor, one GC
  retirement path, no whole-index v1 cache, no current-semantics historical
  fallback, and no ignored correctness-bearing errors.

### Recent-fix regression ledger

The campaign must name and preserve at least:

- large-index commit-latency reduction;
- clean-index cache eviction and memory dashboard attribution;
- B-tree walk/repair and GC mark hardening;
- stale-index retirement during reindex;
- durability latch, emergency spill, and startup spill refusal;
- exact index query behavior and whole-file `@hash` semantics;
- search hit locators plus batch range fetch;
- folders-first pagination in both ascending and descending order;
- system-path backup/sync security exclusions; and
- truly lazy one-chunk-at-a-time ordinary file streaming.

Authoritative counters/tree walks that skip damaged B-tree branches must return
an explicit incomplete/degraded status. Logging a warning and presenting a
smaller count as complete violates AeorDB's honest-guarantees rule.

### Plan-test result

| Assertion | Result |
| --- | --- |
| NVT direction matches AeorDB's original purpose | PASS |
| KV redesign remains out of scope | PASS |
| Authoritative/derived boundary is coherent | PASS, except semantic definitions need an authoritative root |
| Every persistent format is frozen | **FAIL** |
| `root_hash` identifies one immutable query world | **FAIL** |
| GC tracks/reclaims every physical incarnation safely | **FAIL** |
| Backup/replication include all authoritative dependencies | **FAIL** |
| Public API/order contracts preserve recent behavior | **FAIL** |
| Migration and rollback are recognized | PASS, but unsafe phasing remains |
| Testing ambition is sufficient | PASS |
| A new worker can execute P0/P1 without guessing | **FAIL** |

## Remaining Decision Rounds

The shortest path to a genuinely final plan is:

1. **Round 10:** semantic read-view root, immutable semantic state, root lifecycle, and
   authoritative system transfer;
2. **Round 11:** ConverterDefinitionV1, FieldIndexDefinitionV1, complete
   strategy/converter registries, and corrected runtime defaults;
3. **Round 12:** every IndexArtifact directory/page/state/journal/checkpoint
   body and logical position-token codec;
4. **Round 13:** physical retirement inventory plus every GcArtifact and durable
   workspace format;
5. **Round 14:** remaining low-volume controls, capability bits, consolidated
   operational configuration, and cross-platform cutover contracts; and
6. **Final synthesis:** regenerate the formal umbrella/child plans, run all
   mechanical plan tests, and only then mark the campaign ratified.

Most of these are technical decisions suitable for another `planning-cap` and
AGIS self-decision pass. The owner should only be asked where a real product or
operational tradeoff remains; workers must not be asked to improvise persistent
bytes or public guarantees during implementation.

---

# Planning-Cap Remediation Campaign: Closing Every Ratification Blocker

**Status:** Proposed campaign architecture after a fresh Planning Cap and AGIS
pass. This section plans how every remaining issue is closed. It is not yet an
implementation authorization and it does not waive the exact-format gates in
Rounds 10 through 14.

**Baseline inspected:** `5d3e284652f9fec7a5c843f1946132574af4d469`

## 1. Target, Non-Goals, and Judging Standard

The target is one migration-safe AeorDB v4 architecture in which a read root
identifies exact namespace and semantic state, derived indexes remain bounded
and disposable, GC can prove physical reclamation without unbounded memory,
and every write/read/maintenance surface follows one visibility and durability
contract.

The campaign is judged by externally provable behavior:

1. One admitted `root_hash` always means one immutable namespace and one exact
   semantic world.
2. Missing, stale, corrupt, or absent indexes can only reduce performance, not
   alter results.
3. GC cannot reclaim an authoritative or insufficiently proven physical
   incarnation, including lower WAL revisions hidden by the current KV locator.
4. A 16 GiB host can operate below the approved 6 GiB soft and 8 GiB hard
   process envelopes with swap disabled.
5. An acknowledged write is durable under the authoritative storage contract
   and does not synchronously parse or mutate whole indexes.
6. A side-by-side migration of a verified production copy survives crash,
   cancellation, restart, validation failure, and rollback without modifying
   the source.
7. Linux, macOS, and Windows read and write byte-identical v1/v4 formats and
   satisfy their platform durability contracts.

Explicit non-goals remain:

- redesigning the current KV page/NVT layout;
- GPU query execution;
- positional indexes;
- in-place conversion of a v3 database;
- making derived index generations part of namespace snapshots; and
- preserving unsupported legacy behavior by running two permanent code paths.

The plan also keeps the established AeorDB principles: content and namespace
are authoritative, NVT is a lossy navigation hint, indexes are rebuildable,
large data is streamed, publication is dependencies-first/pointer-last, and
correctness-bearing errors are never replaced by warnings or defaults.

## 2. AGIS Territory Result

The fresh code search confirmed that this is not only an index refactor. The
same contracts are produced by or consumed by all of the following families.

### Producers

- ordinary file PUT, streaming finalize, blob commit, buffered batch, embedded
  write, merge, copy, move, rename, delete, restore, import, sync, and plugin
  host writes;
- directory COW propagation and HEAD publication;
- absolute and inherited config, parser registry, plugin alias/module, native
  semantic profile, and lifecycle mutation;
- index extraction, mutation admission, memtable flush, page split/merge,
  compaction, NVT healing, reconciliation, reindex, migration, and repair;
- snapshot, fork, migration, maintenance, provenance, and root-pin publication;
- KV stable-key replacement, dirty recovery, WAL inventory, GC mark/catch-up,
  quarantine, sweep, receipt, evidence, and Void publication; and
- v3-to-v4 and v0-to-v1 cutover/finalization.

### Consumers

- query, search, listing, direct GET, batch/range fetch, snippets/locators,
  sorting, aggregation, pagination, and EXPLAIN;
- HTTP routes, embedded SDKs, bundled web UI, sync clients, bots, and peer
  nodes;
- startup/dirty rebuild, health, metrics, dashboard, counters, task status,
  cancellation, graceful shutdown, and emergency spill;
- backup/export/import, replication, cluster join, client sync, snapshots,
  forks, verify, scrub, repair, and migration; and
- Linux, macOS, and Windows readers/writers.

The current implementation contains the exact divergence the audit predicted:

- `system_store.rs` says protected data participates in replication/versioning,
  while `tree_walker.rs` documents that absolute protected roots are outside
  HEAD and then manually enumerates only selected families;
- backup, tree walking, sync, route filtering, cache invalidation, and GC each
  carry different path lists and different skip/fail behavior;
- directory writes intentionally stop absolute system-path propagation at `/`;
- backup and tree walking still contain warning-and-continue paths that can
  report an incomplete protected-data transfer as success; and
- the current KV bitmap can name only active logical locators, not every WAL
  incarnation consuming physical space.

These observations determine the ownership model below. Patching the current
lists or adding another route-local hook would preserve the bug class.

## 3. Contract Stack and Single Owners

Every implementation child plan must preserve these ownership boundaries.

| Contract | Single owner | Forbidden duplicate behavior |
| --- | --- | --- |
| Read-view identity | `SemanticReadViewResolver` plus typed `NamespaceRootV1` | Current-config fallback in historical routes; route-local root adoption |
| Semantic compilation | `SemanticStateCompiler` | Direct runtime reads of mutable parser/plugin aliases when evaluating an admitted historical root |
| Root admission/lifecycle | `SemanticReadViewResolver`, typed `NamespaceRootV1`, GC candidate state, and bounded post-reclaim `RootExpiryCatalog` | Treating any internal DirectoryIndex hash as `/`; per-route expiration guesses; per-HEAD provenance ledgers |
| Protected system families | `SystemFamilyRegistry` | Hard-coded backup/sync/GC/tree-walker path arrays |
| Namespace mutation and acknowledgement | `NamespaceMutationCoordinator` | Route-specific indexing or durability acknowledgement rules |
| Index coverage | `IndexCoverageCoordinator` | Advertising a pointer merely because its pages decode |
| Stable-key replacement | `LocatorReplacementCoordinator` | Direct KV replacement that omits displaced-incarnation retirement |
| Physical reclamation | `GcCoordinator` and versioned `GcArtifact` state | Treating current KV-slot liveness as complete WAL liveness |
| Artifact codecs | Per-kind bounded readers/writers under one registry | `serde`, `bincode`, native endian, body-length dispatch, or worker-chosen bytes |
| Memory admission | Process-wide `MemoryCoordinator` | Independent cache/task budgets that each assume the same RAM |
| Format migration | `DatabaseMigrationCoordinator` | Startup migration, in-place v3 rewrite, or untracked dual writers |

The names are design roles, not a demand for ten giant structs. A role may be
implemented by a focused module and facade, but all producers must enter the
same contract boundary.

## 4. Corrections Planned for the Seven Audit Findings

### 4.1 Semantic state participates in every v4 root

The preferred representation is not a hidden path. A reserved root-level file
would create collision, filtering, copy/move, counting, backup, and SSE rules
for the lifetime of the format. V4 already has explicit `(EntryType,
EntityVersion)` dispatch, so Round 10 should freeze a typed
`NamespaceRootV1` DirectoryIndex/root-envelope variant instead.
This supersedes only the final audit's suggested hidden-path representation;
it preserves the audit's required invariant that semantic state contributes to
the public root identity.

Every v4 public `root_hash` identifies that immutable envelope. It contains at
least the authoritative namespace-tree root hash and one immutable canonical
`SemanticStateRootV1` hash. The exact bytes are frozen in Round 10, but these
properties are not optional:

- the semantic-state hash participates directly in the public `root_hash`;
- internal directory/B-tree roots have a different role/discriminant and can
  never be reinterpreted as namespace roots;
- the envelope is engine-created structural metadata, not a file path, so it
  cannot appear in listing/search/counts/SSE or be mutated by file/plugin APIs;
- the semantic-state root identifies the exact effective index/scope
  definitions, parser registry snapshot, parser/mapper plans, native semantic
  fingerprints, plugin alias identities, and archived module digests needed
  for deterministic evaluation;
- auth users, API/JWT secrets, runtime lifecycle policy, scheduling, cache
  limits, and other node-local operational state are excluded; and
- changing semantically relevant config or code publishes a new semantic root
  and therefore a new namespace root even when user file content is unchanged.

The semantic root contains canonical identities and Merkle references rather
than timestamps or mutable display metadata. Repeating the same semantic world
produces the same identity. Definitions and executable dependencies are
authoritative inputs and therefore live outside `IndexArtifact`; derived pages
remain outside the namespace tree.

Semantic catalog compilation is paged, incremental, memory-admitted, and
shadowed. A parser registry, plugin alias, global semantic config, or inherited
index-config mutation may fan out across many scopes, so it must not hold the
namespace publication guard while recompiling the world. The mutation is first
validated and durably staged outside the active read view; the compiler builds
the exact new semantic catalog under a task/checkpoint; one short final commit
publishes the staged control/config plus `SemanticStateRootV1` and
`NamespaceRootV1` together. Failure, cancellation, or restart leaves the old
semantic world active and reports the staged task. Every HTTP, embedded,
transform/import/sync, and plugin route that can mutate semantic inputs uses
this path and returns the Round 10 typed accepted task. Selecting an
already-complete exact v4 snapshot/fork root may use the coordinator's short
synchronous authority path because it performs no semantic compilation.
Neither path may expose half-old/half-new semantics.

Historical resolution starts from the selected `NamespaceRootV1`. It never
consults the current mutable registry as semantic authority. A missing exact
dependency returns the already-approved typed historical/dependency error; substituting a
newer parser because it is available is forbidden.

### 4.2 Root admission is structural; expiry evidence is bounded

Round 10 closes this blocker without a per-HEAD provenance ledger.
`NamespaceRootV1` is the immutable structural proof that distinguishes a
complete namespace root from an internal DirectoryIndex. Live authority, GC
candidate state, and transient request pins determine lifecycle state. A root
becomes discoverable only when its entity and first authority are committed
atomically; a prepared or aborted raw tail is quarantined, not admitted. After
that first authority is removed, the committed root may remain retained.

Publication remains dependencies-first and authority-last. Reopen validates
the selected authority's root envelope and immediate typed closure; it does not
need a second ledger to repeat the same publication fact. Origin, parent,
timestamp, and display metadata remain outside root identity.

After physical reclamation, a paged/compacted `RootExpiryCatalog` retains
bounded evidence for deterministic `ROOT_EXPIRED`. Its records do not pin the
reclaimed closure. Once the age/byte budget expires, an absent ancient hash
returns `INVALID_NAMESPACE_ROOT`; AeorDB does not claim infinite historical
memory. If GC is disabled, former roots remain retained and no per-mutation
provenance stream grows.

### 4.3 One typed protected-system registry replaces path folklore

`SystemFamilyRegistry` is the sole classification table for every protected
family. Each descriptor freezes:

- canonical matcher and one binary-owned family handler;
- whether it participates in the namespace semantic root;
- GC root/reference traversal and retention class;
- physical backup, semantic database backup, data-only export, import, peer
  replication, cluster join, and client-sync behavior;
- verify, repair, migration, spill, redaction, and credential sensitivity;
- whether absence is valid, degraded, or fatal for each operation; and
- whether the family may emit user-visible events or metrics.

The registry is binary-owned, versioned policy with permanent family IDs and a
canonical fingerprint recorded in v4 capabilities/semantic state where
relevant. It is not a mutable database file that damaged data or a plugin can
edit to weaken GC, backup, or credential rules. Extensible payloads may be
registered only through a future capability-gated registry version.

Public path denial remains prefix based so an unregistered child cannot become
visible accidentally. Maintenance behavior is fail closed: an unknown or
damaged protected family is conservatively retained by GC and causes a typed
incomplete/failed semantic backup, replication, verify, or migration result.
It is never warning-and-continue success.

Database-semantic backup/peer transfer includes semantic definitions, parser
registry state, plugin aliases, and every referenced module even when derived
indexes are omitted. Data-only export may omit all of them only while declaring
that semantic query/index state is unavailable and needs explicit
reconfiguration. Credentials and JWT/API-key secrets remain local under their
existing transfer policy.

### 4.4 Logical KV liveness and physical WAL retirement are separated

The Category 2 bitmap wording is superseded in one narrow but important way:
the dense `(bucket, slot)` bitmap is the exact liveness projection for active
logical KV locators in the captured layout. It is not a bitmap of all physical
WAL incarnations.

Physical retirement uses a second bounded proof path:

1. every stable-key replacement enters `LocatorReplacementCoordinator`;
2. the coordinator captures old and replacement key, offset, write sequence,
   length, type, reason, and publication boundary in a buffered retirement
   journal;
3. journal segments flush on bounded count/byte/time thresholds and publish a
   durable audited-through sequence/offset watermark;
4. dirty recovery scans from the last proven watermark through the bounded hot
   region, groups duplicate keys, selects the highest valid incarnation, and
   synthesizes missing lower-incarnation retirement evidence;
5. a checkpointed external-sort WAL inventory eventually audits older regions
   without loading the WAL or KV into memory;
6. discovered retired/orphan incarnations enter timestamped quarantine and the
   same later-complete-mark/grace/recheck rules as other candidates; and
7. missing or corrupt retirement evidence leaks disk and degrades GC, but can
   never authorize early Void reuse.

Pointer/control rewrites, ordinary relocations, repair, migration, and future
stable-key formats all use this one replacement path. Direct low-level KV
replacement remains available only inside recovery/bootstrap code with an
explicit proof object and architecture test.

### 4.5 Every persistent byte receives a pre-writer specification

Rounds 11 through 14 freeze every format listed as missing in the audit. The
first production writer stays mechanically disabled until:

- all exact byte tables, kind IDs, key domains, bounds, checksums, canonical
  ordering, malformed-input behavior, and capability bits are in the plan;
- hand-authored golden fixtures exist independently of production writers;
- readers pass all valid/corrupt fixtures on Linux, macOS, and Windows; and
- writers reproduce those bytes exactly.

This includes converter/index definitions, page/directory/state/journal kinds,
the entire `GcArtifact` family, durable workspace records, provenance and
semantic controls, registry/task/degraded controls, lifecycle last-known-good,
repair/migration controls, and logical position tokens.

### 4.6 One acknowledgement and index-visibility contract

`NamespaceMutationCoordinator` owns all authoritative mutations. Its contract
is:

1. reserve bounded mutation/coverage state before making a new HEAD visible;
2. durably publish chunks, FileRecords, directory closure, semantic root when
   affected, provenance, and HEAD under the authoritative durability contract;
3. report success only after that contract succeeds;
4. install the root/scope coverage transition before releasing the publication
   guard, so no query can observe new HEAD with an index falsely marked current;
5. enqueue parsing/conversion/index work asynchronously;
6. when overlay/queue admission cannot be retained, mark the affected scope as
   reconciling/needs-rebuild rather than blocking or lying;
7. expose indexing admission/reconciliation in write response metadata, task
   state, health, metrics, diagnostics, and typed SSE; and
8. on restart, derive stale coverage from manifest source roots and durable
   mutation/reconciliation controls even if volatile scheduling disappeared.

An ordinary successful write promises authoritative durability, not immediate
index completion. Query execution may use a generation only when its manifest
plus admitted overlay covers the selected root. Otherwise it performs exact
fallback or returns the already-approved typed unavailable/reconciling result.

A derived flush failure degrades only the affected index. A WAL, hot-tail,
DatabaseHeader, or filesystem synchronization failure still invokes the global
durability latch, read-only transition, and emergency spill policy.

### 4.7 Route-specific total ordering is preserved

The common pagination matrix does not imply one total order for every route.
Round 12 freezes:

- directory listing: directories first regardless of ascending/descending,
  selected order within category, current case-insensitive default name
  semantics, then canonical path/name and immutable incarnation tie-breakers;
- query/search: declared order, defaulting to canonical path ascending, then
  immutable incarnation tie-breaker; and
- position-token order fingerprints that include route, category policy,
  collation/case behavior, direction, null policy, multi-value policy, and
  tie-breaker definition.

Pagination is applied only after the route's total order. The existing
directories-first descending regression test is a permanent guarding spec.

## 5. Additional Gaps Found During This Pass

The seven audit findings were complete as categories, but two migration
consequences need explicit treatment rather than being left to implementers.

### 5.1 V3 roots do not contain reconstructable historical global semantics

Absolute parser/plugin/global config state is not in current HEAD, and current
code explicitly says historical system state cannot be reconstructed from an
old user root. Migration must not claim otherwise.

The recommended honest policy is:

- current HEAD receives one exact migration-captured v0 semantic state at the
  v4 cutover boundary;
- retained legacy snapshots/forks preserve direct file, listing, and range-read
  content after migration;
- a legacy root receives semantic query/search support only when migration can
  prove the exact dependency bundle for that root;
- otherwise semantic query/search returns `HISTORICAL_VIEW_UNAVAILABLE` rather
  than evaluating the old namespace with migration-time or current semantics;
  and
- snapshot/fork restore creates a new v4 root with an explicitly selected
  semantic state; it does not silently pretend the legacy root hash already
  contained that state.

This is a product-visible limitation, but it is the only policy compatible with
AeorDB's honest-guarantees rule. Round 10 must record it explicitly.

### 5.2 V4 semantic anchoring changes externally held root hashes

Wrapping legacy roots in `NamespaceRootV1` changes every migrated root hash.
Migration must therefore inventory persistent callers and retained roots,
produce a verified legacy-to-v4 root mapping for current HEAD/snapshots/forks/pins it actually
knows, and update database-owned references during cutover.

An external client may hold an arbitrary former v3 HEAD unknown to the
database. AeorDB cannot manufacture a trustworthy mapping for it. Such callers
receive an explicit format-migration/reset response and perform a bounded
resynchronization; the server must not silently reinterpret the old hash or
quietly treat it as an empty base. Sync APIs need a specific reset-required
contract so this does not become an accidental multi-terabyte full transfer.

## 6. Remaining Design Rounds

These rounds are dependency ordered. Each round ends with exact contract text,
hand-auditable tables where applicable, test obligations, parent-plan
corrections, and a clear CLOSED or OPEN status.

### Round 10: Semantic read views, root lifecycle, and protected transfer

Freeze:

1. exact `NamespaceRootV1` role/discriminant/body, namespace-tree reference,
   semantic-state reference, key/hash identity, and v3-to-v4 wrapping behavior;
2. `SemanticStateRootV1`, semantic catalog/definition references, canonical
   identity, paged/incremental shadow compilation, staged mutation API state,
   and atomic activation protocol;
3. semantic inclusion/exclusion matrix, including inherited config, global
   registry, plugin aliases/modules, native semantics, and operational/auth
   exclusions;
4. structural root admission, live/retained/pending lifecycle,
   `RootExpiryCatalog`, request-pin/sweep ordering, retention,
   compaction/write amplification, GC-disabled behavior, crash recovery, and
   root-state lookup;
5. `SystemFamilyDescriptorV1` registry and the complete transfer/retention
   matrix for every current protected family;
6. legacy-root semantic availability and sync reset/mapping behavior; and
7. one `ResolvedReadView` contract used by HTTP and embedded surfaces.

Exit gate: given any retained v4 root, a reader can mechanically resolve one
semantic state or one stable typed error without reading current mutable
semantic controls. Backup/replication/GC/verify derive their protected-family
behavior from one registry.

### Round 11: Converter, strategy, and definition semantics

Freeze:

1. exact `ConverterDefinitionV1` and `FieldIndexDefinitionV1` byte layouts;
2. permanent converter and strategy ID registries;
3. numeric, bytes, string/collation, trigram, fuzzy, phonetic, temporal,
   boolean, null, missing, and multi-value semantics;
4. canonical equality/order, scalar collision, range endpoint, NaN/infinity,
   Unicode, locale, tokenizer, and duplicate-value behavior;
5. built-in semantic fingerprints and the reproducible conformance process;
6. definition identity participation for every parser/converter/strategy
   limit and policy; and
7. exact bounded defaults/ranges for value size, expansion, token count,
   posting count, parser/mapper resources, and query complexity.

Exit gate: an independent reference evaluator and production candidate produce
the same canonical values, identities, coordinates, and comparison results for
all golden/property cases. Any semantic change creates a new definition ID.

### Round 12: Index pages, journals, and public position tokens

Freeze exact identity/body layouts and invariants for:

- `ArtifactDirectoryNode`;
- `PostingPage` and `ValuePage`;
- `NvtTile` and `ScopeCatalogPage`;
- `DocumentStatePage` plus stable reason IDs;
- `MutationJournalSegment` and `IndexTaskCheckpoint`;
- page links/range fences/rank metadata/physical span hints;
- split, merge, compaction, non-reuse, and shallow/deep validation; and
- the logical `after`/`before` position-token wire codec and every route order
  fingerprint.

Exit gate: page scans remain correct with empty/stale/corrupt NVT, continue in
both directions without whole-index loading, preserve directories-first
listing, and paginate without duplicate/omitted records against an independent
reference model.

### Round 13: Physical retirement and complete GC formats

Freeze:

1. permanent `GcArtifact` kind IDs, envelope, key domains, and A/B controls;
2. quarantine manifests, compacted candidate pages, bounded deltas, and the
   final three-field candidate state:
   `pending_since_ms`, `first_unreachable_generation`, and
   `grace_at_pending_ms`;
3. retirement journal segments, audited-through watermarks, lower-incarnation
   recovery records, and physical inventory checkpoints;
4. mark-run controls, root/layout/config captures, active logical-locator
   bitmap metadata, frontier/path/candidate run formats, and external workspace
   manifests;
5. sweep proposal/commit receipts, recovered receipts, corrupt evidence,
   detail/summary catalogs, pins, and retention compaction;
6. exact append/KV/sync crash state machines and Void revalidation; and
7. legacy source, v4 clean-copy, incomplete mark, damaged B-tree, cancellation,
   scratch exhaustion, and restart behavior.

Exit gate: a model checker/fault-injection harness can interrupt every state
transition and prove that the only unsafe-direction failure is leaked space.
No interruption or corrupt/missing artifact can make reclamation earlier.

### Round 14: Controls, capabilities, config, and cutover

Freeze exact formats and policies for:

- index registry/build/reconciliation/degraded state;
- lifecycle last-known-good identity and effective-source diagnostics;
- repair ticket/path latch, migration lease/progress/root map, task pins, and
  semantic/control mutation records;
- first-authority root-publication transaction/commit markers, dirty-rebuild
  admission rules, and the canonical expanded `SystemFamilyRegistry` fixture;
- DatabaseHeader v4 capability-bit assignments and minimum reader/writer rules;
- cluster capability negotiation and refusal of mixed incompatible writers;
- operational config names, default/min/max, source precedence, dynamic versus
  restart behavior, semantic-identity participation, and last-known-good
  handling;
- cache/memory/GC workspace/checkpoint/publication/prefetch/query-position
  budgets; and
- Linux/macOS/Windows durability and side-by-side cutover behavior, including a
  real Windows parent-directory synchronization implementation.

Environment and CLI overrides are resolved per property before declaring a
stored configuration value missing or fatal. A malformed stored document may
be bypassed only for properties supplied by a higher-precedence valid source;
the malformed stored source remains visible as degraded diagnostics.

Exit gate: old/incompatible binaries reject before writing, compatible peers
agree before activation, and every control survives crash/reopen with one
deterministic selected state or one fail-closed typed error.

### Final synthesis

After Rounds 10 through 14 close:

1. regenerate, rather than patch, the July 16 parent plan;
2. split it into the child plans below;
3. add `superseded by` banners to stale NVT/storage/GC/parser/pagination plans;
4. run mechanical plan tests against every owner ruling and audit blocker;
5. create worker briefs, exact commands, owned/forbidden files, and progress
   records; and
6. ratify only if a new worker can execute P0 and P1 without guessing a byte,
   policy, migration action, or test oracle.

## 7. Executable Implementation Campaign

No implementation phase starts until final synthesis is ratified. Each phase
is an independently green, revertable landing unit with tests written first.

### P0: Baseline and independent oracles

- Pin the baseline commit and inventory all persisted callers/root hashes.
- Extract verified v3/v0 fixtures from copied databases; never mutate evidence.
- Capture current query/list/search/fetch, backup/sync, memory, GC, startup,
  blob-commit, and index-latency behavior.
- Run old-versus-old captures first, classify ordering/time/identity noise, and
  establish a machine-readable intended-change allowlist. Selection noise is
  not hidden by sorting; every residual behavior difference receives an owner
  divergence ruling.
- For migration, GC, backup, and repair paths that cannot safely run against
  live data, capture and compare a dry-run operation ledger containing ordered
  operation kind, identity, byte/count totals, and intended durability barrier.
- Turn known defects into target-failing tests, not characterization tests.
- Build independent reference models for codecs, converters, query ordering,
  pagination, root state, GC eligibility, and migration.
- Freeze the recent-fix regression ledger as named guarding specs.

### P1: Reader-first format and capability foundation

- Implement v3/v4 DatabaseHeader dispatch and v0/v1 EntityHeader dispatch.
- Build the shared bounded-codec and independent-fixture harness.
- Add bounded readers for core header/capability/low-volume control fixtures.
  IndexArtifact and GcArtifact readers land as reader-first subphases at the
  start of P4/P5 so this phase does not become one unreviewable codec dump.
- Add permanent type/kind/capability registries and unknown-version behavior.
- Keep every new writer disabled behind its family-specific
  test-only/pre-writer gate.
- Prove byte-identical readers on Linux, macOS, and Windows.

### P2: Contract consolidation and fail-closed hardening

- Introduce `SystemFamilyRegistry`, `NamespaceMutationCoordinator`,
  `IndexCoverageCoordinator`, and `LocatorReplacementCoordinator` facades.
- Route every current producer through them while retaining v3/v0 bytes and
  all behavior not already ratified as defective.
- Delete duplicate route-local/path-list/acknowledgement logic only after
  agreement tests are green.
- Make incomplete protected-tree operations typed rather than warning success.

This phase reduces pathways before format migration. It must not begin writing
v4 artifacts or change public root hashes. Converting an incomplete protected
backup/walk from warning-success to a typed failure is an intentional defect
fix, guarded by target-behavior tests rather than characterized as success.

### P3: Shadow v4 migration substrate and semantic read views

- Require strict source verify (with explicit incomplete/corrupt findings),
  destination and workspace free-space reserve, platform durability support,
  and a projected clone/journal/rollback capacity report before the lease is
  acquired. Repair is a separate explicit operation against a copy; migration
  never silently skips damaged branches.
- Build the online clone, bounded mutation capture, checkpoints, ETA/cancel,
  source-GC suspension, captured-root validation, and rollback.
- Write v4 namespace-root envelopes, semantic definitions/dependencies,
  first-admission witnesses, bounded expiry/audit evidence, root maps,
  and low-volume controls into the destination.
- Migrate current HEAD, snapshots, forks, known pins, database-owned sync
  state, and protected system families under the exact registry policy.
- Materialize the initial destination logical set once. Subsequent source
  mutations accumulate in the durable bounded capture journal but are not
  applied as untracked stable-key replacements before P4 retirement support.
- Mutation capture has an exact disk budget and free-space reserve. If source
  write rate or migration delay exhausts either, migration checkpoints and
  aborts/degrades explicitly; it never grows without bound or weakens source
  durability. The operator is warned that source GC remains suspended while
  the lease is active.
- Keep the destination shadow-only. It cannot accept production mutations or
  replace the v3 source until P4 physical-retirement/GC support is active and
  reopen/full-verify/cross-platform durability gates pass.
- Keep the v3 source untouched and restartable until explicit finalization.

### P4: Physical retirement, bounded GC, and v4 cutover gate

- Replace current `gc_recheck`, full live `HashSet`, unbounded queues, and
  silent branch skips with the approved bounded pipeline.
- Land GcArtifact readers and independent golden fixtures before writers.
- Activate retirement journaling before allowing any stable v4 key to be
  replaced after shadow construction.
- Add durable workspaces/resume, physical inventory, quarantine, receipts,
  evidence, and repair/diagnostic tooling.
- Reconcile the P3 capture journal through the replacement coordinator, then
  perform bounded catch-up while source writes continue.
- Permit sweep only after two complete marks, frozen grace, exact incarnation
  recheck, and Void publication proof.
- Only after retirement/GC recovery survives reopen and fault injection may the
  migration coordinator perform final delta, hard validation, atomic service
  cutover, and rollback rehearsal. The destination starts `never_marked`; its
  first destructive sweep still requires fresh complete v4 marks.
- Before cutover, the v4 destination must run the complete existing v0
  query/search/list/fetch/index suite against migrated data. V4 physical
  framing and the semantic root envelope may not force premature v1 index
  activation.

### P5: Index semantic and artifact codecs

- Land IndexArtifact readers and independent golden fixtures before writers.
- Implement converter/strategy/definition compilers and bounded page codecs.
- Add reader/verify/repair support for every artifact kind.
- Implement lazy page loading, bounded physical coalescing, and cache accounting.
- Keep production pointer activation disabled while query/mutation runtime is
  incomplete.

### P6: Bounded async mutation/cache/publication runtime

- Emit one typed mutation intent from every namespace mutation.
- Install coverage transitions before HEAD visibility and process index work
  asynchronously.
- Add bounded memtables, spill/reconciliation, page split/merge/compaction,
  A/B soft/hard publication, eviction, and process-wide memory admission.
- Prove `/blobs/commit` does no synchronous parser/whole-index work.

### P7: Sparse NVT and root-aware query execution

- Implement converter-bound point/range planning, sparse tiled hints, backward
  anchor search, bidirectional page scans, exact value recheck, and cost model.
- Compose candidates by scope-local document identity and merge by FileKey.
- Add exact fallback/degraded behavior, sorting, aggregations, route-specific
  pagination, position tokens, root metadata, and range-fetch continuity.
- Cut HTTP, embedded SDK, web UI, sync client, and docs together.

### P8: V0-to-v1 shadow migration and production rollout

- Build each v1 scope from one immutable admitted root and exact semantic state.
- Catch up live writes through bounded journals, compare against authoritative
  evaluation, hard publish, reopen, and validate before selecting v1.
- Retain v0 for rollback until canary/soak/production-copy gates pass.
- Roll out to one copied production database, then one canary node, then the
  controlled deployment; finalization is explicit and irreversible only after
  the evidence report is accepted.

### P9: Retirement, documentation, and debt deletion

- Remove v0 mutation/runtime paths only after rollback retention closes.
- Delete duplicate system path lists, route-local invalidation/ack logic,
  whole-index v1 loading, current-semantics historical fallback, and direct KV
  replacement bypasses.
- Publish API/SDK/admin/migration/repair/operator/bot documentation and format
  specifications.
- Produce `dod-evidence.md` and a durable completion report with every gate,
  divergence ruling, retained debt, and operational rollback instruction.

## 8. Testing Spine

The AGIS test protocol rejects one pleasant E2E run as proof. Tests are written
before each phase and use explicit timeouts. Ordinary unit/integration tests use
the repository's short timeout discipline; long crash-soak and production-copy
tests run as named bounded harnesses with progress and hard deadlines.

### Existing characterization inputs

At minimum, preserve and extend the current specs for directory listing,
directory operations/B-tree walks, query/pagination/operators, NVT/index store,
reindex, GC, backup/export/import, sync, entry headers/repair, plugins, and HTTP
query/listing/GC/backup/sync. Existing tests are evidence of current behavior,
not proof of the new formats.

### Target hypotheses

1. Any stale/missing/corrupt NVT or derived page produces results identical to
   authoritative evaluation or a typed incomplete error, never false results.
2. Every admitted root resolves the same namespace/semantic world across query,
   search, listing, fetch, restart, backup/restore, and peer transfer.
3. Every crash point selects one valid old/new state and never loses an
   acknowledged authoritative write.
4. GC can lose work or leak space, but cannot reclaim a reachable, pinned,
   insufficiently marked, mismatched, or unproven incarnation.
5. Memory remains bounded under many cold indexes, parser amplification, GC,
   migration, query concurrency, and cache churn with swap disabled.

### Required proof families

- hand-authored valid/corrupt golden fixtures for every byte and schema on all
  platforms;
- codec fuzzing for truncation, overflow, allocation amplification, noncanonical
  ordering, duplicate IDs, invalid reserve, checksum mismatch, and trailing
  bytes;
- independent property/differential models for converter, parser, NVT scan,
  boolean query, sorting, pagination, root status, GC, and migration;
- fault injection at every append, sync, DatabaseHeader update, stable KV
  replacement, retirement flush/watermark, pointer/control publication,
  workspace checkpoint, sweep receipt, spill, cutover, and shutdown boundary;
- authorization/non-observability tests before root metadata, counts, timing,
  snippets, pagination, and EXPLAIN;
- concurrency tests for HEAD/config/plugin mutation, request pins, GC, index
  publication, backup, and shutdown;
- agreement tests proving every mutation producer enters the same coordinator
  and every protected-data consumer uses the same registry;
- architecture/grep gates banning direct mutable parser reads, direct stable KV
  replacement, whole-index v1 caches, hard-coded protected-root arrays,
  warning-success correctness paths, and route-local root adoption; and
- real `/tmp/codex` HTTP plus embedded SDK tests for each API/SDK phase.

### Resource and performance gates

- On a 16 GiB, swap-disabled profile, sustained mixed work stays below the 6
  GiB soft target and never exceeds the 8 GiB hard budget except a separately
  justified allocator measurement artifact; shutdown/spill headroom remains
  available.
- Startup, GC, migration, reindex, and query report progress and cancellation
  at bounded intervals; no hidden unbounded vector or full-index load is
  allowed.
- With identical authoritative file work, enabling many indexes may add bounded
  mutation-admission bookkeeping but may not add synchronous parsing or
  whole-index mutation to blob/file commit latency. After warm-up, the named
  benchmark's index-enabled median and p95 synchronous commit overhead must be
  no greater than the larger of 10 percent or 50 ms versus indexes disabled on
  the same commit manifest. A failing sample receives two confirmation runs and
  profile-based variance isolation; no profile may show work proportional to
  existing index cardinality.
- NVT-hinted and NVT-absent queries are result-identical; performance is measured
  only after correctness is established.
- An outlier requires a confirmation run and variance isolation; it cannot be
  waved through or block the campaign from one noisy sample.

### Real migration gates

- Migrate a synthetic boundary database and a verified copy of the relevant
  FS-Server1 database; never the production original.
- Exercise concurrent small writes, merges, blob commits, config/plugin changes,
  snapshots/forks, client sync, GC, forced kills, resume, cancellation, and
  rollback.
- Reopen with the release binary, run full verify/repair diagnostics, compare
  authoritative trees and sampled/full content hashes, and run query/list/fetch
  agreement against the source.
- Repeat format/golden/durability gates on macOS and Windows before v4 activation.

All Rust build/test commands in execution plans default to `-j 6` or less.

## 9. Landing, Rollback, and Production Safety

- One phase/wave is one revert unit. Merge, run the full gate, then push; never
  chain a push after a failing gate.
- No child plan edits the live FS-Server1 database. Investigation and migration
  rehearsal use verified copies.
- No v4 writer is enabled before P1 reader/capability gates, and no index pointer
  is activated before P4 physical retirement and P6 coverage accounting.
- The v3 source remains available through v4 validation/canary. The v0 index
  remains available through v1 validation/canary.
- Rollback before v4 finalization restarts the untouched v3 source. Rollback
  before v1 finalization changes only the local registry selection back to v0;
  immutable v1 artifacts can be quarantined later.
- A capability mismatch, incomplete protected transfer, unavailable exact
  semantic dependency, missing Windows durability primitive, or failed
  production-copy proof blocks activation. It is not converted into a warning.

## 9A. Adjacent Work: Absorb or Roadmap

- The broader durability/error-squelch audit remains a parallel campaign.
  Every correctness-bearing ignore encountered in files touched here is
  absorbed and fixed under this campaign; unrelated telemetry/temp-cleanup
  findings remain in the durability audit and are not silently dropped.
- KV page ordering/layout redesign remains explicitly roadmapped. This campaign
  may add typed continuation metadata only where the already-approved field
  index/GC formats require it; it does not mutate KV v0 bytes.
- GPU NVT compositing, positional indexes, distributed compatible-index
  transfer beyond the frozen optional protocol, and per-scope lock sharding
  remain future plans with preserved handoff constraints.
- Existing B-tree corruption repair continues as an adjacent prerequisite and
  regression source. This campaign consumes strict/incomplete traversal status
  but does not claim that index/GC work alone repairs every damaged directory.

## 10. Completeness and Ratification Gates

### Blocker traceability

| Finding | Contract/round | Implementation | Primary proof |
| --- | --- | --- | --- |
| Root lacks exact semantics | `NamespaceRootV1`/`SemanticStateRootV1`, R10 | P3, P7 | Cross-root config/plugin race and historical agreement oracle |
| KV bitmap misses physical incarnations | Replacement/retirement plus full GcArtifact, R13 | P4 | Exhaustive replacement crash matrix and WAL/KV external-sort model |
| Persistent formats incomplete | R11-R14 pre-writer freezes | P1, P4, P5 | Independent cross-platform goldens, fuzzing, writer agreement |
| Root admission/expiry unbounded or ambiguous | Structural root plus candidate/expiry state, R10/R13 | P3, P4 | Publication crash, pre-first-mark, request-pin race, compaction, expiry-state model |
| Protected roots diverge by subsystem | Binary-owned `SystemFamilyRegistry`, R10 | P2, P3 | Pairwise consumer agreement and hard-coded-list grep gate |
| Write acknowledgement/index visibility split | Mutation/coverage coordinators, R10/R14 | P2, P6 | All-producer agreement, crash/restart coverage, blob latency gate |
| Listing order overwritten | Route total-order/token contract, R12 | P7 | Asc/desc directories-first pagination differential tests |
| Legacy roots lack historical semantics | Legacy-fidelity policy, R10 | P3, P7 | Exact-dependency fixture versus typed unavailable result |
| V4 changes external root hashes | Root map/reset contract, R10/R14 | P3, P4 | Known-map migration and unknown-client-root sync-reset E2E |
| Stale parent plans could reintroduce defects | Final synthesis and supersession | P9 | Mechanical stale-term/decision-consistency plan gates |

The campaign is ready to implement only when all statements below are true:

- [ ] Rounds 10 through 14 are CLOSED with exact bytes and policies.
- [ ] Every final audit blocker maps to a contract, format, phase, and test.
- [ ] Legacy historical semantics and externally held v3 root hashes have an
      explicit documented migration outcome.
- [ ] The regenerated parent/child plans contain no root-cursor TTL, two-field
      GC candidate, pointer-owned registry, or generic-list-order leftovers.
- [ ] Every protected family appears exactly once in the system registry and
      every maintenance consumer derives behavior from it.
- [ ] Every stable-key writer appears in the replacement/retirement agreement
      oracle or an explicitly reviewed bootstrap-only allowlist.
- [ ] Every persistent schema has independent golden fixtures and decoder
      bounds before its first writer.
- [ ] A new worker can execute P0 and P1 from the plan without asking what a
      byte, error, durability barrier, migration state, or expected result means.
- [ ] The full test, stress, crash, cross-platform, `/tmp`, and production-copy
      evidence plan is executable with explicit commands and deadlines.
- [ ] The owner has ratified the final synthesis; implementation has not been
      inferred from planning approval.

## Planning Verdict

Every issue from the final audit now has a corrective contract, a decision
round, an implementation phase, a rollback boundary, and a proof obligation.
The plan deliberately does not pretend that the missing binary layouts are
already frozen. Rounds 10 through 14 are the remaining design work; once they
close, final synthesis converts this campaign into executable child plans.

The only newly surfaced product-visible ruling is legacy semantic fidelity.
The recommendation above is to fail semantic historical queries when exact v3
dependencies cannot be proven, while preserving direct historical content
reads. Silently evaluating an old root with newer semantics is not an
acceptable alternative.

---

## Round 10: Semantic Read Views, Root Lifecycle, and Protected State

**Status:** Self-answered and closed at the read-view/protected-state boundary.
Rounds 11 through 14 still own the deferred converter/index definitions,
index-page formats, physical GC artifact bodies, low-volume task/control
records, first-authority transaction controls, the expanded built-in registry
byte fixture, capability-bit assignments, and final configuration defaults. No
writer named in this round may ship before those dependent pre-writer freezes
are also closed.

This round was defined after a fresh code-level territory map, not only from
the parent plan. The current implementation has five material discrepancies
that the new contract must replace:

1. `StorageEngine::head_hash`, `VersionManager`, `tree_walker`,
   `version_access`, query, sync, backup, GC, import, restore, fork promotion,
   and direct CLI promotion each adopt or publish a root independently.
2. A current root is an ordinary `DirectoryIndex`/B-tree hash. Its bytes do not
   distinguish the complete `/` root from an internal B-tree node or nested
   directory.
3. Absolute `/.aeordb-system` and `/.aeordb-config` trees deliberately stop
   propagating into HEAD. `tree_walker.rs` and `backup.rs` compensate with
   different hard-coded lists, omit current families, and contain
   warning/continue or silent-skip paths.
4. Current peer sync admits that historical protected state cannot be
   reconstructed, then adds selected **current** protected files to a diff from
   an old namespace root. That is not one immutable read view.
5. `EntryHeader.entry_version` already carries per-entity payload versions in
   v3 (including `FileRecord` v1). The v4 reader must first select physical
   framing from `DatabaseHeader`, then dispatch the complete entity through
   `(EntryType, EntityVersion)`. It must not assume that every migrated v4
   entity has entity version 1 merely because the new common header layout was
   introduced in the v4 container.

The AGIS territory map found these direct root producers:

- directory COW propagation, batch/blob commit, import/patch application, and
  embedded writes;
- snapshot restore, fork promotion/update, direct CLI/server promote, and
  backup import;
- side-by-side migration/cutover and future repair/maintenance roots; and
- semantically relevant index-config, parser-registry, plugin-alias, and native
  semantic changes that may alter the semantic state while leaving user file
  bytes unchanged.

The direct and indirect consumers are:

- query, search, listing, direct GET/HEAD, multi-fetch, range extraction,
  locator/snippet continuation, sorting, aggregation, pagination, and EXPLAIN;
- snapshots, forks, GC, verify, scrub, repair, backup, export/import, patches,
  peer replication, client sync, migration, and cluster join;
- HTTP, embedded SDK, bundled web UI, sync clients, bots, CLI diagnostics,
  health, metrics, and dashboard; and
- every Linux, macOS, and Windows reader/writer.

Handling fewer paths would recreate the current split-read-view defect. The
contracts below therefore have one owner each and agreement gates for every
listed producer/consumer.

### 1. `DirectoryIndexV1` and exact `NamespaceRootV1`

**DECIDED (self):** A v4 public root is a distinct immutable
`NamespaceRootV1`, not a hidden file, ordinary B-tree node, mutable control, or
stored API cursor. It uses `EntryType::DirectoryIndex` because it is structural
namespace metadata, but `DirectoryIndexV1` has an explicit kind envelope so
internal nodes and complete roots cannot be confused.

The complete stored value starts with this shared `DirectoryIndexV1` envelope.
All multibyte integers are little-endian:

| Value offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `directory_magic` | ASCII `ADIR` |
| 4 | 2 | `directory_schema_version` | Exactly `1` |
| 6 | 2 | `directory_kind` | Permanent kind below |
| 8 | 2 | `directory_header_length` | Exactly `32` |
| 10 | 2 | `directory_flags` | Zero in v1 |
| 12 | 4 | `directory_value_length` | Complete value including CRC |
| 16 | 4 | `body_length` | Exact kind body |
| 20 | 4 | `item_count` | Kind-specific; zero for namespace root |
| 24 | 8 | reserved | Zero |
| 32 | body length | `body` | Exact kind body |
| final 4 | 4 | `directory_crc32` | CRC-32/ISO-HDLC over all prior value bytes |

Permanent kinds are:

~~~text
0x0000  invalid / never emitted
0x0001  leaf node      body deferred; no writer before its format freeze
0x0002  internal node  body deferred; no writer before its format freeze
0x0003  namespace root exact body frozen below
~~~

The current v0 flat/B-tree directory payload remains codec 1. A v4 database
may contain migrated `(DirectoryIndex, EntityVersion 0)` tree entities inside
v4 physical framing; the v4 common header does not silently change their body
semantics. `NamespaceRootV1` is `(DirectoryIndex, EntityVersion 1,
directory_kind 0x0003)`. A future v1 leaf/internal writer remains disabled
until those two bodies are frozen. This resolves the physical-framing versus
entity-version ambiguity without rewriting v0 tree bytes as an unspecified new
codec.

Let `H` be the database hash width. The namespace-root body is:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `root_flags` | Zero |
| 4 | 32 | `required_reader_capabilities` | Checked 256-bit bitset |
| 36 | 2 | `namespace_tree_codec` | `1 = directory_tree_v0` |
| 38 | 2 | `semantic_state_codec` | `1 = SemanticStateRootV1` |
| 40 | `H` | `namespace_tree_root` | Nonzero complete `/` tree root |
| `40 + H` | `H` | `semantic_state_root` | Nonzero immutable semantic-state object ID |
| `40 + 2H` | 32 | reserved | Zero |

The body length is exactly `72 + 2H`. The complete stored value is exactly
`108 + 2H`: 172 bytes for Blake3-256 and 236 bytes for a 64-byte database
hash. `item_count` is zero.

Its entity/KV key, and therefore its public `root_hash`, is:

~~~text
H(
  ASCII "aeordb.directory-index.immutable.v1\0" ||
  directory_kind u16 LE ||
  complete ADIR stored value including trailing CRC
)
~~~

The surrounding v4 EntityHeader requires that exact `H`-byte key, no outer
compression/encryption in v1, and key/value/integrity agreement. Timestamp,
write sequence, publication origin, snapshot name, parent root, task ID, and
display metadata are deliberately absent from the value and identity. The same
namespace tree plus exact semantic state always produces the same root hash.

The namespace tree root and semantic-state object are typed GC edges. A root is
not valid merely because its envelope and CRC decode: both immediate targets
must exist, have the expected kind/codec, pass key/identity checks, and satisfy
shallow structural validation. Full verify, migration cutover, repair, and
semantic backup traverse the complete closure.

Changing user namespace content publishes another namespace-tree root and
another `NamespaceRootV1` that normally reuses the same semantic-state ID.
Changing effective semantic behavior publishes another semantic-state ID and
another namespace root even when the namespace-tree root is unchanged. Derived
IndexArtifacts are not referenced and cannot affect `root_hash`.

### 2. Authoritative immutable semantic objects

**DECIDED (self):** Semantic state is authoritative input, not rebuildable
`IndexArtifact`. Store it as immutable content-addressed system objects under
one protected family. Large catalogs are paged and traversed lazily; no reader,
compiler, backup, GC run, or startup path may load the whole semantic world.

Every semantic object is a regular protected system file at:

~~~text
/.aeordb-system/semantic-objects/<hash-algorithm-id-4hex>/<kind-id-4hex>/<object-id-lowercase-hex>
~~~

The path is a deterministic locator, not identity. The exact file bytes begin
with:

| Object offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `object_magic` | ASCII `ASEM` |
| 4 | 2 | `object_schema_version` | Exactly `1` for the kind |
| 6 | 2 | `object_kind` | Permanent kind below |
| 8 | 2 | `object_header_length` | Exactly `32` |
| 10 | 2 | `object_flags` | Zero unless the kind assigns bits |
| 12 | 4 | `object_total_length` | Exactly `36 + body_length` |
| 16 | 4 | `body_length` | Checked kind body |
| 20 | 8 | `item_count` | Kind-specific checked count |
| 28 | 4 | reserved | Zero |
| 32 | body length | `body` | Exact kind body |
| final 4 | 4 | `object_crc32` | CRC-32/ISO-HDLC over all prior object bytes |

Permanent object kinds are:

~~~text
0x0000  invalid / never emitted
0x0001  SemanticStateRoot
0x0002  SemanticCatalogLeaf
0x0003  SemanticCatalogInternal
0x0004  SemanticDefinitionRecord
~~~

An immutable semantic object ID is:

~~~text
H(
  ASCII "aeordb.semantic-object.immutable.v1\0" ||
  object_kind u16 LE ||
  complete ASEM object bytes including trailing CRC
)
~~~

Creation is write-once. If the deterministic path already exists, the engine
must decode, recompute ID, compare length, and compare every byte. Any
disagreement is corruption or a collision alarm; it is never overwrite. The
object is fully durable before any object or namespace root may reference it.
Orphans are harmless and later reclaimable.

The object ID is not confused with the regular FileRecord/path-hash locator
used to store the file. Given kind and object ID, readers and GC derive the
canonical path above, resolve its FileRecord, stream the exact bytes, and then
verify CRC and object ID. No caller scans the system directory to find an
object. V1 caps are checked before allocation:

~~~text
SemanticStateRoot complete object:      4 KiB
SemanticCatalogInternal complete object: 64 KiB
SemanticCatalogLeaf complete object:     1 MiB
SemanticDefinitionRecord complete object: 1 MiB
catalog traversal depth:                 at most H digest bytes
~~~

Objects larger than their kind cap are invalid even though the common length
field is `u32`. Large executable modules remain in the separately frozen plugin
archive and are not smuggled through this object family.

#### `SemanticDefinitionRecordV1`

This object wraps one already-canonical semantic definition or projection. Let
`D` be its exact definition bytes:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 2 | `definition_class` | Permanent class below |
| 2 | 2 | `definition_schema_version` | Exact class codec |
| 4 | 4 | `definition_flags` | Zero in v1 |
| 8 | `H` | `semantic_id` | Recomputed class-specific semantic ID |
| `8 + H` | 4 | `definition_length` | `D`; class and 1 MiB object bounds apply |
| `12 + H` | 4 | reserved | Zero |
| `16 + H` | `D` | `definition` | Exact canonical bytes |

The body length is `16 + H + D`; `item_count = 1`. Permanent classes are:

~~~text
0x0001  EffectiveIndexConfigProjectionV1
0x0002  ParserRegistryProjectionV1
0x0003  ScopeDefinitionV1
0x0004  ValueStoreDefinitionV1
0x0005  FieldIndexDefinitionV1
0x0006  ExecutableDependencyDefinitionV1
0x0007  NativeDependencyDefinitionV1
~~~

Classes 3 and 4 use the already-frozen IDs and bytes. Class 5 remains writer-
disabled until Round 11 freezes its complete definition. Classes 6 and 7 use
the exact Round 9 dependency/manifest identities. Config projections are
post-validation **semantic projections**, not raw JSON or FileRecord hashes:
semantically irrelevant whitespace, JSON member order, default spelling, and
unordered source arrays canonicalize away. A behaviorally meaningful change
must change the projection ID.

The remaining class ID domains are now explicit:

~~~text
EffectiveIndexConfigProjectionId =
  H("aeordb.semantic.effective-index-config-projection.v1\0" || canonical projection bytes)

ParserRegistryProjectionId =
  H("aeordb.semantic.parser-registry-projection.v1\0" || canonical projection bytes)

ExecutableDependencyDefinitionId =
  H("aeordb.semantic.executable-dependency-definition.v1\0" || exact Round 9 dependency bytes)

NativeDependencyDefinitionId =
  H("aeordb.semantic.native-dependency-definition.v1\0" || exact native dependency bytes)
~~~

Projection bytes use the frozen `CanonicalConfigValueV1` structural codec only
after the owning config compiler has removed aliases/default spellings and
sorted every semantically unordered set. Generic JSON canonicalization alone
is not sufficient. Round 11/14 may add fields to the source config schemas, but
cannot change these domains or allow source formatting to affect identity.

#### Canonical Merkle-radix semantic catalog

The catalog contains only small bindings. Definitions remain separate objects,
so one large ValueStore definition cannot force an oversized catalog page.
A binding is logically:

~~~text
record_kind u16
owner_key   canonical bytes
semantic_id H
definition_object_id H
~~~

Permanent record kinds match the seven definition classes above. Owner keys
are:

- control projection: `control_kind u16 LE || canonical absolute owner path`;
- scope/value/field definition: exactly its `ScopeId`, `ValueStoreId`, or
  `IndexId`;
- executable/native dependency: exactly its canonical dependency fingerprint.

The control path is 1..65,535 UTF-8 bytes after the two-byte kind. Scope,
ValueStore, Index, and dependency owner keys have the exact fixed width assigned
by their defining registry (never a caller-supplied variable blob). No v1
owner key may exceed 65,537 bytes, and all bounds are checked before allocation.

The catalog lookup digest is:

~~~text
H(
  ASCII "aeordb.semantic-catalog-key.v1\0" ||
  record_kind u16 LE || owner_key
)
~~~

Using a canonical compressed Merkle radix tree instead of a sorted page array
gives mutation-order-independent identity and bounded COW updates. It also
avoids rewriting every later page when one early path is inserted.

A `SemanticCatalogLeaf` contains records with one identical full lookup digest:

| Leaf body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `leaf_flags` | Zero |
| 4 | 4 | `record_count` | 1..4,096; equals envelope `item_count` |
| 8 | `H` | `lookup_digest` | Recomputed for every record |
| `8 + H` | 4 | `records_length` | Exact following bytes |
| `12 + H` | 4 | reserved | Zero |
| `16 + H` | variable | `records` | Strict canonical order, exact consumption |

Each record is:

| Record offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 2 | `record_kind` | Known nonzero kind |
| 2 | 2 | `record_flags` | Zero |
| 4 | 4 | `owner_key_length` | `K`; checked class bound |
| 8 | `H` | `semantic_id` | Nonzero, class/definition agreement |
| `8 + H` | `H` | `definition_object_id` | Exact kind-4 object ID |
| `8 + 2H` | `K` | `owner_key` | Exact canonical key |

Collision records are sorted by `(record_kind numeric, owner_key raw bytes)`
and reject duplicates. A leaf's complete object is capped at 1 MiB. More than
4,096 distinct keys with one full cryptographic digest, or a leaf exceeding the
cap, is a typed hash-flood/collision failure; it cannot be truncated.

A `SemanticCatalogInternal` is the unique maximally compressed radix node:

| Internal body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `node_flags` | Zero |
| 4 | 2 | `depth` | Digest bytes consumed before prefix |
| 6 | 2 | `prefix_length` | `P`; maximal common prefix |
| 8 | 2 | `child_count` | 2..256; equals envelope `item_count` |
| 10 | 2 | reserved | Zero |
| 12 | 8 | `subtree_record_count` | Exact checked sum |
| 20 | `P` | `prefix` | Digest bytes at `depth..depth + P` |
| `20 + P` | variable | `children` | Strictly increasing edge byte |

Each child is exactly:

~~~text
edge_byte            u8
child_flags          u8 = 0
reserved             u16 = 0
child_record_count   u64 LE, nonzero
child_object_id      H bytes, nonzero
~~~

The body length is `20 + P + child_count * (12 + H)`. The node requires
`depth + P < H`; `edge_byte` is the next lookup-digest byte. Internal nodes
with one child, non-maximal prefixes, unsorted/duplicate edges, inconsistent
child depths, or wrong subtree counts are noncanonical corruption. Traversal
uses an explicit bounded stack of at most `H` digest bytes, never recursive
unbounded allocation.

The empty catalog has no root object and a zero root slot. For a nonempty set,
this Patricia/radix construction has exactly one canonical shape independent
of insertion order, compiler batch size, platform map implementation, or
restart boundaries. One binding update rewrites its definition object, leaf,
and one bounded root path; it does not load or rewrite unrelated definitions.

#### Exact `SemanticStateRootV1`

The kind-1 body is:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `state_flags` | Bit 0 = content-only legacy; all others zero |
| 4 | 32 | `required_reader_capabilities` | Checked 256-bit bitset |
| 36 | 2 | `semantic_catalog_codec` | Exactly `1` |
| 38 | 2 | `semantic_definition_codec` | Exactly `1` |
| 40 | 2 | `compiler_profile_version` | Exactly `1` |
| 42 | 2 | `availability_reason` | Stable reason below |
| 44 | 1 | `catalog_root_present` | Canonical boolean |
| 45 | 3 | reserved | Zero |
| 48 | `H` | `compiler_fingerprint` | Exact semantic compiler profile |
| `48 + H` | `H` | `semantic_registry_fingerprint` | Semantic projection of system registry |
| `48 + 2H` | `H` | `catalog_root` | Catalog object ID or all zero |
| `48 + 3H` | 8 | `catalog_record_count` | Exact records |
| `56 + 3H` | 8 | `catalog_node_count` | Exact leaf + internal objects |
| `64 + 3H` | 8 | `definition_count` | Exact definition objects |
| `72 + 3H` | 8 | `dependency_count` | Executable + native definitions |
| `80 + 3H` | 32 | reserved | Zero |

The body length is `112 + 3H`; the complete object is `148 + 3H`: 244
bytes for Blake3-256 and 340 bytes for a 64-byte database hash. Envelope
`item_count` equals `catalog_record_count`.

Availability reasons are:

~~~text
0  complete semantic state
1  legacy global semantic state was never captured
2  exact legacy executable/native dependency cannot be proven
3  legacy semantic control is corrupt or incomplete
~~~

A complete state has flag bit 0 clear, reason zero, nonzero compiler and
semantic-registry fingerprints, and canonical root/count presence. A
content-only legacy state has bit 0 set, a nonzero reason, zero catalog and all
counts, and zero compiler/registry fingerprints. It authorizes structural
listing/fetch of the associated namespace root but never semantic
query/search/aggregation/fallback. It is an honest typed limitation, not a
request to consult current controls.

`semantic_registry_fingerprint` is computed from only the family-registry
fields that change semantic interpretation. Backup, redaction, cadence, cache,
or node-local transfer-policy changes do not churn namespace roots. The full
operational registry fingerprint is recorded by the operation manifests that
use it and capability-gated independently.

### 3. What belongs to the semantic world

**DECIDED (self):** Include exactly the behavior needed to reproduce query
meaning. Exclude node policy and authority state. The rule is transitive: a
mutable path or alias never becomes authority merely because its current bytes
are easy to read.

| Input | In semantic state? | Canonical authority |
| --- | ---: | --- |
| Global `/.aeordb-config/indexes.json` | Yes | Effective semantic projection plus compiled definitions |
| Every inherited/nested `.aeordb-config/indexes.json` | Yes | Effective projection and scope/value/index bindings |
| `/.aeordb-config/parsers.json` | Yes | Canonical registry projection and compiled parser plans |
| Scope membership/resolver/path/FileKey semantics | Yes | `ScopeDefinitionV1` |
| Source selector/parser/mapper/null/error/limit semantics | Yes | `ValueStoreDefinitionV1` |
| Strategy/converter/token/order/limit semantics | Yes | `FieldIndexDefinitionV1` |
| Referenced plugin alias | Yes | Exact canonical alias/dependency identity, never mutable path alone |
| Referenced WASM module | Yes | Exact archived raw-module digest/manifest dependency |
| Referenced native implementation | Yes | Exact native semantic fingerprint/profile |
| Compiler/resolver semantics | Yes | Compiler fingerprint and definition IDs |
| Semantic protected-family classification | Yes | Semantic registry projection fingerprint |
| Unused installed plugin/alias | No | No current read behavior depends on it |
| Users, groups, permissions, API/JWT/refresh/magic credentials | No | Current authorization state, reapplied per request |
| Lifecycle/GC/snapshot retention/cadence | No | Operational policy |
| Cron, webhook, CORS, email delivery | No | Operational policy |
| Cluster node/peers/sync progress/join audit | No | Node/cluster operational state |
| Task progress, migration/repair/GC workspaces | No | Operational resumability |
| IndexArtifact/GcArtifact generations | No | Derived/maintenance state |
| Page packing, cache, memory, NVT resolution, flush cadence | No | Physical/performance policy |
| Timestamps, author/display labels, logging | No | Nonsemantic metadata |

Raw config FileRecord identity is used as an optimistic activation precondition
but is not hashed into semantic identity. Otherwise whitespace-only rewrites
would manufacture new semantic worlds. The compiler persists source identities
in its task/checkpoint for race detection while the state root contains only
canonical output semantics.

### 4. Semantic mutation and atomic activation

**DECIDED (self):** Every mutation that must **compile or change semantic
inputs** is staged and task-backed. There is no route-specific synchronous
compiler and no long-held namespace write lock. This applies to direct
HTTP/SDK config routes and to generic PUT, blob/batch, merge, copy/move,
sync/import, plugin-host, delete, and plugin-alias operations that touch
semantic inputs.

The public HTTP result for an accepted semantic mutation is always `202`:

~~~json
{
  "task": {
    "id": "<uuid>",
    "type": "semantic_mutation",
    "status": "queued"
  },
  "base_root_hash": "<captured NamespaceRootV1>",
  "visibility": "after_task_completion"
}
~~~

The embedded API returns the same typed task handle. A future explicit wait
helper may await the task, but it cannot change acknowledgement or visibility
semantics. A batch containing ordinary files and a semantic control is one
staged atomic mutation: none of its namespace changes become visible early.

Permanent task states are logically:

~~~text
queued -> capturing -> compiling -> ready_to_activate -> activating -> completed
          |             |              |                   |
          +-------------+--------------+-------------------+-> failed/cancelled
                                                            -> superseded/retry
~~~

Round 14 freezes the exact task/control bytes. Behavior is fixed here:

1. Validate syntax, authorization, schema, limits, and module bytes before
   accepting the task. Archive immutable plugin module bytes first when needed.
2. Capture one base namespace root, semantic state, exact source-control
   identities, and semantic control generation. Persist that capture.
3. Build definition objects and the canonical catalog under bounded memory,
   checkpoints, progress, ETA, cancellation, and restart. No namespace
   publication guard is held.
4. Validate the complete candidate closure and construct its candidate
   `NamespaceRootV1`.
5. Acquire the short publication guard. Recheck every semantically relevant
   source identity/control generation. Unrelated ordinary HEAD changes are
   rebased by applying the staged control path change to the latest tree;
   conflicting semantic changes supersede/retry rather than merge guesses.
6. Durably append immutable dependencies first. In one authoritative
   transaction, publish staged mutable controls/config, index-coverage
   transitions, the new namespace-root entity, its first HEAD/snapshot/fork or
   durable maintenance authority, and task activation authority. The
   namespace-root entity is not KV-visible before the transaction commit
   marker; HEAD remains the final externally selected authority.
7. Report completion only after durable read-back resolves the new root and
   exact semantic closure. Parsing/index construction remains asynchronous
   under the separate coverage contract.

Crash before authority publication leaves only immutable orphans and the old
read view. Crash after HEAD publication recovers the task as completed from the
selected root/control identities; it does not roll HEAD back because a task
status write was torn. Cancellation before activation leaves the old world
active. Cancellation loses the race once the atomic activation commit begins.

Restoring or promoting a root still uses the same mutation coordinator, but a
v4 snapshot/fork already names a fully materialized exact `NamespaceRootV1`.
After validation, that operation may take the coordinator's short synchronous
authority-selection path; inventing a background compiler task would add no
safety. Importing or transforming a root, or restoring a legacy content-only
root with newly selected semantics, remains staged/task-backed. A legacy
content-only root cannot silently become writable/current; restore requires an
explicit currently supported semantic state and produces a new v4 namespace
root.

### 5. Root admission, lifecycle, and bounded expiry evidence

**DECIDED (self):** Do **not** append a separate provenance record for every
HEAD. `NamespaceRootV1` plus committed first-authority publication is the
durable proof that a hash is an actual complete namespace root. A per-write
provenance catalog would duplicate every root publication, grow forever when
GC is disabled, and create another stable-key retirement path.

This narrowly supersedes the Category 3 and remediation-campaign requirement
for a separate published-origin ledger while preserving the owner-approved
“actual roots only” rule. The engine exposes no standalone NamespaceRoot store
primitive. A new NamespaceRoot entity and its first HEAD, snapshot, fork, or
durable maintenance authority are one committed transaction; the root is not
inserted into the authoritative KV view before that commit marker. Dirty-start
rebuild, verify, repair, and migration must ignore/quarantine an uncommitted raw
root tail and may never manufacture its first authority. Once a committed root
later loses all authority, the still-present entity is a valid `retained` root.
This makes former roots self-proving without admitting prepared/aborted data or
depending on secrecy of a candidate hash.

Every ordinary namespace commit, semantic activation, restore, fork/snapshot
creation that introduces a root, migration cutover, and repair promotion uses
this same root-publication transaction. An implementation whose WAL/KV
publication cannot provide that atomic visibility is not permitted to enable
the v4 writer; Round 14 must freeze the transaction/control bytes and recovery
decision table first.

Successful readable root states are exactly:

- `live`: reached by HEAD, snapshot, fork, or a durable active
  migration/repair/build pin; `expires_at = null`;
- `retained`: a formerly authority-published, valid complete `NamespaceRootV1`
  remains readable but has no current authoritative root and has not yet
  entered a completed GC candidate set;
  `expires_at = null` because no eligibility boundary exists yet; and
- `pending_delete`: the root is in a durably published GC candidate state;
  `expires_at = pending_since_ms + grace_at_pending_ms` using checked
  arithmetic; overflow is corrupt/invalid lifecycle state and disables reclaim.

Reading does not refresh or clear any state. A request pin is a transient sweep
exclusion and does **not** upgrade the root's reported state: a selected
retained/pending root still reports retained/pending. Admission obtains that
pin, then rechecks candidate/reclaim state so a sweep that won first returns
`ROOT_EXPIRED`, while a request that won first keeps the closure until request
completion. `retained` can become `pending_delete` after a complete mark, or
`live` if a new authoritative reference is created. `pending_delete` becomes
live and clears candidate state if reached by a complete later root set before
reclaim.

Creating a snapshot/fork/task pin or selecting HEAD for a pending root uses the
same root-state guard and post-acquisition recheck as request admission. If the
authority transaction wins, it clears candidate state before publication. If
reclaim authority wins, the authority creation fails `ROOT_EXPIRED`; it cannot
resurrect a partially released closure.

The GC state machine also has internal non-readable `reclaiming` and
`reclaimed` classifications. They are never returned as successful
`root.state` values: both return `410 ROOT_EXPIRED`. Reclaim authority and
enough expiry evidence are published durably **before** any root/closure byte is
released to Void, so a crash cannot reopen logically dead bytes as a retained
view. Final physical completion adds the reclaim sequence/timestamp/receipt;
an interrupted reclaim resumes conservatively from the earlier durable state.

A bounded `RootExpiryCatalog` carries that evidence through reclaim and retains
it after the root object is removed, solely for deterministic
`410 ROOT_EXPIRED`. Each logical evidence record contains:

~~~text
root_hash                   H
root_format                 NamespaceRootV1
pending_since_ms            i64
grace_at_pending_ms         u64
first_unreachable_generation u64
final_mark_generation       u64
reclaimed_at_ms             i64
reclaim_write_sequence      u64
sweep_receipt               H
evidence_expires_at_ms      i64
reason                      permanent enum
~~~

The physical paged/compacted GcArtifact body, key, and publication state are
frozen in Round 13. Its age and byte budgets plus config sources/defaults are
frozen in Round 14. The mandatory behavior is:

- adding evidence happens once per reclaimed root, not once per write/read;
- evidence does not pin the namespace tree, semantic state, or definitions;
- compaction keeps newest evidence for a root and evicts oldest records when
  either the configured age or byte budget is exceeded;
- missing/corrupt/unknown expiry state disables destructive GC for the affected
  evidence segment but cannot make another root eligible;
- while evidence remains, the old hash returns `ROOT_EXPIRED`;
- after bounded evidence expires, an absent ancient hash returns
  `INVALID_NAMESPACE_ROOT`; AeorDB does not claim infinite historical memory;
  and
- if GC is disabled, former root objects simply remain `retained`; no
  provenance or expiry stream grows per namespace mutation.

Resolution order is deterministic:

1. authenticate the caller and strictly decode the configured hash width;
2. consult the finite v3-to-v4 migration map described below;
3. resolve and fully validate the `NamespaceRootV1` envelope and immediate
   closure;
4. classify live/retained/pending/reclaiming under one root-state snapshot and
   establish the request pin;
5. if no root object is present, consult bounded expiry evidence;
6. return `INVALID_NAMESPACE_ROOT` for an existing non-root DirectoryIndex or
   an absent hash with no retained evidence; and
7. return `DATABASE_CORRUPTION` when an authoritative reference names a missing
   or corrupt root/closure.

Authorization and concealment are applied before root existence, state,
counts, timing, names, snippets, positions, or EXPLAIN become observable. Root
hash knowledge remains neither authentication nor authorization.

### 6. One binary-owned `SystemFamilyRegistry`

**DECIDED (self):** Replace every hard-coded path/type list with one immutable,
binary-owned, versioned `SystemFamilyRegistry`. It is not a database file and
cannot be weakened by a plugin, damaged control, or remote peer. Permanent
family IDs are never reused. New families require a registry version and any
reader capability needed by their safety boundary.

The registry's canonical bytes begin with:

| Registry offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `registry_magic` | ASCII `ASFR` |
| 4 | 2 | `registry_schema_version` | Exactly `1` |
| 6 | 2 | `registry_header_length` | Exactly `32` |
| 8 | 4 | `registry_total_length` | Header + descriptors + CRC |
| 12 | 4 | `descriptor_count` | Exact, nonzero |
| 16 | 4 | `descriptors_length` | Exact bytes |
| 20 | 4 | `registry_flags` | Zero |
| 24 | 8 | reserved | Zero |
| 32 | variable | `descriptors` | Canonical descriptor-key order below |
| final 4 | 4 | `registry_crc32` | CRC-32/ISO-HDLC over prior bytes |

Each descriptor is:

| Descriptor offset | Size | Field |
| ---: | ---: | --- |
| 0 | 2 | permanent `family_id` |
| 2 | 1 | `storage_domain` |
| 3 | 1 | `match_kind` |
| 4 | 1 | `semantic_role` |
| 5 | 1 | `gc_policy` |
| 6 | 1 | `physical_copy_policy` |
| 7 | 1 | `logical_backup_policy` |
| 8 | 1 | `data_export_policy` |
| 9 | 1 | `peer_replication_policy` |
| 10 | 1 | `cluster_join_policy` |
| 11 | 1 | `client_sync_policy` |
| 12 | 1 | `import_policy` |
| 13 | 1 | `verify_policy` |
| 14 | 1 | `repair_policy` |
| 15 | 1 | `migration_policy` |
| 16 | 1 | `spill_policy` |
| 17 | 1 | `sensitivity` |
| 18 | 1 | `event_policy` |
| 19 | 1 | `absence_policy` |
| 20 | 1 | `unknown_child_policy` |
| 21 | 1 | reserved, zero |
| 22 | 2 | reserved, zero |
| 24 | 4 | `descriptor_flags`, zero in v1 |
| 28 | 2 | `matcher_length` |
| 30 | 2 | reserved, zero |
| 32 | variable | exactly `matcher_length` structured matcher bytes |

One behavioral family may require multiple matchers (for example the lifecycle
file and its last-known-good control). Descriptors are therefore ordered
lexicographically by `(family_id u16, storage_domain u8, match_kind u8,
matcher bytes)`; duplicate descriptor keys are invalid. Descriptors sharing a
family ID must have byte-identical semantic, GC, transfer, verification,
repair, migration, spill, sensitivity, event, absence, unknown-child, and
flags fields. A candidate may match multiple descriptors of one
family but must never resolve to two different family IDs after specificity
rules. This keeps family policy singular without forcing unrelated storage
domains into an ad hoc compound matcher.

All policy and matcher values are permanent checked enums. Matcher kinds cover
canonical absolute exact path, absolute prefix, reserved path-segment/suffix,
EntryType, KV key domain, in-file control domain, and external workspace kind;
they are structured bytes, not glob strings reparsed differently by consumers.
The full operational fingerprint is:

~~~text
H("aeordb.system-family-registry.v1\0" || complete registry bytes including CRC)
~~~

The semantic projection fingerprint includes only `(family_id,
storage_domain, match_kind, matcher bytes, semantic_role)` for descriptors
with a nonzero semantic role.
Transfer/redaction/GC policy changes therefore do not alter query meaning.
Payload version dispatch remains owned by each family's already-versioned
codec, not duplicated in this classification registry. Human-readable handler
or module names are binary diagnostics, not canonical bytes: refactoring a
Rust module cannot change a database fingerprint or namespace root.

The v1 numeric registries are frozen here. Zero is invalid unless explicitly
named `None`:

~~~text
StorageDomain: 1 Path, 2 EntryType, 3 KvKeyPrefix, 4 ControlRegion,
               5 ExternalWorkspace
MatchKind:     1 AbsolutePathExact, 2 AbsolutePathPrefix,
               3 DescendantReservedFile, 4 DescendantReservedSubtree,
               5 ReservedPathSegment, 6 EntryTypeExact, 7 KvKeyPrefix,
               8 ControlTagExact, 9 WorkspaceKindExact
SemanticRole:  0 None, 1 CanonicalProjection, 2 ExecutableDependency,
               3 AuthoritativeSemanticObject, 4 DerivedDisposable
GcPolicy bits: 0x01 TraceEdges, 0x02 PinWhileAuthoritative,
               0x04 Quarantine, 0x08 DerivedRebuildable,
               0x10 EvidenceRetention, 0x20 ConservativeRetain
TransferPolicy: 1 RequiredInclude, 2 OptionalValidated, 3 OmitDeclared,
                4 NodeLocal, 5 RedactOmit, 6 NamedSubsetOnly,
                7 FailUnknown
VerifyPolicy:   1 StrictIfPresent, 2 StrictRequired, 3 Rebuildable,
                4 ConservativeUnknown
RepairPolicy:   1 DiagnoseOnly, 2 OwnerSpecific, 3 RebuildDerived,
                4 RecoveryReplay, 5 ManualRequired
MigrationPolicy: 1 RequiredCopy, 2 DestinationLocal, 3 RebuildDestination,
                  4 OwnerConverter, 5 OmitDeclared, 6 FailUnknown
SpillPolicy:    1 Ineligible, 2 HotTailSource, 3 RecoveryArtifact,
                4 ResumableWorkspace
Sensitivity:    0 Internal, 1 Protected, 2 Credential, 3 Secret,
                4 PublicMetadata
EventPolicy:    0 None, 1 AuthorizedNamespace, 2 SystemAdministrative,
                3 OperationalRedacted, 4 SensitiveSuppressed
AbsencePolicy:  1 AllowedDefault, 2 AllowedEmpty, 3 DegradedVisible,
                4 RebuildRequired, 5 FatalIfAuthoritative,
                6 DisableDestructiveGc, 7 LegacyDiagnostic
UnknownChildPolicy: 0 NoChildren, 1 Reject, 2 ClassifyByRegistry,
                    3 RetainAndFailComplete
~~~

Path matcher bytes are canonical UTF-8 absolute paths with `/` separators,
no NUL, dot segment, duplicate separator, or trailing slash except `/` itself.
`AbsolutePathPrefix` ends in `/`. `DescendantReservedFile` is
`segment_len u16 || segment || suffix_len u16 || suffix`; it requires at least
one ordinary ancestor segment and therefore cannot also match an absolute-root
control. `DescendantReservedSubtree` is `segment_len u16 || segment` and has
the same ordinary-ancestor requirement but matches the subtree below that
segment. `ReservedPathSegment` uses the same bytes without that requirement.
EntryType, control-tag, and workspace matchers are one assigned `u16`; a KV
prefix is raw nonempty bytes. Exact path wins over descendant-file, then
descendant-subtree, then longest prefix or segment match; equal-priority
cross-family overlap is a build-time registry error. An item
that reaches a protected domain without a known winner becomes family
`0xFFFE`; it is never accepted as ordinary user data.

The matrix abbreviations expand to the numeric policies above. `I`, `V`, `O`,
`L`, `R`, `N`, and `F` mean `RequiredInclude`, `OptionalValidated`,
`OmitDeclared`, `NodeLocal`, `RedactOmit`, `NamedSubsetOnly`, and
`FailUnknown`. Physical byte/COW copy uses
`RequiredInclude` for every in-file family and `NodeLocal` for external
workspaces. Import mirrors logical backup only for included/validated data;
redacted, omitted, and node-local data remain destination-owned. Every present
known authoritative family verifies strictly; derived families verify as
rebuildable; unknown protected state uses `ConservativeUnknown`. Repair and
migration are owner-specific unless the row says derived/rebuild, legacy,
node-local, or unknown/fail. Only the WAL hot tail may be a `HotTailSource`,
family `0x0070` is a `RecoveryArtifact`, and `0x0071` is a
`ResumableWorkspace`; all other families are spill-ineligible. The checked-in
registry fixture must expand every row to one or more complete descriptor bytes and is rejected
at build time unless it agrees with these rules and the operation matrix.
`cluster_join_policy` is `NamedSubsetOnly` only for family `0x0016`'s named
shared JWT/signing material and `OmitDeclared` for every other v1 family; node
identity and peer records are created/adopted by the join state machine, not
copied as foreign file authority.

Policy abbreviations in the complete v1 family matrix are:

~~~text
Semantic: - none, C canonical source/projection, D exact executable dependency,
          A authoritative semantic object, X disposable derived state
Transfer: I include/required, O omit and declare, L node-local, V optional only
          through validated compatible transfer, R redact/omit sensitive,
          N named authenticated-protocol subset only
GC:       T trace typed edges, P pin while authoritative, Q ordinary quarantine,
          D derived quarantine, E evidence-retention policy, R conservative retain
Failure:  M missing allowed, G degraded, F operation fails, R read-only/retain
~~~

Physical byte/COW copies include every in-file family exactly; the table's
backup column governs logical database backup.

| ID | Canonical family/matcher | Sem | GC | Logical backup | Peer | Data/client | Sensitivity/failure rule |
| ---: | --- | :---: | :---: | :---: | :---: | :---: | --- |
| `0x0001` | exact `/.aeordb-config/indexes.json` | C | T | I | I | O | Missing means canonical defaults; malformed blocks semantic activation |
| `0x0002` | non-root descendant `*/.aeordb-config/indexes.json` | C | T | I | I | I under normal path auth | Part of namespace tree and semantic projection |
| `0x0003` | exact `/.aeordb-config/parsers.json` | C | T | I | I | O | Missing canonical empty; malformed blocks affected semantics |
| `0x0004` | exact `/.aeordb-config/lifecycle.json` plus typed last-known-good control | - | P | I | I | O | Destructive policy fail-closed per Category 4 |
| `0x0005` | exact `/.aeordb-config/cron.json` | - | P | I | I | O | Operational; malformed disables schedules visibly |
| `0x0006` | exact `/.aeordb-config/webhooks.json` | - | P | I | I | O | Operational; secrets redacted from diagnostics |
| `0x0007` | exact `/.aeordb-config/cors.json` | - | P | I | I | O | Operational HTTP policy |
| `0x0008` | other descendant `*/.aeordb-config/**` entries | - | T | I | I | I under path auth | Preserved namespace content; unknown semantics forbidden |
| `0x0009` | exact `/.aeordb-config/runtime.json` plus typed LKG/diagnostics controls | - | P | V | O | O | Host-sensitive operational policy; backup may carry it only as validated inactive input |
| `0x0010` | prefix `/.aeordb-system/users/` | - | P | I | I | O | Protected auth state; malformed entry fails auth/transfer completeness |
| `0x0011` | prefix `/.aeordb-system/groups/` | - | P | I | I | O | Protected auth state |
| `0x0012` | prefix `/.aeordb-system/permissions/` | - | P | I | I | O | Protected auth state; omission is fatal to semantic DB clone/peer |
| `0x0013` | prefix `/.aeordb-system/api-keys/` | - | P | R | L | O | Node/database-local credential; omitted from logical restore and peer data sync |
| `0x0014` | prefix `/.aeordb-system/refresh-tokens/` | - | Q | R | L | O | Node/database-local credential; normal expiry cleanup plus GC |
| `0x0015` | prefix `/.aeordb-system/magic-links/` | - | Q | R | L | O | Node/database-local credential; normal expiry cleanup plus GC |
| `0x0016` | prefix `/.aeordb-system/config/` including `jwt_signing_key` | - | P | R | L | O | Secret; join protocol alone transfers named required keys |
| `0x0017` | exact `/.aeordb-system/email-config.json` | - | P | R | L | O | Node-local secret-bearing operational config |
| `0x0018` | prefix `/.aeordb-system/join-audit/` | - | Q | O | L | O | Node-local audit; physical copy only by default |
| `0x0020` | exact `/.aeordb-system/cluster/node_id` | - | P | O | L | O | Must be regenerated/adopted per node |
| `0x0021` | exact `/.aeordb-system/cluster/peers` | - | P | I | L | O | Operator config; never copied as destination authority by peer data sync |
| `0x0022` | prefix `/.aeordb-system/sync-peers/` | - | Q | O | L | O | Node-local resumability; reset safely on clone/import |
| `0x0030` | prefix `/.aeordb-system/plugins/` legacy records | - | Q | I | I | O | Migration input; cannot satisfy corrected ABI by itself |
| `0x0031` | prefix `/.aeordb-system/plugin-aliases/` | C when referenced | T | I | I | O | Mutable alias never historical authority by itself |
| `0x0032` | prefix `/.aeordb-system/plugin-artifacts/blake3/` | D | T/Q | I | I | O | Required referenced modules transfer even when indexes are omitted |
| `0x0033` | prefix `/.aeordb-system/semantic-objects/` | A | T/Q | I | I | O | Any missing referenced object fails semantic transfer/verify |
| `0x0040` | snapshot EntryType records | - | P/Q | I | I | O | Authoritative root records, strict decode |
| `0x0041` | fork EntryType records | - | P/Q | I | I | O | Authoritative mutable root records, strict decode |
| `0x0042` | background task KV domain/registry | - | P/Q | O | L | O | Operational; active migration/build roots traced |
| `0x0043` | migration/repair/lease/root-pin controls | - | P/Q | O | L | O | Active roots traced; exact controls frozen Round 14 |
| `0x0044` | legacy-root map and root-state/expiry controls | - | T/E | I | I | O | Required for deterministic migrated sync/root behavior |
| `0x0050` | IndexArtifact EntryType | X | D | V | V | O | Omission means `needs_rebuild`; corrupt active generation degrades only index |
| `0x0051` | GcArtifact EntryType | - | E/Q | O | L | O | Corruption disables destructive GC; never foreign authority |
| `0x0052` | DeletionRecord EntryType | - | P | O | L | O | Recovery state; physical copy/migration only |
| `0x0053` | Void allocator/hot-tail state | - | P | O | L | O | Physical allocator authority, never logical transfer |
| `0x0054` | DatabaseHeader A/B slots and HEAD/base/target controls | - | P | O | L | O | Physical authority; logical transfer constructs destination-local slots |
| `0x0055` | KV block/pages, resize buffers, and locator snapshot | - | P | O | L | O | Physical lookup authority; rebuild/verify rather than logical copy |
| `0x0056` | database NVT locator region | X | D | O | L | O | Disposable physical hint; never namespace/index semantic authority |
| `0x0057` | WAL hot-tail/buffer publication controls | - | P | O | L | O | Durability/recovery authority; physical migration only |
| `0x0060` | nested `.aeordb-indexes` legacy files | X | D | O | O | O | Migration/rebuild input only; never v1 authority |
| `0x0061` | nested `.aeordb-logs` | - | Q | O | O | O | Operational diagnostics; log failure never erases primary error |
| `0x0062` | legacy `/.aeordb-system/snapshots` files | - | R | O | O | O | Explicit migration diagnostic; actual snapshots are EntryType records |
| `0x0063` | legacy `apikeys` and `cluster/sync` aliases | - | R | O | O | O | Must migrate/verify or fail; never coexist as second authority |
| `0x0070` | external emergency hot-tail spill | - | P | O | L | O | Startup fatal until explicit repair/replay |
| `0x0071` | external migration/GC/index workspaces | - | P/Q | O | L | O | Resume/cleanup through owning task and database ID |
| `0xFFFE` | unknown protected absolute child/domain | - | R | F | F | O | GC retains; backup/peer/verify/migration fail typed incomplete |

`0x0000` is invalid and all unassigned IDs remain reserved. Family `0xFFFE`
is an engine result, not a persisted descriptor writers may select.

The operation rules are:

- **GC:** derive roots/typed edges/retention from descriptors. Unknown protected
  state is retained and disables sweep for its affected closure.
- **Logical semantic backup:** include namespace data, semantic objects,
  canonical config projections, aliases, and every referenced exact module.
  Credentials are redacted under the existing new-database-identity contract.
  Omission/corruption of a required family fails the backup; no warning-success.
- **Data-only export:** may omit absolute semantic/system state but declares
  `semantic_state = unavailable/reconfiguration_required`. Nested config files
  remain namespace data; they do not falsely prove missing global dependencies.
- **Peer replication:** transfers database-authoritative noncredential auth
  records and semantic families
  only between peers proving the same database identity and negotiated
  capabilities. Node identity, peer topology/progress, tasks, GC, and allocator
  state remain local. API keys, refresh tokens, magic links, and email secrets
  remain local under the existing credential policy. The cluster join protocol
  may transfer only its explicitly named shared JWT/signing material. Derived
  indexes use only the separately validated optional transfer protocol.
- **Client sync:** never exposes absolute protected state. Nested config remains
  subject to ordinary path authorization and internal-path policy.
- **Import/migration:** validate all required families and dependencies before
  authority publication. Unknown/missing required state aborts without changing
  the target. Credentials and node-local state follow explicit target policy.
- **Verify/repair:** every registered family has one typed verifier and stated
  repair boundary. A missing optional family is not corruption; an unreadable
  present family is never silently equivalent to absence.

The current `augment_with_system_subtrees`, `export_system_subtree`, GC prefix
array, sync filter lists, and cache invalidation path switches become consumers
of the registry/coordinator or are deleted. Pairwise agreement tests and a grep
gate reject another hard-coded protected-family array.

### 7. Legacy v3 roots and migration-safe sync

**DECIDED (self):** V3 directory roots cannot be assigned historical global
parser/plugin/config semantics that were never captured. Migration wraps what
it can prove and says exactly what it cannot.

The wire/storage `RootFormat` registry is distinct from `DatabaseFormat`:

~~~text
0x0000  LegacyDirectoryTreeRootV0
0x0001  NamespaceRootV1
0x0002..0xFFFF reserved; reject before root lookup
~~~

A v3 database normally emitted root format 0. A v4 database emits format 1;
`source_database_format = 3` below describes the old container, not the root
format. When `since_root_hash` is present, new clients must send
`since_root_format`. During the coordinated migration release only, an omitted
format is resolved by first requiring a valid NamespaceRootV1 and otherwise an
exact finite `LegacyRootMapV1` hit. Failure returns the reset contract below;
the compatibility path never admits an ordinary legacy DirectoryIndex by
shape alone. If no base hash is supplied, its format must also be absent.

For every database-owned v3 root known at migration preflight (HEAD, snapshots,
forks, active database-owned sync bases/pins, and migration/maintenance roots):

1. Copy/validate its exact namespace tree into the v4 destination.
2. For current HEAD, compile one exact v4 semantic state representing the
   validated legacy/v0 behavior, controls, and dependencies captured at the
   cutover boundary.
3. A root byte-identical to that captured HEAD may reuse the complete state.
4. Another historical root receives a complete semantic state only when
   migration can independently prove its exact effective global and executable
   dependencies. Nested config bytes alone are insufficient.
5. Otherwise bind the tree to canonical content-only `SemanticStateRootV1`
   with reason 1, 2, or 3. Listing, metadata, direct/batch fetch, and range
   extraction remain available. Query, search, aggregation, semantic sort,
   semantic locators, and parser fallback return
   `503 HISTORICAL_VIEW_UNAVAILABLE`.
6. Create and verify the corresponding `NamespaceRootV1`; never mutate the v3
   source or claim the old hash already carried semantics.

Migration persists a finite immutable `LegacyRootMapV1` for roots it actually
knows:

~~~text
source_database_id
source_database_format = 3
legacy_root_hash H
namespace_root_v1 H
semantic_availability = complete | content_only
source_kind = head | snapshot | fork | sync_pin | maintenance
source_name/identity (bounded, diagnostic)
mapping_migration_id
~~~

Its exact paged control bytes are frozen in Round 14. The map is included in
logical semantic backup and same-database peer replication. It is finite at
migration cutover and is not extended for every old HEAD that the database had
already forgotten.

A known legacy root supplied to a read or sync request maps deterministically
to its v4 root. The successful response reports the v4 `root.hash` and:

~~~http
X-AeorDB-Root-Mapped-From: <legacy hash>
X-AeorDB-Root-Format: 1
~~~

Semantic routes still honor the mapped state's complete/content-only status.
An unknown v3-era hash is never guessed from an ordinary DirectoryIndex. Direct
reads return `422 INVALID_NAMESPACE_ROOT` unless a coordinated client supplies
the migration metadata below.

The sync protocol adds and returns:

~~~json
{
  "database_id": "<stable database UUID>",
  "root_format": 1,
  "since_root_hash": "<optional>",
  "since_root_format": 1
}
~~~

A client persists `database_id`, `root_format`, and the reported root together.
If the database identity differs, a supplied base is no longer retained, or a
v3 base has no known map, the server returns
`409 SYNC_BASE_RESET_REQUIRED` with no file/chunk changes:

~~~json
{
  "code": "SYNC_BASE_RESET_REQUIRED",
  "database_id": "<current>",
  "root_format": 1,
  "current_root_hash": "<current v4 root>",
  "provided_root_hash": "<old base>",
  "reset_required": true,
  "reason": "unknown_legacy_root"
}
~~~

Stable reset reasons are `database_id_mismatch`, `base_not_retained`,
`unknown_legacy_root`, and `unsupported_root_format`. Authorization-safe
responses may omit the provided hash/current root detail, but never the code,
reason, reset flag, current database ID, or current root format.

The client must explicitly begin bounded manifest/path-hash reconciliation or
confirm a full baseline. The server never treats a missing supplied base as an
empty tree and never starts an accidental multi-terabyte transfer. Omitting
`since_root_hash` remains the caller's explicit full-baseline request.

Snapshot/fork restore of a content-only legacy root requires the caller to
choose `semantic_state = current_supported` (or a future explicit exact state),
then produces a **new** v4 root. It never rewrites the retained historical root
or pretends its old queries were evaluated under that new state.

### 8. One `ResolvedReadView` for every read

**DECIDED (self):** One engine-owned `SemanticReadViewResolver` resolves every
HTTP and embedded read. Route handlers receive a `ResolvedReadView`; they do
not accept a raw hash or call `head_hash`, `walk_version_tree`,
`resolve_file_at_version`, mutable parser/config loaders, or current plugin
aliases themselves.

The logical contract is:

~~~text
ReadViewSelector {
  root_hash: Option<Hash>
  root_format: Option<RootFormat>   // sync/migration compatibility only
}

ResolvedReadView {
  database_id: 16 bytes
  root_format: RootFormat::NamespaceRootV1
  root_hash: H
  mapped_from_legacy_root: Option<H>
  namespace_tree_codec: u16
  namespace_tree_root: H
  semantic_state_id: H
  semantic_availability: Complete | ContentOnly(reason)
  root_state: Live | Retained | PendingDelete
  expires_at_ms: Option<i64>
  request_pin: owned guard
  authorization_snapshot: request-admission current auth/group/permission view
  required_reader_capabilities: 256-bit bitset
}
~~~

The view deliberately contains no IndexArtifact generation, pointer, NVT cell,
physical offset, page ID, WAL locator, query plan, or cache handle. Query
planning may pin a compatible derived closure **after** resolving the view, but
that closure never redefines it.

If `root_hash` is omitted, the resolver captures current HEAD once. HEAD in v4
must itself be a valid NamespaceRootV1. Every operation below consumes the same
view contract:

- query, search, aggregation, sort, pagination, and EXPLAIN;
- root and nested directory listing;
- direct GET/HEAD and whole/batch fetch;
- line, character, JSON, locator, and snippet/range fetch;
- snapshot/fork historical file routes;
- backup/diff/export source selection and sync base/current selection; and
- embedded SDK equivalents.

GC, verify, migration, and repair use a privileged variant with the same
structural resolver plus operation-specific authority/pins. They may diagnose a
corrupt closure; they do not weaken public admission.

All successful namespace reads return the already-approved root object and
headers. `retained` is now a defined third state:

~~~json
{
  "root": {
    "hash": "<v4 root>",
    "state": "retained",
    "expires_at": null
  }
}
~~~

The stable error table is extended, not replaced:

| HTTP | Code | Condition |
| ---: | --- | --- |
| 409 | `SYNC_BASE_RESET_REQUIRED` | Coordinated sync base has wrong DB identity, is unavailable, or cannot map across migration |
| 410 | `ROOT_EXPIRED` | Root is logically unavailable/reclaimed and bounded evidence remains |
| 422 | `INVALID_NAMESPACE_ROOT` | Well-formed hash is not an admitted root or expiry evidence aged out |
| 503 | `HISTORICAL_VIEW_UNAVAILABLE` | Root is content-readable but exact semantics/executor are unavailable |
| 500 | `DATABASE_CORRUPTION` | Authoritative/admitted root or immediate typed closure is corrupt |

`HISTORICAL_VIEW_UNAVAILABLE` is route-sensitive: a content-only view still
serves structural/content routes. Missing an executor does not invalidate a
complete precomputed immutable index read, but it blocks authoritative parser
fallback, rebuild, and mutations that require that executor.

### 9. Publication, backup, GC, and replication closure rules

One namespace root is complete only when all required edges are handled under
their typed policy:

~~~text
NamespaceRootV1
  -> namespace_tree_root
       -> directories -> FileRecords/symlinks -> chunks
  -> SemanticStateRootV1
       -> semantic catalog radix nodes
       -> canonical definition records
       -> exact referenced plugin/native dependencies
       -> referenced plugin module archives
~~~

Derived IndexArtifact pointers/pages and GcArtifact history are outside this
closure. Current auth/operational controls are also outside it and follow the
system-family matrix.

Publication is dependencies-first and authority-last. Backup, same-database
peer replication, migration, and full verify trace the exact closure and fail
typed incomplete if a required edge cannot be read or validated. They cannot
substitute a mutable current alias/config. Data-only export may omit the
semantic branch only with explicit unavailable metadata.

GC's live mark roots are HEAD, snapshots, forks, durable active
tasks/migrations, and request pins, with their exact typed namespace/semantic
edges. A structurally valid former root not reached from that live set remains
`retained` before candidate publication and may then enter `pending_delete`
under the frozen multi-mark/grace rules. Pending roots remain readable and are
revalidated for resurrection/request-pin races, but pending state alone does
not mark their closures live forever. A semantic definition's dependency
fields are typed references; GC never scans arbitrary bytes for something
hash-shaped. Missing or unknown semantic edges disable reclaim for the
affected closure.

Peer replication first negotiates database ID, DatabaseHeader/capabilities,
system registry version/fingerprint, semantic-object codecs, and exact module
availability. It transfers immutable objects/modules before root authority and
publishes a destination-local control/root update last. Node-local controls and
foreign derived/GC pointers never become destination authority.

### 10. AGIS test protocol and proof obligations

The feature hypothesis is:

> For any admitted root, every read surface resolves one immutable namespace
> and one exact semantic world, while protected transfer and GC either preserve
> the complete typed closure or fail visibly without publishing authority.

Tests are written before each production writer. Unit/fixture tests use the
repository's short timeout discipline; bounded migration/crash/E2E harnesses
declare explicit larger deadlines and progress. No test may wait indefinitely
on a task, server, plugin, or GC generation.

#### Golden and codec proof

- Hand-authored 32- and 64-byte-hash fixtures cover `ADIR` namespace roots,
  every `ASEM` kind, definition records, radix leaf/internal nodes, system
  registry descriptors, exact CRCs, key domains, lengths, reserved bytes, and
  minimum/maximum legal records on Linux, macOS, and Windows.
- Reader-first tests prove v3 physical framing with FileRecord payload v1 is
  not mistaken for v4 framing, and v4 `(DirectoryIndex, EntityVersion 0)` tree
  nodes coexist with v1 NamespaceRoot entities.
- Mutation/fuzz tests reject unknown kind/version/capability, truncation,
  trailing bytes, arithmetic overflow, allocation amplification, wrong key/ID,
  noncanonical booleans/reserve/counts, malformed UTF-8/path, CRC mismatch,
  radix depth/prefix/edge errors, duplicate records, and collision-leaf bounds.
- Independent reference construction shuffles insertion order, batching,
  restart boundaries, config JSON/member order, and map implementations.
  Equivalent semantics produce byte-identical catalog/state/root IDs; every
  meaningful definition/dependency/compiler change changes the correct ID.

#### Root resolver and authorization proof

- Agreement tests feed HEAD movement, live snapshot/fork, retained former root,
  pending root, eligible/reclaimed root, prepared orphan root, nested B-tree
  node, nested directory, random hash, corrupt authoritative root, known v3
  map, and unknown v3 base through every HTTP/embedded route.
- A prepared/aborted NamespaceRoot tail is never KV-visible or admissible,
  including after dirty-start rebuild, verify, repair, and migration. The same
  bytes become admissible only through a committed first-authority transaction.
- Omitted root captures HEAD once while concurrent writes advance it. Query to
  locator to range fetch reproduces exact bytes from one reported root.
- Request-pin versus sweep tests exercise both lock orders and every crash-safe
  candidate transition. Reads never refresh retention.
- Current auth changes and historical reads prove current authorization is
  reapplied before root status, names, counts, positions, snippets, timing, and
  EXPLAIN. A root hash never bypasses workspace/path/share/key restrictions.
- Architecture gates reject direct route calls to `head_hash`, raw historical
  tree walkers, mutable parser/config/alias loaders, or route-local root-state
  logic.

#### Activation and crash proof

- Target-failing tests cover every semantic-input-changing producer: HTTP
  PUT/delete, blob/batch, merge, copy/move, transform/import/sync, plugin
  deployment/removal, plugin host write, and embedded APIs. Pairwise agreement
  proves all return one staged task contract and no path becomes visible early.
  Exact already-materialized v4 snapshot/fork selection instead proves the
  coordinator's short synchronous authority path; legacy restore that selects
  new semantics remains task-backed.
- Fault injection stops before/after module archive, definition object, radix
  node, semantic state, NamespaceRoot, staged control, coverage transition,
  hot-tail flush/sync, DatabaseHeader/HEAD publication, and task completion.
  Reopen selects the complete old or new view, never mixed semantics.
- The root-publication crash matrix permutes root bytes, KV visibility,
  transaction marker, first authority, HEAD selection, sync, and dirty rebuild.
  No incomplete ordering admits the new root; no complete ordering loses it.
  A code-architecture gate permits only the root codec to construct bytes and
  only the publication coordinator to persist them; migration must call that
  coordinator rather than own a second writer.
- Concurrent ordinary writes rebase without semantic recompilation when safe.
  Concurrent semantic writes conflict/supersede deterministically; no
  last-writer-wins registry guess activates stale output.
- Cancellation, shutdown, disk-full, durability latch, missing dependency,
  corrupt source config, memory admission, and worker crash leave the old
  semantic world active and expose a durable actionable task failure.
- A large synthetic catalog compiles/updates under the 16 GiB host profile with
  bounded pages/stacks/cache admission and no whole-catalog vector.

#### Protected-family and transfer proof

- One registry completeness test enumerates every current path constant,
  reserved segment, EntryType, KV domain, header control, and external
  workspace. Every protected item matches exactly one family; overlaps resolve
  through permanent priority rules; unknowns become `0xFFFE`.
- Pairwise agreement tests run GC, backup, export/import, peer/client sync,
  verify, repair, migration, spill, cache invalidation, SSE/metrics, and public
  path denial against every descriptor. Hard-coded-list grep gates are zero.
- Each present-but-corrupt family is distinguished from absence. Backup,
  replication, verify, and migration cannot warn/continue to successful
  completeness; unknown protected data is retained by GC.
- Security fixtures prove credentials are absent from logical backup/data
  export/client sync/ordinary peer replication/diagnostics; only the explicitly
  named cluster signing material may cross the authenticated join contract.
  Semantic objects and root metadata never expose any credential.
- Semantic backup and peer transfer omit derived pages but include every exact
  definition/module needed to rebuild. Data-only import reports semantic
  unavailability instead of claiming `needs_rebuild` with missing executors.

#### Migration and real-world proof

- Verified v3 fixtures and a copied production database map current HEAD,
  snapshots, forks, known sync pins, duplicate-root names, content-only roots,
  corrupt legacy controls, and unavailable modules without mutating the source.
- Known root mappings preserve incremental sync. Unknown mappings return 409
  with zero changes/chunks and require explicit bounded reset. A missing base
  can never silently become full sync.
- Restore of a content-only root requires explicit supported semantics and
  creates a distinct v4 root. Direct/range reads of the old root remain exact;
  semantic routes fail with the stable typed error.
- A real `/tmp/codex` database is exercised through HTTP and embedded SDK:
  deploy parser/module, create nested/global configs, write/query/search/range
  fetch, mutate aliases/config under concurrent ordinary writes, snapshot,
  restart, export/import, peer-sync, run two GC marks, read retained/pending
  roots, reclaim one, and verify root/semantic/module behavior.
- Release candidates repeat root/semantic golden and migration-read tests on
  Linux, macOS, and Windows before v4 activation.

False-confidence patterns explicitly rejected are serializer-generated
goldens, using the production compiler as the reference canonicalizer, mocking
away persistence/backup/GC/restart, testing only HEAD, one pleasant E2E run,
and asserting that a warning log makes an incomplete protected transfer safe.

### 11. Round 10 parent-plan corrections

The umbrella and child plans must now make these mechanical corrections:

1. Replace `RootProvenanceStore`/per-published-root records with structural
   `NamespaceRootV1`, atomic committed first-authority publication, GC
   candidate state, and bounded post-reclaim `RootExpiryCatalog`. Add the
   `retained` public state; uncommitted prepared roots are never admitted.
2. Replace every hidden semantic-anchor path and current-config historical
   fallback with `SemanticStateRootV1` and immutable semantic objects.
3. Correct P1/P3 wording that implies every v4 entity has EntityVersion 1.
   Database format selects physical framing; `(EntryType, EntityVersion)` still
   owns each complete entity codec, and migrated v0/v1 payload semantics remain
   explicit.
4. Make every mutation that compiles or changes semantic inputs task-backed
   `202`/typed-handle with atomic activation and no early batch visibility;
   preserve only the exact-root selection fast path described above.
5. Replace all protected path arrays and warning-success walkers with the
   permanent registry and complete matrix above.
6. Add credentials/node-local/semantic/derived distinctions to backup, peer,
   client sync, import, migration, verify, repair, and GC child plans.
7. Add canonical content-only semantic states, finite legacy-root maps, sync
   `database_id`/`root_format`, and zero-change
   `SYNC_BASE_RESET_REQUIRED` behavior.
8. Require one `ResolvedReadView` for HTTP, embedded, backup/diff, sync,
   snapshot/fork, and maintenance selection; update the mutation/consumer grep
   gates accordingly.
9. Keep v4 cutover blocked on physical retirement/GC support: semantic objects
   and root envelopes can be written shadow-only in P3, but stable authority
   cannot cut over before P4 recovery/sweep proofs are green.
10. Add the exact Round 10 fixtures and agreement oracles to P0/P1 before any
    writer is enabled.

### Round 10 verdict

Round 10 is closed. Given any retained v4 root, a reader can mechanically
resolve exactly one namespace-tree root and either one exact complete semantic
state or one stable content-only reason without consulting mutable current
semantic controls. Internal directory nodes cannot masquerade as `/`.

The design avoids three tempting but harmful shortcuts: a hidden semantic file
inside the namespace, an unbounded provenance write for every HEAD, and a
mutable registry file that can weaken safety policy. It remains aligned with
AeorDB's established spirit: content-addressed authority, immutable snapshots,
typed versioned bytes, rebuildable indexes, bounded memory, conservative GC,
dependencies-first publication, explicit migration, and fail-loud correctness.

The next round may define converter and strategy semantics. It may not reopen
root identity, semantic-state inclusion, mutation activation, root lifecycle,
protected-family ownership, legacy fidelity, or `ResolvedReadView` merely to
make a local implementation easier.

# Round 11: Converter, Strategy, and Definition Semantics

**Status:** CLOSED by Planning Cap and AGIS self-decision. This round freezes
the semantic compiler boundary required by Round 12. It does not freeze page
packing, NVT resolution, cache policy, or task scheduling.

## 1. Evidence and Corrections to the Current Implementation

The live code confirms that a compatibility adapter is mandatory:

- `json_query_value_to_bytes` converts every JSON number through an unsigned
  big-endian byte representation, including lossy float casts;
- numeric converters silently return coordinate zero for short/malformed
  values and constructors accept invalid or reversed ranges;
- timestamp conversion accepts several ambiguous text forms and maps parse
  failure to the Unix epoch;
- `StringConverter` combines the first byte and length, which is neither an
  equality key nor an order-preserving collation;
- `f64` maps NaN to zero and clamps infinities without a stable equality rule;
- persisted `f64` coordinates are compared with partial ordering;
- trigram and phonetic behavior depends directly on library/runtime Unicode
  behavior without a persistent semantic fingerprint;
- the current query parser compiles literals before resolving the field's
  definition; and
- tokenizing indexes use scalar collisions as candidate identity and retain an
  unbounded in-memory raw-value map for correctness rechecks.

None of those behaviors becomes the corrected v1 contract by accident.
Migration preserves each effective v0 behavior under named adapter IDs. New
definitions use the corrected contracts below.

## 2. Exact `ConverterDefinitionV1`

`ConverterDefinitionV1` uses the approved 32-byte `ACNV` definition envelope.
Let `P` be `parameter_length`. Its body is:

| Definition offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 32 | 2 | `converter_id` | Permanent nonzero registry ID |
| 34 | 2 | `converter_semantics` | `1` corrected or a named migration adapter |
| 36 | 4 | `accepted_source_type_mask` | Only known canonical source tags |
| 40 | 2 | `canonicalization_semantics` | Permanent nonzero ID |
| 42 | 2 | `comparison_semantics` | Permanent nonzero ID |
| 44 | 2 | `coordinate_semantics` | Permanent nonzero ID |
| 46 | 2 | `collation_semantics` | Zero when inapplicable |
| 48 | 2 | `tokenizer_semantics` | Zero when inapplicable |
| 50 | 2 | `expansion_semantics` | Permanent nonzero ID |
| 52 | 2 | `invalid_value_semantics` | Permanent nonzero ID |
| 54 | 2 | `expanded_duplicate_semantics` | Permanent nonzero ID |
| 56 | 4 | `parameter_length` | `P`; at most 64 KiB minus fixed bytes |
| 60 | 4 | `converter_flags` | Zero in v1 |
| 64 | 8 | `max_input_value_bytes` | Nonzero, finite |
| 72 | 4 | `max_output_keys_per_value` | Nonzero, finite |
| 76 | 4 | `max_single_posting_key_bytes` | Nonzero, finite |
| 80 | 8 | `max_total_posting_key_bytes_per_value` | Nonzero, finite |
| 88 | 32 | `semantics_fingerprint` | Exact built-in/adaptor behavior fingerprint |
| 120 | `P` | `parameters` | Converter-specific canonical bytes |

The complete length is exactly `120 + P`, with no trailing bytes. The enclosing
FieldIndexDefinition cap remains authoritative. A decoder validates the known
registry row, legal type mask, exact parameter length, semantic-ID
combination, finite bounds, and fingerprint before allocating parameter data.

`ConverterFingerprint` remains:

~~~text
H("aeordb.index.converter-definition.v1\0" || complete ACNV bytes)
~~~

The fingerprint at offset 88 is not that database-dependent ID. It is the
fixed 32-byte BLAKE3 digest of the converter's canonical behavioral spec bundle
and conformance corpus, as defined in section 11.

## 3. Permanent Converter Registry

Corrected converter IDs are:

| ID | Canonical name | Source values | Posting-key/order purpose |
| ---: | --- | --- | --- |
| `0x0001` | `typed_exact_blake3_v1` | Any scalar canonical type | Structural equality; hash-distributed hint |
| `0x0002` | `bytes_binary_order_v1` | bytes | Raw byte lexicographic order |
| `0x0003` | `utf8_binary_order_v1` | UTF-8 | Raw UTF-8 byte order; no locale |
| `0x0004` | `u64_order_v1` | u64 or nonnegative in-range i64 | Unsigned numeric order |
| `0x0005` | `i64_order_v1` | i64 or in-range u64 | Signed numeric order |
| `0x0006` | `f64_finite_order_v1` | finite f64 or exactly representable integer | Corrected finite IEEE order |
| `0x0007` | `timestamp_ms_order_v1` | in-range integer or strict timestamp text | UTC Unix milliseconds |
| `0x0008` | `bool_order_v1` | bool | false before true |
| `0x0009` | `unicode_trigram_v1` | UTF-8 | Word and substring trigram expansion |
| `0x000A` | `soundex_ascii_v1` | UTF-8 | Aeor Soundex v1 codes |
| `0x000B` | `double_metaphone_primary_ascii_v1` | UTF-8 | Aeor DM primary v1 codes |
| `0x000C` | `double_metaphone_alt_ascii_v1` | UTF-8 | Aeor DM alternate-only v1 codes |

The following IDs are migration-only and can be decoded/read/rebuilt but never
selected by a new `$v: 1` user definition:

~~~text
0x8001 hash_v0                 0x8008 string_v0
0x8002 u8_v0                   0x8009 timestamp_v0
0x8003 u16_v0                  0x800A trigram_v0
0x8004 u32_v0                  0x800B soundex_v0
0x8005 u64_v0                  0x800C dmetaphone_primary_v0
0x8006 i64_v0                  0x800D dmetaphone_alt_v0
0x8007 f64_v0
~~~

Those adapters preserve the current effective byte interpretation, range
casts, timestamp fallbacks, tokenization, alternate-code fallback, scalar
calculation, and query recheck behavior. They do not inherit corrected v1
behavior merely because the implementation shares helper code.

Converter registry IDs are permanent and never reused. Unknown IDs or known
IDs with the wrong semantic fingerprint make the definition unsupported, not
"best effort." A capable reader may still consume a complete materialized
posting closure without the builder capability only when the manifest declares
no operation that requires converter execution; fallback/rebuild remains
unavailable until the exact converter capability exists.

## 4. Canonical Posting Keys, Comparison, and Coordinates

Correctness uses complete posting keys and ValueStore values. NVT coordinates
are hints only.

### Canonical equality and ordering

- `typed_exact_blake3_v1` equality is byte identity of the complete
  `CanonicalSourceValueV1`, including its type tag. It does not order values.
- bytes order by unsigned raw bytes, prefix-shorter first.
- UTF-8 order by unsigned encoded bytes, prefix-shorter first. V1 has no
  locale-sensitive collation and never consults process locale.
- u64 posting keys are eight-byte little-endian canonical values.
- i64 and timestamp keys are eight-byte little-endian two's-complement values.
- f64 canonicalizes `-0.0` to `+0.0`; finite values use the standard sortable
  numeric comparison but remain eight-byte little-endian IEEE-754 canonical
  values. NaN and either infinity are invalid.
- bool keys are one byte, `0x00` false and `0x01` true.
- token keys begin with a one-byte token class followed by the exact canonical
  token bytes. Hash coordinates never replace those bytes.

The physical posting total order is `(normalized_coordinate,
converter_semantic_compare(posting_key), document_ordinal,
source_value_ordinal, expansion_ordinal)`. For every ordered converter the
coordinate is monotone, so this is the same semantic value order with lossy
coordinate ties resolved by the complete value. Exact/token strategies receive
a stable distributed order suitable for point lookup but make no range promise.
Bytes, UTF-8, and token converters compare their canonical byte keys directly;
numeric converters decode their exact little-endian fixed-width key and compare
the typed value. A page may prefix-compress byte-comparable keys physically in
Round 12, but numeric keys are not lexicographically reinterpreted. Coordinate
collision therefore creates scan work only; it cannot create equality or
violate the Round 3B little-endian rule.

### Fixed-point normalized coordinate

Corrected v1 does not persist or compare `f64` NVT scalars. It uses
`NormalizedCoordinateV1 = u64`, interpreted as a fixed-point point in `[0,1]`.
For resolution `R`, the cell is:

~~~text
cell = min(R - 1, (coordinate * R) >> 64)
~~~

with a `u128` intermediate. The all-ones coordinate maps to `R - 1`.

- u64 uses its numeric value directly.
- i64/timestamp compute a transient sign-bit-flipped integer from the decoded
  little-endian canonical value.
- finite f64 computes the standard sortable-bit transform transiently from the
  decoded little-endian canonical value.
- bool uses zero or `u64::MAX`.
- bytes/UTF-8 use the first eight bytes of the ordered key as big-endian,
  right-padded with zero. This is monotone but intentionally lossy.
- exact, trigram, and phonetic keys use the first eight bytes of
  `BLAKE3(domain || complete key)` as big-endian.

Range planning computes endpoint coordinates from the same converter, widens
to include the predecessor cell and successor cell when present, then verifies
complete posting keys while scanning. Empty, collided, stale, sparse, or
corrupt hints fall back to directory/page scans. No coordinate can exclude a
correct result.

## 5. Exact `FieldIndexDefinitionV1`

`FieldIndexDefinitionV1` uses the approved 32-byte `AFIX` envelope. Let `H` be
the database hash width, `N` the strategy-name length, and `C` the complete
embedded ConverterDefinition length:

| Definition offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 32 | `H` | `value_store_id` | Nonzero exact ValueStoreDefinition ID |
| `32 + H` | 2 | `strategy_id` | Permanent nonzero ID |
| `34 + H` | 2 | `strategy_semantics` | `1` corrected or named v0 adapter |
| `36 + H` | 2 | `posting_value_codec` | `1`, canonical posting key v1 |
| `38 + H` | 2 | `coordinate_codec` | `1`, fixed-point u64 above |
| `40 + H` | 2 | `comparison_semantics` | Known nonzero ID |
| `42 + H` | 8 | `supported_operation_bits` | Exact registry mask |
| `50 + H` | 2 | `normalization_semantics` | Zero when inapplicable |
| `52 + H` | 2 | `collation_semantics` | Zero when inapplicable |
| `54 + H` | 2 | `tokenizer_semantics` | Zero when inapplicable |
| `56 + H` | 2 | `expansion_semantics` | Known nonzero ID |
| `58 + H` | 2 | `expanded_duplicate_semantics` | `1`, first occurrence per source value |
| `60 + H` | 2 | `value_ordinal_semantics` | `1`, preserve source ordinals |
| `62 + H` | 2 | `recheck_semantics` | Known nonzero ID |
| `64 + H` | 2 | `scoring_semantics` | Zero when unscored |
| `66 + H` | 2 | `unindexable_semantics` | `1`, whole document for this index |
| `68 + H` | 4 | `converter_definition_length` | `C`; 120..64 KiB |
| `72 + H` | 2 | `strategy_name_length` | `N`; 1..256 |
| `74 + H` | 2 | reserved | Zero |
| `76 + H` | 4 | `max_expanded_tokens_per_document` | Nonzero, finite |
| `80 + H` | 4 | `max_postings_per_document` | Nonzero, finite |
| `84 + H` | 4 | reserved | Zero |
| `88 + H` | 8 | `max_canonical_posting_bytes_per_document` | Nonzero, finite |
| `96 + H` | 8 | `max_query_recheck_value_bytes` | Nonzero, finite |
| `104 + H` | 32 | `strategy_semantics_fingerprint` | Exact behavioral bundle digest |
| `136 + H` | `N` | `strategy_name` | Exact registry name, UTF-8 |
| `136 + H + N` | `C` | `converter_definition` | Complete canonical ACNV bytes |

The complete length is exactly `136 + H + N + C` and remains at most 256 KiB.
The registry validates ID/name/operation/converter combinations; a caller
cannot author a misleading name, operation bit, or recheck mode. The embedded
converter fingerprint is recomputed. `IndexId` remains the approved hash of
the complete AFIX bytes.

## 6. Permanent Strategy and Operation Registries

Permanent corrected strategy IDs are:

| ID | Name | Required converter | Supported operations |
| ---: | --- | --- | --- |
| `0x0001` | `exact` | `typed_exact_blake3_v1` | `eq`, `in` |
| `0x0002` | `ordered` | bytes/UTF-8/u64/i64/f64/timestamp/bool ordered | `eq`, `in`, `gt`, `lt`, `between`, sort, aggregate |
| `0x0003` | `trigram` | `unicode_trigram_v1` | `contains`, `similar`, `fuzzy`, `match` |
| `0x0004` | `soundex` | `soundex_ascii_v1` | `phonetic`, `match` |
| `0x0005` | `dmetaphone` | primary converter | `phonetic`, `match` |
| `0x0006` | `dmetaphone_alt` | alternate converter | `phonetic`, `match` |

Operation bits are permanent:

~~~text
0 eq       1 in       2 gt       3 lt       4 between
5 contains 6 similar  7 phonetic 8 fuzzy    9 match
10 sort    11 aggregate
~~~

Bits 12..63 are zero in v1. `gt` and `lt` are exclusive. `between` is inclusive
at both ends and rejects a lower endpoint greater than the upper endpoint.
`in` preserves no result multiplicity: it canonicalizes, sorts, and deduplicates
query literals before lookup. A document may retain duplicate source-value
postings, but query results deduplicate by document ordinal before boolean
composition.

There is no persistent `fuzzy` index strategy. Fuzzy is a query/recheck policy
that uses a trigram index for candidates when complete coverage is available.
Likewise `match` is a planner operation over exact/trigram/phonetic strategies,
not another duplicated posting family.

## 7. Text, Trigram, Fuzzy, and Phonetic Semantics

`AeorTextFoldV1` applies the frozen Unicode lowercase mapping to Unicode scalar
values and performs no NFC/NFD/NFKC normalization. Exact and ordered UTF-8
strategies do not apply it. The Unicode data version and every edge case are
part of the behavior bundle, not inherited from whichever standard library is
installed.

`unicode_trigram_v1` emits two token classes:

1. `0x01 word`: lowercase; non-alphanumeric scalar values are boundaries;
   each nonempty word receives two leading U+0020 and one trailing U+0020;
   each three-scalar window is one UTF-8 token.
2. `0x02 substring`: lowercase complete source string, no padding or boundary
   removal; each three-scalar window is one UTF-8 token.

Tokens are deduplicated within one source-value ordinal in first-occurrence
order. Duplicate source values retain distinct value ordinals. Empty text emits
no token. A one- or two-scalar `contains`/fuzzy query cannot obtain a complete
candidate set from trigrams and therefore uses bounded ValueStore recheck or a
typed query-budget error; it never returns a false negative.

Corrected query semantics are:

- `contains`: substring over `AeorTextFoldV1` scalar sequences; score `1.0`.
- `similar`: Dice coefficient over unique word-trigram sets. Both empty sets
  score `1.0` only when the folded strings are exactly equal; otherwise zero.
  Threshold must be finite in `[0,1]`; default `0.3`.
- `fuzzy/damerau_levenshtein`: Optimal String Alignment distance over folded
  Unicode scalar values. `auto` is 0 edits for length 0..2, 1 for 3..5, and 2
  otherwise. Explicit edits are 0..8. Score is
  `1 - distance/max(codepoint lengths,1)`.
- `fuzzy/jaro_winkler`: the v1 conformance algorithm uses a four-scalar prefix
  cap and scaling factor 0.1. `auto` threshold is 0.8; an explicit edit value
  maps to `max(0, 1 - edits/max(query_codepoints,1))`.
- `phonetic`: succeeds when any query/source token has the same nonempty code
  in at least one requested available phonetic strategy.
- `match`: exact folded equality, contains, trigram similarity `>= 0.3`,
  phonetic equality, and auto OSA are evaluated against canonical ValueStore
  text. Score is the maximum component score; `matched_by` is the sorted,
  deduplicated permanent strategy-name list.

Soundex and Aeor Double Metaphone v1 are the existing Aeor algorithms frozen
as conformance behavior, not claimed to be complete third-party reference
implementations. Tokenization uses Unicode alphanumeric runs; each phonetic
algorithm then retains ASCII letters according to its spec. Corrected alternate
Metaphone emits only a distinct alternate code. The v0 alternate adapter keeps
the current primary fallback.

Every candidate path performs exact recheck against the query-pinned immutable
ValueStore generation. Raw values do not live in an unbounded FieldIndex
`HashMap`. A stale/missing candidate index may cause a bounded value scan or an
explicit unavailable/budget result, never a false match or omission.

## 8. Numeric, Temporal, Boolean, Null, Missing, and Multi-Value Rules

- u64 accepts canonical u64 and nonnegative i64. i64 accepts canonical i64 and
  u64 at most `i64::MAX`. Other coercions are invalid.
- f64 accepts finite canonical f64 and only integers that round-trip exactly
  through f64. It rejects NaN, infinities, overflow, and precision-losing
  coercion. Signed zero canonicalizes as described above.
- timestamp accepts an in-range integer millisecond value or strict RFC 3339
  text with an explicit `Z`/numeric offset. It rejects naive dates/times,
  numeric strings, leap-second ambiguity outside the frozen parser contract,
  and failed parses. Output is checked UTC Unix milliseconds.
- bool accepts only canonical false/true.
- typed null is a present source value and can participate in `exact` equality.
  It is not ordered and is excluded from numeric/text/token strategies.
- missing emits no ValueStore value and no posting. A query for missing/null is
  not inferred from absent postings; it uses the exact ValueStore/scope state.
- unsupported type/coercion, nonfinite numeric input, or a semantic bound is a
  deterministic strategy-specific DocumentState under the approved
  all-or-nothing policy. Operational admission/OOM/cancellation is retryable
  and never becomes durable unindexable state.

Each source value keeps its zero-based ordinal. Exact/ordered strategies emit
one posting per accepted ordinal. Expanding strategies preserve the source
ordinal and assign zero-based expansion ordinals after within-value token
deduplication. Duplicate source values and equal tokens from different source
ordinals remain distinct postings. Query output remains one document unless an
explicit future API asks for value-level results.

## 9. Definition-Aware Query Literal Compilation

HTTP JSON is parsed into a bounded raw query AST. It is not converted to bytes
at route parsing time. `SemanticReadViewResolver` selects the root; the query
compiler resolves the exact ValueStore and FieldIndex definitions and compiles
each literal through their canonical source/converter contracts.

This replaces `json_query_value_to_bytes` as an authoritative path. The old
function survives only inside the v0 adapter and characterization tests.
Embedded SDKs use the same typed compiler.

Special metadata literal rules are part of the ValueStore definition:

- `@hash` accepts exactly `2H` lowercase or uppercase hexadecimal characters
  at the API and compiles to exactly `H` raw bytes; malformed width/text is an
  input error;
- timestamps accept the strict forms above;
- `@content_type = null` addresses absent content type under corrected typed
  semantics; and
- `@file_name` canonicalizes to `@filename` before definition lookup.

The compiler rejects an operation unsupported by the selected exact strategy
closure. It may combine complete strategies for `match`/`phonetic`; it cannot
silently substitute a current definition, a differently fingerprinted index,
or a scan with different coercion semantics.

Protocol hard bounds are:

~~~text
where nesting depth                 32
boolean/field AST nodes             1,024
IN literals                         4,096
one raw literal                     1 MiB
all raw literal bytes               8 MiB
sort fields                         32
group/aggregate fields              32 each
explicit fuzzy edits                8
similarity threshold                finite [0,1]
default returned documents          20
maximum returned documents/request  1,000
~~~

These are request-admission limits, not IndexId inputs. Changing them may
require a protocol capability but cannot alter the meaning of an already
accepted literal. Query execution work/memory budgets are operational Round 14
controls; budget exhaustion returns a typed incomplete/unavailable result and
does not silently truncate correctness.

## 10. Corrected Default Profile and Semantic Bounds

New v4 databases write `$v: 1` index configuration and compile these defaults:

| Field | Corrected strategies |
| --- | --- |
| `@path` | UTF-8 ordered exact/range plus trigram |
| `@filename` | UTF-8 ordered, trigram, soundex, primary and alternate Metaphone |
| `@extension` | UTF-8 ordered |
| `@hash` | typed exact over raw hash bytes |
| `@created_at`, `@updated_at` | timestamp ordered |
| `@size` | u64 ordered |
| `@content_type` | typed exact |

This preserves the approved decision to keep `@path` search behavior and to
avoid trigram indexes for `@hash` and `@content_type`.

Corrected definitions materialize these defaults unless a `$v: 1` config
chooses a smaller value:

~~~text
ValueStore max source values/document             1,024
ValueStore max canonical source bytes/document    8 MiB
ValueStore max document input                      64 MiB
JSON selector work items                           1,000,000
JSON selector examined bytes                       64 MiB
converter max one source value                     1 MiB
converter max one posting key                      1 MiB
converter max expanded keys/source value           65,536
converter max expanded key bytes/source value      4 MiB
FieldIndex max expanded tokens/document            65,536
FieldIndex max postings/document                   65,536
FieldIndex max posting bytes/document              8 MiB
query recheck value bytes/document                 8 MiB
WASM linear memory                                 64 MiB
WASM request bytes                                 64 MiB
WASM result bytes                                  16 MiB
WASM fuel                                          10,000,000 frozen-meter units
~~~

The public semantic-config compiler allows finite values from 1 through these
hard maxima unless a narrower per-field minimum applies. `max_document_input`
may be raised to 1 GiB for an explicit corrected definition; parser request,
result, linear memory, and canonical outputs retain their own maxima. Values
larger than the corrected hard maxima require a future semantics/capability ID,
not an unchecked allocation. The migration-only `u64::MAX` exceptions approved
in Round 8 remain adapter-only and are always constrained by operational
admission.

## 11. Built-In Fingerprints and Reproducible Conformance

Each corrected converter and strategy has one checked-in canonical bundle:

~~~text
semantics/v1/<permanent-name>/
  SPEC.md                    canonical LF UTF-8 normative text
  vectors.bin                hand-authored canonical input/output vectors
  invalid.bin                malformed/rejected vectors
  properties.json           named property/oracle declarations
~~~

The 32-byte fingerprint is:

~~~text
BLAKE3("aeordb.builtin-semantics.v1\0" ||
       permanent_id_u16_le ||
       canonical length-prefixed bytes of the four files in name order)
~~~

P0 creates those bundles independently of the production converter writer,
records each expected digest in the permanent registry, and prohibits
automatic regeneration in normal tests. Production code, the independent
reference evaluator, and every platform must reproduce the vectors and
properties. A code/library/Unicode upgrade may retain the ID only if it passes
the exact old corpus; any behavior change requires a new semantic ID and
fingerprint. Source-code hashes, compiler versions, and crate versions alone
are not behavioral authority.

The same process freezes v0 adapter behavior from captured production fixtures.
An adapter bug fix that changes an observable result receives another adapter
ID; it is not rewritten in place.

## 12. Migration, Failure, and Capability Policy

- Existing `$v: 0` `indexes.json` compiles to explicit v0 ValueStore,
  converter, and strategy adapters. Its current effective behavior is shadow-
  built and compared against captured v0 results.
- New v4 bootstraps and newly authored `$v: 1` configs use corrected IDs.
- Merely reopening, reindexing, or rewriting physical pages cannot change a
  definition ID. An administrator explicitly upgrades semantic config to move
  a v0 lineage to corrected v1 behavior.
- The migration root records the exact legacy/corrected definitions. A mixed
  generation is never advertised.
- Unsupported converter/strategy capability prevents build/fallback and marks
  the affected semantic query unavailable. It does not block ordinary file
  reads or consume a vaguely similar index.
- Malformed definition bytes, invalid combinations, fingerprint mismatch,
  oversize values, trailing bytes, and unknown required bits fail before
  allocation and preserve the previous active generation.
- NVT failure discards only the hint. Posting/value/definition corruption
  invalidates that derived closure, opens a rebuild/repair ticket, and invokes
  authoritative fallback under the exact same semantic definition.

## 13. AGIS Proof Matrix and Parent-Plan Corrections

Before a writer is enabled, tests must cover:

1. hand-authored ACNV/AFIX 32- and 64-byte-hash golden fixtures on Linux,
   macOS, and Windows, including every malformed length/reserve/ID pair;
2. independent evaluator agreement for every canonical type, integer edge,
   signed zero, finite float boundary, NaN/infinity rejection, timestamp offset,
   Unicode expansion, punctuation, empty input, duplicate, and multi-value;
3. property tests that posting order is total, coordinate order is monotone
   where promised, collisions cannot alter results, and every range widens/
   rechecks safely;
4. exact current-production capture for every v0 converter and fuzzy/phonetic
   query, including malformed inputs and current fallbacks;
5. definition mutation tests proving every semantic field/limit/fingerprint
   changes the correct ID while page/NVT/cache changes do not;
6. query-compiler differential tests across HTTP and embedded APIs, rejecting
   every coercion/operation ambiguity and proving `@hash` raw-byte identity;
7. bounded adversarial Unicode, token explosion, duplicate source values,
   huge `in`, deep boolean AST, and recheck-budget tests without partial
   publication or partial results;
8. missing/stale/corrupt NVT and index closures against an authoritative
   reference scan, with zero false positives after recheck and zero false
   negatives;
9. mixed old/new dependency and capability negotiation, restart, cancellation,
   and shadow activation; and
10. a real `/tmp/codex` database containing metadata, JSON, Unicode text,
    timestamps, duplicate arrays, large values, and v0/v1 definitions, queried
    through the public API before and after restart.

Parent and child plans must now:

- replace persisted `f64` scalar authority with `NormalizedCoordinateV1`;
- replace route-level generic byte conversion with the definition-aware query
  compiler;
- remove unbounded raw-value maps from FieldIndex correctness state;
- describe fuzzy as candidate plus immutable ValueStore recheck, not an exact
  scalar lookup;
- preserve `$v: 0` behavior through explicit adapters and use `$v: 1` for
  corrected semantics; and
- carry the exact registry/bundle fixtures into P0/P1 capability gates.

### Round 11 verdict

Round 11 is closed. Given a ValueStore definition and a FieldIndex definition,
an independent implementation can produce the same typed values, posting keys,
total order, fixed-point coordinates, candidates, recheck result, scores, and
semantic IDs without consulting process locale, mutable config, or current
library behavior. Every known v0 discrepancy has an explicit migration-only
home.

Round 12 may define how these logical records are packed and traversed. It may
not make coordinates authoritative, collapse canonical values into hashes,
drop source/expansion ordinals, or reintroduce route-specific literal coercion.

# Round 12: Index Pages, Journals, and Public Position Tokens

**Status:** CLOSED by Planning Cap and AGIS self-decision. This round freezes
every remaining IndexArtifact v1 body. It preserves immutable copy-on-write
sharing and makes the ArtifactDirectory, rather than NVT or physical offsets,
the correctness-bearing traversal structure.

## 1. Common Ownership, Birth Generation, and Directory Roles

An immutable page/node envelope's `generation` is its **birth generation**.
Unchanged immutable artifacts may be shared by later manifests for the same
semantic owner. A directory descriptor records the child's birth generation,
and a consuming manifest requires:

~~~text
child owner == manifest owner
child birth generation <= manifest generation
descriptor birth generation == child envelope generation
descriptor ArtifactHash == fully verified child key
~~~

This clarifies the earlier phrase "belongs to a generation." Requiring every
unchanged page to be rewritten with every manifest generation would destroy
copy-on-write and is forbidden. A newly rewritten logical page may retain its
PageId while receiving a new birth generation and ArtifactHash.

Permanent owner classes are:

~~~text
0 invalid   1 ScopeId   2 ValueStoreId   3 IndexId
~~~

Permanent ArtifactDirectory roles are:

~~~text
0 invalid
1 scope_ordinal        2 scope_reverse
3 value                4 value_document_state
5 posting              6 index_document_state
7 nvt_tile
~~~

The role fixes the owner class, child kind, key codec, comparison, and whether
PageId zero is legal. A role/owner/child mismatch is corruption. Unknown roles
are unsupported and never guessed from key length.

Permanent directory/page key codecs are:

~~~text
1 document_ordinal_u64_v1
2 file_key_v1
3 value_position_v1          (document ordinal, source value ordinal)
4 posting_position_v1        (coordinate, posting key, document/value/expansion ordinals)
5 nvt_cell_u64_v1
~~~

Structural integers inside keys remain little-endian; role-aware comparators
decode them. Generic byte comparison is used only for FileKey and
converter-owned byte-comparable posting values.

## 2. Exact `ArtifactDirectoryNodeV1`

The identity is exactly:

| Identity offset | Size | Field |
| ---: | ---: | --- |
| 0 | `H` | semantic owner ID |
| `H` | 1 | owner class |
| `H + 1` | 1 | directory role |

Identity length is `H + 2`. The body is:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 2 | `level` | Zero leaf; positive internal; at most 15 |
| 2 | 2 | `key_codec` | Exact role registry value |
| 4 | 4 | `entry_count` | 1..65,536 |
| 8 | 4 | `node_flags` | Zero |
| 12 | 4 | reserved | Zero |
| 16 | 4 | `lower_fence_length` | `L`; nonzero, at most 1 MiB |
| 20 | 4 | `upper_fence_length` | `U`; nonzero, at most 1 MiB |
| 24 | 8 | `subtree_live_count` | Exact reachable live records |
| 32 | 8 | `subtree_tombstone_count` | Exact reachable tombstones |
| 40 | 8 | `subtree_page_count` | Exact leaf page descriptors |
| 48 | 8 | `subtree_logical_bytes` | Exact logical child bytes |
| 56 | 8 | `minimum_page_id` | Role-dependent; zero if role has none |
| 64 | 8 | `maximum_page_id` | Role-dependent; zero if role has none |
| 72 | 4 | `entries_length` | `E`; exact records below |
| 76 | 4 | reserved | Zero |
| 80 | `L` | `lower_fence` | Exact first reachable lower fence |
| `80 + L` | `U` | `upper_fence` | Exact last reachable upper fence |
| `80 + L + U` | `E` | entries | Consume body exactly |

Body length is exactly `80 + L + U + E`. A node is normally at most 64 KiB
and is absolutely capped at 4 MiB so one legal large key can receive a
dedicated node. There are no empty directory nodes; a manifest uses an absent
root for an empty collection.

Each leaf descriptor is:

| Size | Field |
| ---: | --- |
| 4 | lower-fence length `A` |
| 4 | upper-fence length `B` |
| 8 | logical PageId or role-defined zero |
| `H` | exact child ArtifactHash |
| 8 | child birth generation |
| 8 | live record count |
| 8 | tombstone count |
| 8 | logical bytes |
| 8 | physical WAL offset hint |
| 4 | physical total-length hint |
| 4 | reserved zero |
| 8 | physical write-sequence hint |
| `A` | lower fence |
| `B` | upper fence |

Its fixed length is `72 + H`. Physical fields are hints only. A reader verifies
the current KV locator still names the same ArtifactHash, offset, length, and
write sequence before coalescing physical reads; otherwise it performs normal
KV lookup and may heal the next directory generation.

Each internal descriptor is:

| Size | Field |
| ---: | --- |
| 4 | child lower-fence length `A` |
| 4 | child upper-fence length `B` |
| `H` | child directory ArtifactHash |
| 8 | child birth generation |
| 8 | subtree live count |
| 8 | subtree tombstone count |
| 8 | subtree page count |
| 8 | subtree logical bytes |
| 8 | minimum PageId |
| 8 | maximum PageId |
| 8 | physical WAL offset hint |
| 4 | physical total-length hint |
| 4 | reserved zero |
| 8 | physical write-sequence hint |
| `A` | lower fence |
| `B` | upper fence |

Its fixed length is `88 + H`.

Descriptors are strictly ordered by role comparator, nonoverlapping, and
contain no duplicate fence. Gaps are legal. Parent fences and aggregate counts
must equal their first/last/summed children. Internal child levels are exactly
one below the parent. Cycles, repeated child hashes within one path, depth over
16, arithmetic overflow, and inconsistent ranks invalidate the derived
closure. Rank descent uses subtree live counts and therefore supports deep
offset/page access without materializing prior results.

## 3. Common Ordered Page Prefix and Identities

Posting, Value, ScopeCatalog, and DocumentState page bodies share this exact
96-byte prefix:

| Body offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | `page_flags` | Zero |
| 4 | 2 | `record_codec` | Exactly 1 |
| 6 | 2 | `key_codec` | Exact role value |
| 8 | 8 | `previous_page_id` | Posting only; otherwise zero |
| 16 | 8 | `next_page_id` | Posting only; otherwise zero |
| 24 | 4 | `lower_fence_length` | `L`; nonzero |
| 28 | 4 | `upper_fence_length` | `U`; nonzero |
| 32 | 4 | `record_count` | Nonzero; live + tombstone |
| 36 | 4 | `live_record_count` | Exact |
| 40 | 4 | `tombstone_count` | Exact |
| 44 | 4 | reserved | Zero |
| 48 | 8 | `records_length` | `R`; exact |
| 56 | 8 | `logical_live_bytes` | Exact live record bytes |
| 64 | 8 | `minimum_coordinate` | Posting only; otherwise zero |
| 72 | 8 | `maximum_coordinate` | Posting only; otherwise zero |
| 80 | 16 | reserved | Zero |
| 96 | `L` | lower fence | Exact first record key |
| `96 + L` | `U` | upper fence | Exact last record key |
| `96 + L + U` | `R` | records | Consume body exactly |

Body length is `96 + L + U + R`. Pages target 64 KiB, split above 96 KiB,
merge below 16 KiB when the combined page is at most 64 KiB, and are capped at
4 MiB. A single legal large record receives a dedicated page and may exceed the
normal split threshold. V1 page records are canonical uncompressed records;
prefix/dictionary compression requires another page codec.

Identities are:

~~~text
PostingPage:       IndexId      H || page_id u64 LE
ValuePage:         ValueStoreId H || page_id u64 LE
DocumentStatePage: owner ID     H || owner_class u8 || 7 zero || page_id u64 LE
Scope ordinal page: ScopeId H || role u8 || lower document ordinal u64 LE
Scope reverse page: ScopeId H || role u8 || lower FileKey H
~~~

Value/field PageIds are nonzero, lineage-local, retained across rewrites, and
allocated from their manifest high-water mark. Scope pages need no persistent
PageId or omitted manifest high-water: their canonical lower fence participates
in identity and their directory descriptor PageId is zero. Two rewritten scope
pages may share an identity but always have distinct content-derived artifact
keys; the selected directory hash is authoritative.

## 4. Exact Posting, Value, Scope, and State Records

### `PostingPageV1`

One record is:

| Relative offset | Size | Field |
| ---: | ---: | --- |
| 0 | 1 | flags: bit 0 tombstone; all others zero |
| 1 | 3 | reserved zero |
| 4 | 4 | posting-key length `K` |
| 8 | 8 | normalized coordinate |
| 16 | 8 | document ordinal |
| 24 | 4 | source value ordinal |
| 28 | 4 | expansion ordinal |
| 32 | `K` | complete canonical posting key |

Length is `32 + K`. Records use the Round 11 total order. Coordinate and full
key must recompute from the exact converter definition. No live duplicate
logical tuple is legal. A tombstone and live row for the same tuple cannot
coexist in one published page. Page min/max coordinates equal first/last.

The published posting directory and page chain are both exact. The first page
has previous zero, the last has next zero, and every adjacent pair agrees in
both directions. Split preserves the left PageId, allocates a new right PageId,
and rewrites the former successor only to correct its previous link. Merge
keeps the lower PageId, retires the upper PageId forever, and similarly rewrites
at most the successor. The directory remains the correctness path; links make
forward/reverse sequential scans cheap and are fully checked by verify/cutover.

### `ValuePageV1`

One record is:

| Size | Field |
| ---: | --- |
| 1 | flags: bit 0 tombstone |
| 3 | reserved zero |
| 4 | canonical value length `V` |
| 8 | document ordinal |
| 4 | source value ordinal |
| 4 | reserved zero |
| `H` | exact current RecordRevisionHash |
| `V` | complete CanonicalSourceValueV1; empty only for tombstone |

Fixed length is `24 + H`. The order is `(document ordinal, source value
ordinal)`. Live values validate against the exact ScopeCatalog descriptor and
ValueStore definition. A document's live ordinals are contiguous from zero and
preserve the source order; missing documents have no rows.

### `ScopeCatalogPageV1`

An ordinal-map record is:

~~~text
flags u8 (0 live, bit 0 tombstone)
reserved[3] = 0
path_length u32 LE
document_ordinal u64 LE
FileKey H
RecordRevisionHash H
canonical absolute path bytes
~~~

Fixed length is `16 + 2H`. Live rows require nonzero keys/revisions and a path
whose FileKey recomputes. Tombstones retain their last path/key/revision for
audit and ordinal non-reuse. Order is document ordinal.

A reverse-map record is exactly:

~~~text
flags u8 = 0
reserved[3] = 0
document_ordinal u64 LE
FileKey H
~~~

Length is `12 + H`; order is FileKey bytes. Reverse pages contain live rows
only. Full validation proves a one-to-one match between reverse rows and live
ordinal rows, the same exact coverage root, and no ordinal at/above the
manifest high-water.

### `DocumentStatePageV1`

One state record is:

| Size | Field |
| ---: | --- |
| 1 | flags: bit 0 tombstone |
| 1 | stage ID |
| 2 | stable reason ID |
| 4 | evidence length `E` |
| 8 | document ordinal |
| `H` | RecordRevisionHash |
| 8 | observed value/token count |
| 8 | observed canonical bytes |
| 8 | observed work/fuel units |
| 4 | dependency ordinal; zero if none |
| 4 | reserved zero |
| `E` | CanonicalConfigValueV1 evidence |

Fixed length is `48 + H`; `E` is at most 4 KiB. Evidence is stable structured
data, never an implementation error string, stack trace, path, clock, or host
resource reading.

Stage IDs are `1 parser`, `2 selector`, `3 mapper`, `4 canonical_value`,
`5 converter`, and `6 expansion`. Permanent reason IDs are:

~~~text
0x0000 invalid
0x0001 malformed_document
0x0002 deterministic_plugin_rejection
0x0003 parser_output_contract
0x0004 mapper_output_contract
0x0005 selector_work_limit
0x0006 selector_examined_bytes_limit
0x0007 source_value_count_limit
0x0008 source_value_bytes_limit
0x0009 unsupported_source_type
0x000A invalid_numeric_or_temporal_value
0x000B nonfinite_float
0x000C converter_input_limit
0x000D expansion_count_limit
0x000E posting_count_limit
0x000F posting_bytes_limit
~~~

ValueStore-owned pages may use stages 1..4 and applicable reasons; Index-owned
pages may use stages 5..6 and applicable reasons. Operational failures have no
durable state row. State order is document ordinal; at most one live state row
exists per document per owner generation.

## 5. Exact `NvtTileV1`

Identity is `IndexId H || tile_start_cell u64 LE`. The body is:

| Body offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | flags, zero |
| 4 | 2 | tile codec, `1` |
| 6 | 2 | entry codec, `1` |
| 8 | 8 | resolution |
| 16 | 8 | tile start cell; equals identity |
| 24 | 4 | tile cell count |
| 28 | 4 | populated entry count |
| 32 | 8 | basis posting generation |
| 40 | 8 | entries length; exactly count * 40 |
| 48 | 8 | approximate postings in tile |
| 56 | 8 | reserved zero |
| 64 | ... | sorted sparse cell entries |

Each 40-byte entry is:

~~~text
relative_cell u32 LE
flags u32 LE: bit 0 predecessor present, bit 1 successor present
predecessor_page_id u64 LE
successor_page_id u64 LE
approximate_live_postings u64 LE
sample_coordinate u64 LE
~~~

Relative cells are strictly increasing and below `tile_cell_count`.
Presence/zero fields agree. A tile with no entries is not persisted. The sample
coordinate must map to the named cell but is a diagnostic/hint only.

Lookup computes a cell, finds that exact sparse entry or scans backward within
the tile, then through predecessor nonempty tiles in the tile ArtifactDirectory.
If no predecessor exists it begins at the pinned posting manifest's first page.
Every hinted PageId is resolved through and range-checked against the pinned
posting directory. A stale/missing PageId falls back to directory predecessor
search using the complete posting position. A corrupt tile or entire NVT
closure is discarded without invalidating postings.

Resolution may grow or shrink and sparse cells may overwrite prior hints in a
new NVT generation. No tile points to a PostingPage ArtifactHash, so NVT never
pins a posting generation.

## 6. Exact `MutationJournalSegmentV1`

Identity is `journal_owner_id[16] || segment_ordinal u64 LE`; owner ID is a
TaskId or the fixed system index-journal stream ID named by Round 14. Envelope
generation is the nonzero task/stream generation. Body:

| Body offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | flags: bit 0 chain reset; others zero |
| 4 | 2 | journal codec, `1` |
| 6 | 2 | owner kind: 1 task, 2 system stream |
| 8 | 8 | segment ordinal; equals identity |
| 16 | 8 | first mutation sequence |
| 24 | 8 | last mutation sequence |
| 32 | 4 | record count |
| 36 | 4 | records length `R` |
| 40 | `H` | previous segment ArtifactHash or zero on reset |
| `40 + H` | `H` | source root before first record |
| `40 + 2H` | `H` | source root after last record |
| `40 + 3H` | `H` | semantic state root for this stream interval |
| `40 + 4H` | 16 | runtime boot ID |
| `56 + 4H` | `R` | mutation records |

Body length is `56 + 4H + R`. Segments target 1 MiB, contain at most 10,000
records, and are capped at 16 MiB. Records are strictly increasing by
`(mutation_sequence, mutation_id, batch_ordinal)`. Sequence gaps are legal;
reuse/reordering is not. First/last and source-root chain boundaries must agree
across linked segments.

One mutation record is:

| Size | Field |
| ---: | --- |
| 4 | complete record length |
| 2 | kind: 1 create, 2 replace, 3 delete, 4 move, 5 copy, 6 restore, 7 scope transition |
| 2 | presence: bit 0 before, bit 1 after |
| 8 | mutation sequence |
| 4 | batch ordinal |
| 4 | batch count |
| `H` | MutationId |
| `H` | namespace root before mutation/batch |
| `H` | namespace root after mutation/batch |
| `H` | before FileKey or zero |
| `H` | before RecordRevisionHash or zero |
| `H` | after FileKey or zero |
| `H` | after RecordRevisionHash or zero |
| 4 | before path length `A` |
| 4 | after path length `B` |
| 8 | committed-at Unix milliseconds |
| `A` | canonical before path |
| `B` | canonical after path |

Fixed length is `40 + 7H`. Presence bits require/clear the corresponding
path/key/revision. Move has both sides; copy records only the created
destination because its source remains unchanged. All records in one namespace
batch share MutationId/roots/batch count and cover each ordinal exactly once.

A checkpoint may compact an audited prefix and start a new chain-reset segment
whose source-before root equals its captured coverage root. The old chain can
retire only after the new checkpoint/control is durable. Missing/corrupt
journal data marks coverage reconciling/needs-rebuild; it never permits a
generation to claim current coverage.

## 7. Exact `IndexTaskCheckpointV1`

Identity is `TaskId[16] || checkpoint_sequence u64 LE`. Envelope generation is
the task generation, not the checkpoint sequence. Body:

| Body offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | checkpoint flags, zero |
| 4 | 2 | checkpoint codec, `1` |
| 6 | 2 | task kind |
| 8 | 2 | task state |
| 10 | 2 | phase ID |
| 12 | 32 | required capabilities |
| 44 | 8 | started-at Unix milliseconds |
| 52 | 8 | updated-at Unix milliseconds |
| 60 | `H` | captured source namespace root |
| `60 + H` | `H` | target namespace root or zero |
| `60 + 2H` | `H` | primary ScopeId/ValueStoreId/IndexId or zero |
| `60 + 3H` | `H` | journal head ArtifactHash or zero |
| `60 + 4H` | 8 | journal floor sequence |
| `68 + 4H` | 8 | journal audited-through sequence |
| `76 + 4H` | 8 | next/scanned document ordinal |
| `84 + 4H` | 8 | completed logical work |
| `92 + 4H` | 8 | total logical work hint |
| `100 + 4H` | 4 | resume-key length `K` |
| `104 + 4H` | 4 | attachment count `A` |
| `108 + 4H` | 4 | attachment bytes `B` |
| `112 + 4H` | 4 | external descriptor length `X` |
| `116 + 4H` | 4 | reserved zero |
| `120 + 4H` | `K` | role-specific logical resume key |
| `120 + 4H + K` | `B` | attachment records |
| `120 + 4H + K + B` | `X` | external run descriptor or absent |

The body length is exactly `120 + 4H + K + B + X`, capped at 4 MiB. `K` is at
most 1 MiB and `A` at most 4,096.

Task kinds are `1 scope_build`, `2 value_build`, `3 field_build`, `4 nvt_build`,
`5 reconcile`, `6 v0_migration`, `7 compaction`, and `8 index_repair`. States
are `1 running`, `2 cancel_requested`, `3 canceled`, `4 failed_retryable`,
`5 failed_terminal`, `6 complete_unpublished`, and `7 published`. Phase IDs are
task-kind-specific permanent registries; unknown phases are unsupported.

Each attachment record is:

~~~text
role u16 LE
flags u16 LE
owner_id H
artifact_hash H
artifact_birth_generation u64 LE
~~~

Length is `12 + 2H`. Records are sorted by `(role, owner_id, artifact_hash)`,
unique, and name every unpublished immutable artifact that must remain rooted
for resume. Role IDs cover partial directory roots, candidate manifests,
validation reports, and spill-run metadata; they never reinterpret arbitrary
hashes as GC edges.

The optional external descriptor is:

~~~text
workspace_id[16]
path_length u32 LE
manifest_digest[32] (BLAKE3)
durable_sequence u64 LE
durable_bytes u64 LE
canonical absolute UTF-8 path bytes
~~~

Length is `68 + path_length`, at most 64 KiB. It is node-local operational
resume state, not compatible-transfer authority. Missing/tampered external
state makes the checkpoint nonresumable and restarts from its captured source;
it cannot make partial output current.

Low-volume A/B task controls in Round 14 select one checkpoint. The checkpoint
itself never acts as a mutable task lease or active pointer.

## 8. Split, Merge, Compaction, and Validation Invariants

1. Page IDs and document ordinals never reuse or wrap. A rewrite keeps its
   logical ID; a split allocates a fresh right ID; a merge retires one ID.
2. Mutations build immutable pages, then COW directory nodes, then manifest,
   then the inactive active-pointer slot. Dependencies are verified first.
3. A published posting generation has exact directory fences, rank metadata,
   first/last IDs, and bidirectional page links. Only affected pages and the
   required neighbor are rewritten.
4. Tombstones remain part of the immutable generation until compaction. Merge/
   compaction may drop a tombstone only after its coverage/journal boundary
   proves no retained overlay or task requires it.
5. Directory nodes and pages may be structurally shared only under the same
   semantic owner and exact artifact hash. Cross-owner byte coincidence grants
   no sharing authority.
6. Shallow validation checks manifest plus immediate directory roots. Every
   consumed path/page is verified lazily. Full verify recomputes all fences,
   links, ranks, counts, owner/generation constraints, records, definitions,
   and artifact keys.
7. Missing/corrupt posting/value/scope/state pages invalidate only the affected
   derived closure and force exact fallback/rebuild. Missing/corrupt NVT state
   is simply discarded. No best-effort page skip can return a successful
   complete query.
8. Physical read coalescing is allowed only after every hinted contiguous span
   validates against current KV locators. Short reads, relocation, Void overlap,
   or write-sequence mismatch fall back to individual verified reads.

## 9. Exact Logical Position Token V1

The public token is canonical unpadded base64url of this binary record. It is
neither encrypted nor signed and contains no expiry, page, WAL, NVT, manifest,
or planner state:

| Token offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | magic `APOS` |
| 4 | 2 | schema version `1` |
| 6 | 2 | route kind |
| 8 | 4 | total decoded length |
| 12 | 2 | database hash-algorithm ID |
| 14 | 1 | sort component count; at most 32 |
| 15 | 1 | flags, zero |
| 16 | `H` | order fingerprint |
| `16 + H` | `H` | selected NamespaceRootV1 hash |
| `16 + 2H` | 4 | sort tuple length `T` |
| `20 + 2H` | `H` | immutable FileKey tie-breaker |
| `20 + 3H` | `H` | RecordRevisionHash tie-breaker |
| `20 + 4H` | `T` | canonical sort tuple |
| `20 + 4H + T` | 4 | CRC-32/ISO-HDLC over prior bytes |

Total decoded length is `24 + 4H + T`, at most 1 MiB. Route kinds are
`1 directory_listing`, `2 query`, `3 global_search`, and `4 aggregate_groups`.
Malformed base64, padded/noncanonical spelling, wrong algorithm/width, unknown
route, trailing bytes, count mismatch, oversize values, and CRC failure return
`INVALID_POSITION_CURSOR` before storage planning.

Each tuple component is:

~~~text
canonical value/comparator tag u16 LE
state u8: 0 present, 1 typed null, 2 missing
reserved u8 = 0
payload_length u32 LE
canonical payload bytes
~~~

Null/missing have zero tag/payload. Present values use their definition's exact
canonical little-endian/byte representation. The order fingerprint is:

~~~text
H("aeordb.position-order.v1\0" || CanonicalRouteOrderDefinitionV1)
~~~

That definition includes route kind, requested sort fields/directions,
directories-first policy, name fold/collation, null/missing policy,
multi-value selector, default/tie fields, score semantics, and every semantic
fingerprint involved. It excludes root, limit, page, offset, after/before,
include-total, and physical plan.

The server validates the token's root and order fingerprint, then resolves its
FileKey/revision in the selected authorized route result universe and
recomputes the sort tuple. Because tokens are unsigned, this validation is the
correctness boundary for client-edited tokens. A missing or mismatched position
returns `INVALID_POSITION_CURSOR`; a different root/order uses the already-
approved 409 codes.

`after` seeks strictly greater than the full tuple/tie. `before` seeks strictly
less, walks backward, takes the closest `limit`, and reverses that page back
into requested order. Neither operation scans from the first result merely to
locate the position.

## 10. Route Total Orders

Directory listing order is:

1. category: DirectoryIndex first; every non-directory second, never reversed;
2. requested primary field and direction;
3. canonical name/path ties; and
4. FileKey then RecordRevisionHash ascending.

Default name comparison uses the frozen Aeor listing lowercase behavior,
followed by raw UTF-8 name bytes so case-fold collisions are total. When name
is the primary, both folded and raw name follow requested direction. For size/
time primaries, folded/raw name and path ties are ascending. Thus descending
never moves folders below files.

Query order uses declared fields, defaulting to canonical path ascending. A
multi-value field selects its semantic minimum for ascending and maximum for
descending. For every direction, present non-null sorts before typed null,
which sorts before missing; only present values reverse. Canonical path,
FileKey, and revision ascending finish the total order.

Global fuzzy search defaults to score descending, then canonical path, FileKey,
and revision ascending. Scores must be finite corrected Round 11 values.
Aggregate groups order by requested group order or count descending, then the
complete canonical group tuple; their token tie is the hash of that tuple in
the FileKey slot and the exact aggregate input-root identity in the revision
slot.

Pagination is applied after these route orders. Directory/category, null,
multi-value, score, collation, or tie changes create a different order
fingerprint and cannot consume an older token.

## 11. AGIS Proof Matrix and Parent-Plan Corrections

Before a writer is enabled, tests must cover:

1. hand-authored fixtures for every node/page/tile/journal/checkpoint/token at
   both hash widths, with exact lengths, CRCs, keys, identities, and birth
   generations on Linux, macOS, and Windows;
2. independent ordered-map/reference-model agreement across random insert,
   replace, delete, split, merge, compaction, tombstone, and restart histories;
3. structural sharing across manifest generations without rewriting unchanged
   pages and without admitting wrong-owner/future-generation artifacts;
4. rank seeks, forward/reverse chain walks, empty/gapped directories, maximum
   keys, oversized dedicated pages, and every corrupt fence/link/rank/count;
5. NVT empty/stale/sparse/wrong-resolution/corrupt/missing-page behavior with
   identical answers and bounded fallback scans;
6. value/state/scope cross-validation, duplicate source values, zero-posting
   text, every stable state reason, and operational-failure non-persistence;
7. mutation batches, segment rollover/reset/compaction, missing journals,
   checkpoint A/B fallback, external-run loss/tampering, and crash at every
   artifact/control publication boundary;
8. every legal/illegal pagination matrix case, forged token, root/order/route
   mismatch, duplicate sort values, null/missing/multi-value behavior, and
   closest-page `before` semantics;
9. permanent directories-first ordering for ascending and descending, plus the
   original hundreds-of-files/folders pagination regression; and
10. a real `/tmp/codex` database queried/listed through HTTP and embedded APIs
    while writes, reindex, restart, NVT deletion, cache eviction, and page
    compaction occur under constrained memory.

Parent/child plans must now:

- treat artifact generation on nodes/pages as birth generation and permit
  verified same-owner structural sharing;
- use ArtifactDirectory rank/fence traversal as correctness and NVT/page links/
  physical spans only under their exact validation rules;
- remove whole-index `Vec`/`HashMap` loading from query, sort, aggregate, and
  pagination paths;
- replace JSON/base64 cursors with exact APOS v1 in a coordinated cutover;
- preserve route-specific total orders and directories-first descending; and
- root every unpublished attachment/journal/checkpoint through one typed task
  control before old state can retire.

### Round 12 verdict

Round 12 is closed. An implementation can seek, scan, reverse, split, merge,
compact, checkpoint, and paginate from immutable bounded artifacts without
loading a complete index. An empty or destroyed NVT changes only how quickly a
posting page is found. Every public position is logical, root-bound,
order-bound, authorized, and independently revalidated.

Round 13 may define physical retirement and reclamation evidence. It may not
treat current directory/KV reachability as proof that an older WAL incarnation
is dead, reclaim unpublished task attachments, or make a GC scratch structure
authoritative merely because it decodes.

# Round 13: Physical Retirement and Complete GC Formats

**Status:** CLOSED by Planning Cap and AGIS self-decision. This round freezes
the complete GcArtifact family, bounded external workspace, physical inventory,
quarantine, sweep, Void, audit, and crash transitions. Missing evidence always
leaks or delays space; it never accelerates reclamation.

## 1. Common `GcArtifactV1` Envelope, Keys, and Kinds

`EntryType::GcArtifact = 0x0A` and KV tag `0xB` remain approved. Every stored
value uses this exact 32-byte envelope, structurally parallel to AIDX:

| Offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `AGCA` |
| 4 | 2 | artifact schema version | `1` |
| 6 | 2 | artifact kind | Permanent registry ID |
| 8 | 2 | envelope header length | `32` |
| 10 | 2 | artifact flags | Zero |
| 12 | 4 | complete value length | Exact |
| 16 | 2 | identity length | Exact kind identity |
| 18 | 2 | reserved | Zero |
| 20 | 4 | body length | Exact |
| 24 | 8 | generation | Nonzero family/run generation |
| 32 | variable | identity | Kind-specific |
| after identity | variable | body | Kind-specific |
| final 4 | 4 | CRC-32/ISO-HDLC | All prior stored-value bytes |

The permanent kind registry is:

~~~text
0x0000 invalid

0x0001 QuarantineActiveControl       stable A/B
0x0002 MarkRunActiveControl          stable A/B
0x0003 PhysicalInventoryActiveControl stable A/B
0x0004 AuditCatalogActiveControl     stable A/B
0x0005 VoidCatalogActiveControl      stable A/B

0x0010 QuarantineManifest
0x0011 RootExpiryCatalogManifest
0x0012 PhysicalInventoryManifest
0x0013 MarkRunCheckpoint
0x0014 AuditCatalogManifest
0x0015 GcRunSummary
0x0016 VoidCatalogManifest
0x001F GcArtifactDirectoryNode

0x0020 CandidatePage
0x0021 CandidateDelta
0x0022 RootExpiryPage
0x0023 RetirementJournalSegment
0x0024 PhysicalInventoryPage
0x0025 MarkMutationJournalSegment
0x0026 VoidExtentPage
0x0027 VoidClaim

0x0030 SweepProposal
0x0031 SweepCommitReceipt
0x0032 RecoveredSweepReceipt
0x0033 CorruptGcEvidence
0x0034 AuditDetailPage
0x0035 AuditSummaryPage
0x0036 AuditPin
~~~

Unassigned IDs are reserved permanently. Immutable keys are:

~~~text
H("aeordb.gc-artifact.immutable.v1\0" || kind u16 LE || complete AGCA value)
~~~

Only the five active-control kinds have stable mutable keys:

~~~text
H("aeordb.gc-artifact.control.v1\0" ||
  kind u16 LE || database_id[16] || slot u8)
~~~

Outer entity compression/encryption are none in v1. The family-wide stored
value cap is 64 MiB, but tighter caps below apply. GC graph traversal follows
only typed ArtifactHash fields. Incarnation digests, namespace roots, policy
fingerprints, and evidence hashes are never accidentally treated as edges.

## 2. Exact A/B GC Controls

All five controls have identity `database_id[16] || slot u8`, where slot is
zero A or one B. Body:

~~~text
control_sequence u64 LE
target_manifest_hash H
~~~

Body length is `8 + H`; envelope generation equals the target manifest/run
generation. Target kinds are fixed by the registry:

~~~text
QuarantineActiveControl        -> QuarantineManifest
MarkRunActiveControl           -> MarkRunCheckpoint
PhysicalInventoryActiveControl -> PhysicalInventoryManifest
AuditCatalogActiveControl      -> AuditCatalogManifest
VoidCatalogActiveControl       -> VoidCatalogManifest
~~~

Selection/rewrite rules are the approved IndexArtifact A/B rules: count every
structurally valid sequence, choose the highest closure-valid target, reject
equal-sequence/different-target ambiguity, and overwrite invalid/lower state.
Quarantine, inventory, audit-retention, and Void authority use hard publication.
Mark checkpoints may use hard checkpoint publication without changing active
quarantine. A control never embeds predecessor history beyond its other slot.

## 3. Physical Incarnation Identity

Every reclamation-bearing record uses one exact `PhysicalIncarnationIdV1`:

| Relative offset | Size | Field |
| ---: | ---: | --- |
| 0 | `H` | logical KV/entity key |
| `H` | `H` | v1 integrity hash or legacy full-entity digest |
| `2H` | 8 | WAL offset |
| `2H + 8` | 8 | write sequence; zero only for v0 |
| `2H + 16` | 4 | total entity length |
| `2H + 20` | 1 | EntryType |
| `2H + 21` | 1 | entity version |
| `2H + 22` | 2 | reserved zero |

Length is `24 + 2H`. For v1, fields must match a fully verified EntityHeader,
key, bounds, and integrity hash. For v0, the second hash is:

~~~text
H("aeordb.legacy-physical-incarnation.v1\0" || exact complete v0 entity bytes)
~~~

Offset is never chronology. Identity equality requires every field. A logical
key may have multiple valid incarnations; only a currently verified KV locator
is active. Physical ranges must be nonzero, within the current WAL region, and
checked for overflow/overlap before use.

Permanent candidate classes are:

~~~text
1 unreachable_active_locator
2 retired_lower_incarnation
3 orphan_uncommitted_incarnation
4 expired_derived_artifact
5 expired_gc_audit_artifact
6 expired_namespace_root_closure
7 unexplained_gap_inventory_candidate
~~~

An unexplained gap is not a Void. It first enters physical inventory and normal
two-complete-mark quarantine.

## 4. GC Paged Directory and Page Framing

`GcArtifactDirectoryNode` identity is:

~~~text
database_id[16] || catalog_id[16] || directory_role u16 LE
~~~

Its body and leaf/internal descriptor bytes are exactly the Round 12
ArtifactDirectoryNodeV1 body, including 80-byte fixed body, fences, ranks,
birth generations, physical hints, depth 16, 4 MiB cap, and strict ordered
nonoverlap. The semantic owner fields are supplied by this AGCA identity
instead of the AIDX owner identity.

Permanent roles/key codecs are:

~~~text
1 candidates          PhysicalIncarnationId order
2 root_expiry         root hash bytes
3 physical_inventory WAL offset then complete incarnation ID
4 void_free_extents   WAL offset
5 void_claims         claim ID bytes
6 audit_details       occurred_at then event ID
7 audit_summaries     completed_at then run ID
~~~

Candidate, RootExpiry, PhysicalInventory, VoidExtent, AuditDetail, and
AuditSummary pages use this common 64-byte body prefix:

| Body offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | page flags, zero |
| 4 | 2 | record codec, `1` |
| 6 | 2 | key codec, role-defined |
| 8 | 4 | lower-fence length `L` |
| 12 | 4 | upper-fence length `U` |
| 16 | 4 | record count |
| 20 | 4 | live record count |
| 24 | 8 | records length `R` |
| 32 | 8 | logical live bytes |
| 40 | 24 | reserved zero |
| 64 | `L` | lower fence |
| `64 + L` | `U` | upper fence |
| `64 + L + U` | `R` | records |

Identity is `database_id[16] || catalog_id[16] || role u16 || page_id u64`.
Pages target 1 MiB, are capped at 16 MiB, and use nonzero never-reused page IDs
from the owning manifest. Directory roots are absent for empty catalogs.

## 5. Quarantine Candidate State, Pages, Deltas, and Manifest

A canonical candidate row is:

~~~text
PhysicalIncarnationIdV1       24 + 2H
candidate_class u16 LE
candidate_flags u16 LE = 0
pending_since_ms u64 LE
first_unreachable_generation u64 LE
grace_at_pending_ms u64 LE
~~~

Length is `52 + 2H`. These are the final three candidate-state fields already
approved. A row's presence in a complete active QuarantineManifest means it was
unreachable in that manifest's complete generation. Eligibility requires:

~~~text
active mark generation > first_unreachable_generation
now >= pending_since_ms + grace_at_pending_ms (checked/saturating boundary)
exact candidate identity still exists and is not current/reachable/pinned
all required policy and corruption checks pass
~~~

CandidatePage records are those rows, strictly ordered by complete physical
identity. No duplicate/overlapping incarnation is legal.

`CandidateDelta` identity is
`database_id[16] || mark_generation u64 || delta_ordinal u32`; body is:

~~~text
flags u32 = 0
record_codec u16 = 1
reserved u16 = 0
record_count u32
records_length u32
previous_delta_hash H or zero
sorted delta records
~~~

Each delta record is `operation u8` (`1 set`, `2 clear`), three zero bytes,
then one complete candidate row. Clear requires only the identity/class; its
three state fields are zero. Records are unique and sorted. Deltas apply in
listed order. At most 256 deltas and 64 MiB of delta bytes may separate a base
compaction; exceeding either threshold requires a new compacted base directory.

`QuarantineManifest` identity is
`database_id[16] || mark_generation u64`; envelope generation and body
mark-generation agree. Body:

| Body offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | flags, zero |
| 4 | 32 | required capabilities |
| 36 | 8 | complete mark generation |
| 44 | 8 | completed-at Unix milliseconds |
| 52 | `H` | captured authority-root-set digest |
| `52 + H` | `H` | captured semantic-state digest |
| `52 + 2H` | `H` | KV-layout fingerprint |
| `52 + 3H` | `H` | complete mark-result digest |
| `52 + 4H` | `H` | compacted candidate-directory root or zero |
| `52 + 5H` | `H` | exact RootExpiryCatalogManifest or zero |
| `52 + 6H` | 4 | delta count `D` |
| `56 + 6H` | 4 | reserved zero |
| `60 + 6H` | 8 | active candidate count |
| `68 + 6H` | 8 | candidate bytes |
| `76 + 6H` | 8 | currently eligible count hint |
| `84 + 6H` | 8 | currently eligible bytes hint |
| `92 + 6H` | 8 | next candidate page ID |
| `100 + 6H` | `D * H` | ordered CandidateDelta hashes |

Body length is `100 + 6H + D*H`, capped at 1 MiB. Eligible counts are
diagnostic snapshots; sweep recomputes eligibility. Base/deltas reconstruct one
exact sorted current candidate map. Candidates reachable again are cleared only
by a later complete manifest. Incomplete/canceled marks cannot emit or select a
QuarantineManifest.

## 6. Root Expiry Catalog

`RootExpiryCatalogManifest` identity is `database_id[16] || generation u64`.
Body:

~~~text
flags u32 = 0
required_capabilities[32]
retention_ms u64
byte_budget u64
directory_root H or zero
next_page_id u64
record_count u64
logical_bytes u64
oldest_expired_at_ms u64
newest_expired_at_ms u64
~~~

Body length is `92 + H`. Empty-root/count/time invariants are canonical.
One RootExpiryPage record is:

~~~text
namespace_root_hash H
expired_at_ms u64
last_pending_since_ms u64
reason u16 (1 reclaimed, 2 administratively retired)
reserved[6] = 0
reclaim_receipt_hash H
~~~

Length is `24 + 2H`; order is root hash. These records do not reference or pin
the reclaimed closure. Age/byte compaction produces a new manifest and old
catalog generations enter ordinary quarantine. An ancient absent hash after
catalog expiry returns `INVALID_NAMESPACE_ROOT`, as Round 10 approved.

## 7. Retirement Journal and Physical Inventory

`RetirementJournalSegment` identity is
`database_id[16] || segment_ordinal u64`. Body:

~~~text
flags u32 (bit 0 chain reset)
record_codec u16 = 1
reserved u16 = 0
first_replacement_sequence u64
last_replacement_sequence u64
record_count u32
records_length u32
previous_segment_hash H or zero
records
~~~

A record is:

~~~text
record_length u32
reason u16: 1 stable_key_replace, 2 relocation, 3 repair,
            4 migration, 5 pointer_or_control_replace
flags u16 = 0
replacement_publication_sequence u64
retired_at_ms u64
old PhysicalIncarnationIdV1
replacement PhysicalIncarnationIdV1
~~~

Fixed length is `24 + 2*(24 + 2H) = 72 + 4H`. Records are strictly ordered by
publication sequence and old identity. Segments target 1 MiB/cap 16 MiB.
Buffered retirement journaling may be soft because lost evidence only leaks
space; the durable audited-through watermark advances only after segment sync.
Dirty recovery scans every physical record after that watermark and
synthesizes missing lower-incarnation retirements before reclamation.

`PhysicalInventoryManifest` identity is
`database_id[16] || inventory_generation u64`. It records:

~~~text
flags u32 = 0
required_capabilities[32]
inventory_generation u64
completed_at_ms u64
kv_layout_fingerprint H
audited_wal_offset u64
audited_write_sequence u64
retirement_journal_through_sequence u64
directory_root H or zero
next_page_id u64
active_count u64
retired_count u64
orphan_count u64
quarantined_count u64
reclaimed_count u64
inventoried_bytes u64
~~~

Body length is `132 + 2H`. Counts/root/offsets are exact for the checkpoint.

One PhysicalInventoryPage row is:

~~~text
PhysicalIncarnationIdV1
state u8: 1 active, 2 retired, 3 orphan, 4 quarantined, 5 reclaimed
reason u8
flags u16 (bit 0 replacement present, bit 1 receipt present)
replacement PhysicalIncarnationIdV1 or all zero
discovered_at_ms u64
retirement_publication_sequence u64
reclaim_receipt_hash H or zero
~~~

Length is `68 + 5H`; order is WAL offset then complete identity. Active rows
must exactly match KV locators. Retired/orphan rows may become quarantine
candidates but never free space directly. Missing/corrupt inventory protects
the unknown range and degrades GC. Full inventory is checkpointed by external
sort and can resume without loading WAL identities into memory.

## 8. Mark Run Checkpoint and External Workspace

`MarkRunCheckpoint` identity is:

~~~text
database_id[16] || run_id[16] || checkpoint_sequence u64 LE
~~~

Envelope generation is the run generation. Body:

| Body offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | flags: bit 0 resumable, bit 1 canceled |
| 4 | 2 | checkpoint codec, `1` |
| 6 | 2 | state: 1 marking, 2 reconciling, 3 complete-unpublished, 4 canceled, 5 failed |
| 8 | 2 | phase: roots, bitmap, frontier, paths, inventory, candidates, reconcile, validate |
| 10 | 2 | reserved zero |
| 12 | 32 | required capabilities |
| 44 | 8 | started-at milliseconds |
| 52 | 8 | updated-at milliseconds |
| 60 | `H` | authority-root-set digest |
| `60 + H` | `H` | semantic-state digest |
| `60 + 2H` | `H` | KV-layout fingerprint |
| `60 + 3H` | 32 | effective-policy fingerprint |
| `92 + 3H` | 32 | SystemFamilyRegistry fingerprint |
| `124 + 3H` | 8 | captured DatabaseHeader slot sequence |
| `132 + 3H` | 8 | captured write-sequence high water |
| `140 + 3H` | 8 | reconciled-through write sequence |
| `148 + 3H` | 8 | active bitmap bit count |
| `156 + 3H` | 8 | KV bucket count |
| `164 + 3H` | 4 | KV slots per bucket |
| `168 + 3H` | 4 | workspace path length `P` |
| `172 + 3H` | 16 | workspace ID |
| `188 + 3H` | 32 | workspace-manifest BLAKE3 digest |
| `220 + 3H` | `H` | mark mutation-journal head or zero |
| `220 + 4H` | 8 | checkpoint logical work |
| `228 + 4H` | 8 | total logical work hint |
| `236 + 4H` | `P` | canonical workspace path |

Body length is `236 + 4H + P`, capped at 256 KiB. The path is node-local and
must match the configured/default private sibling root and embedded workspace
identity. An absent/mismatched workspace makes the run nonresumable.

The default durable workspace remains:

~~~text
<database-parent>/.<database-filename>-gc-<database-id>-<run-id>/
~~~

An override creates database/run subdirectories under the configured root.
Creation is private, no-follow, identity-bound, space-preflighted, and subject
to a free-space reserve.

### External workspace manifest

`manifest.agcw` is:

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | magic `AGCW` |
| 4 | 2 | schema `1` |
| 6 | 2 | state matching checkpoint |
| 8 | 8 | complete length |
| 16 | 16 | database ID |
| 32 | 16 | run ID |
| 48 | 8 | run generation |
| 56 | 8 | checkpoint sequence |
| 64 | 8 | created-at milliseconds |
| 72 | 8 | updated-at milliseconds |
| 80 | 2 | hash algorithm ID |
| 82 | 2 | object count `N` |
| 84 | 4 | flags, zero |
| 88 | `H` | KV-layout fingerprint |
| `88 + H` | `H` | authority-root-set digest |
| `88 + 2H` | 32 | effective-policy fingerprint |
| `120 + 2H` | variable | sorted object descriptors |
| final 4 | 4 | CRC-32/ISO-HDLC |

Each descriptor is:

~~~text
kind u16
flags u16
ordinal u64
stored_length u64
logical_record_count u64
BLAKE3_digest[32]
name_length u32
reserved u32 = 0
canonical relative name bytes
~~~

Fixed length is 68 bytes. Names are unique, slash-normalized relative paths
without `..`, NUL, symlinks, or alternate platform spellings. `N <= 65,535`.

Every object starts with:

~~~text
magic "AGWO"[4]
schema u16 = 1
kind u16
complete_length u64
database_id[16]
run_id[16]
run_generation u64
checkpoint_sequence u64
object_ordinal u64
body_length u64
body bytes
CRC32 u32
~~~

The fixed header is 80 bytes. Object kinds are `1 bitmap_segment`,
`2 frontier_run`, `3 path_visit_run`, `4 mutation_run`, `5 candidate_run`, and
`6 diagnostic_run`. The manifest BLAKE3 digest covers the complete object,
including CRC. Records are length-prefixed, strictly sorted by kind-specific
key, bounded at 1 MiB each, and objects are capped by configured scratch budget.

Bitmap segment bodies declare start bit, logical bit count, byte count, and raw
bytes with unused high bits zero. Frontier/path/candidate runs use exact typed
hash/path/incarnation records. Mutation runs use the writer-publication record
below. Diagnostic runs are nonauthoritative bounded evidence.

Checkpoint publication writes/syncs new objects, writes/syncs and atomically
renames the workspace manifest with parent-directory sync, verifies it, writes
the immutable in-DB checkpoint, then hard-publishes the inactive MarkRun control.
The previous checkpoint/workspace objects remain until the new closure verifies.

## 9. Mark Mutation Journal and Completion Boundary

`MarkMutationJournalSegment` identity is
`database_id[16] || run_id[16] || segment_ordinal u64`. Body framing matches the
retirement segment but records every successful writer publication observed
after root/layout capture:

~~~text
publication_sequence u64
MutationId H
root_before H
root_after H
published logical key H
new PhysicalIncarnationIdV1
operation u16
reserved u16 = 0
~~~

Length is `12 + 4H + (24 + 2H) = 36 + 6H`. Records are strictly ordered;
segment gaps are reconciled from the durable global sequence/inventory rather
than assumed empty.

The dense active-logical-locator bitmap maps the captured immutable KV layout's
`(bucket, slot)` positions only. It is never interpreted as all physical WAL
incarnations. Frontier/path/candidate runs and mutation reconciliation remain
separate bounded structures.

A complete mark requires:

1. every authority root and registered protected family traversed under strict
   typed rules;
2. no unresolved authoritative branch, missing root, overflow, I/O ambiguity,
   or repair latch;
3. physical inventory reconciled through the captured WAL/layout watermark;
4. writer mutation journal caught up through a sequence observed under the
   final publication guard, with an empty subsequent drain;
5. external objects and in-DB checkpoint fully verified; and
6. deterministic candidate output validated against the previous active
   quarantine state.

Only then may the immutable QuarantineManifest and its inactive hard control be
published. Cancellation, scratch exhaustion, checkpoint loss, convergence
starvation, shutdown deadline, or any damaged authoritative B-tree leaves the
prior quarantine generation unchanged.

## 10. Sweep Proposal and Receipts

`SweepProposal` identity is `database_id[16] || batch_id[16]`. Body:

~~~text
flags u32 = 0
proposal_codec u16 = 1
reserved u16 = 0
created_at_ms u64
quarantine_manifest_hash H
mark_generation u64
candidate_count u32 (1..4,096)
records_length u32
proposal_digest H
candidate PhysicalIncarnationIdV1 records
~~~

Body length is `40 + 2H + count*(24 + 2H)`, capped at 16 MiB. The digest hashes
the sorted candidate records under domain `aeordb.sweep-proposal.v1`.

`SweepCommitReceipt` and `RecoveredSweepReceipt` use the same identity and:

~~~text
flags u32 (bit 0 recovered for recovered kind only)
receipt_codec u16 = 1
reserved u16 = 0
reclaim_committed_at_ms u64
proposal_hash H
void_catalog_manifest_hash H
mark_generation u64
outcome_count u32
outcomes_length u32
reclaimed_count u64
reclaimed_bytes u64
skipped_count u64
failed_count u64
outcome records
~~~

Fixed body is `64 + 2H`. An outcome record is:

~~~text
PhysicalIncarnationIdV1
outcome u16: 1 reclaimed, 2 skipped_reachable, 3 skipped_changed,
             4 skipped_pinned, 5 skipped_policy, 6 failed_io,
             7 failed_corrupt
stable_reason_detail u16
reserved u32 = 0
resulting_void_offset u64 or zero
resulting_void_length u32 or zero
reserved u32 = 0
~~~

Its length is `48 + 2H`. Offset/length are both nonzero only for `reclaimed`.
Outcomes exactly cover the proposal once and are sorted by proposal order.

Full-detail receipt/evidence artifacts are rooted by the audit catalog for the
approved seven-day/4 GiB/latest-two policy; summaries use 90 days. Receipt
timestamps are actual durable Void-catalog commit times, never mark times.

## 11. Receipt-Backed Void Catalog and Durable Claims

The current raw hot-tail `(offset, size)` Void snapshot is not sufficient
authority. It cannot identify what was reclaimed and dirty gap scanning can
mistake an unindexed lower incarnation for free space. V4 therefore makes the
receipt-backed Void catalog authoritative. Hot-tail data is only a cache of its
selected control/manifest and is rejected if it disagrees.

`VoidCatalogManifest` identity is `database_id[16] || generation u64`; body:

~~~text
flags u32 = 0
required_capabilities[32]
published_at_ms u64
free_extent_directory_root H or zero
outstanding_claim_directory_root H or zero
next_page_id u64
free_extent_count u64
free_bytes u64
outstanding_claim_count u64
claimed_bytes u64
previous_control_sequence u64
~~~

Body length is `92 + 2H`. Counts and roots are exact.

One VoidExtentPage row is:

~~~text
offset u64
length u32
flags u32 = 0
origin_sweep_proposal_hash H
origin_quarantine_manifest_hash H
reclaimed_incarnation_digest H
reclaim_commit_sequence u64
void_generation u64
~~~

Length is `32 + 3H`; order is offset; extents never overlap active KV/WAL
entities, database/KV/header regions, hot tail, or another extent. Split
remainders retain origin identity and must remain subsets of the original
proposal extent. The proposal and quarantine references are known before Void
publication and therefore do not create a receipt/catalog hash cycle. The
post-publication receipt is the sole authority for the actual durable commit
time and binds this exact Void catalog hash. V1 does not coalesce extents with
different origins; later coalescing requires a typed reconciliation receipt.

`VoidClaim` identity is `database_id[16] || claim_id[16]`; body:

~~~text
flags u32 = 0
claim_codec u16 = 1
state u16: 1 reserved, 2 settled, 3 abandoned
created_at_ms u64
requesting_boot_id[16]
requesting_task_or_batch_id[16]
source_void_manifest_hash H
extent_count u32 (1..4,096)
extents_length u32
sorted (offset u64, length u32, reserved u32,
        origin_sweep_proposal_hash H) records
~~~

Fixed body is `56 + H`; cap 16 MiB.

Before overwriting any Void byte, the allocator hard-publishes a new Void
catalog that removes the complete claimed extents from the free directory and
adds the immutable claim. Only then may it split/use them in memory. Normal
writes do not fsync per entity; they consume an already durable bounded claim.
Settling removes the claim after all used entities/KV locators are durable and
returns only proven unused remainders through a new catalog generation.

After crash, every outstanding claim is treated as **not free**. Recovery scans
its extents, recognizes fully verified new entities, and sends unexplained
remainders through physical inventory and ordinary quarantine. It never simply
puts the old extent back. Lost claim evidence leaks the range.

Dirty-startup gap scanning may report/inventory suspected gaps but may not
register them with the allocator. `VoidManager::replace_all`, hot-tail load,
and metrics derive reusable space only from one closure-valid active Void
catalog minus outstanding claims.

## 12. Exact Sweep Crash State Machine

For each bounded batch:

1. write, sync, verify, and root the immutable SweepProposal;
2. under the final namespace/locator/pin guard, re-resolve every candidate,
   grace, later complete mark, current locator, replacement, reachability,
   task/request pin, protected family, repair latch, and physical range;
3. durably publish required DeletionRecord/KV locator removals and retirement
   state, then sync; bytes are not reusable yet;
4. COW-build a VoidCatalog generation containing only successful exact extents,
   hard-publish its inactive control, and sync; this is the reclamation commit;
5. write the exact SweepCommitReceipt and hard-publish the Audit catalog; and
6. release guards and enqueue old controls/artifacts for ordinary quarantine.

Crash outcomes are deterministic:

- before step 3: proposal is retryable evidence; no state changed;
- after locator removal but before Void publication: space leaks until proposal
  reconciliation safely finishes or records a skip;
- after Void publication but before receipt: startup validates catalog,
  proposal, KV absence, incarnation/range identity, and writes a
  RecoveredSweepReceipt before another sweep;
- after receipt: idempotent completion; duplicate batch IDs cannot reclaim or
  count twice; and
- any ambiguity removes/protects the extent from allocator visibility until a
  fresh inventory/mark cycle, even if this leaks space.

The allocator's later claim protocol is a separate hard boundary. A receipt-
backed free extent cannot be overwritten directly.

## 13. Audit, Evidence, Pins, and Retention

`AuditCatalogManifest` identity is `database_id[16] || generation u64`. Body:

~~~text
flags u32 = 0
required_capabilities[32]
published_at_ms u64
detail_directory_root H or zero
summary_directory_root H or zero
next_detail_page_id u64
next_summary_page_id u64
detail_record_count u64
detail_logical_bytes u64
summary_record_count u64
summary_logical_bytes u64
oldest_detail_at_ms u64
newest_detail_at_ms u64
oldest_summary_at_ms u64
newest_summary_at_ms u64
detail_retention_cutoff_ms u64
summary_retention_cutoff_ms u64
pin_count u32 (<= 4,096)
pins_length u32
sorted active AuditPin ArtifactHashes
~~~

Fixed body is `148 + 2H`, plus `pin_count * H`, capped at 1 MiB. Empty-root,
count, byte, time, and next-page invariants are canonical. Pins are unique.

An AuditDetailPage record is:

~~~text
event_id H
event_kind u16
outcome u16
occurred_at_ms u64
run_id[16]
batch_id[16]
payload_length u32
reserved u32 = 0
CanonicalConfigValueV1 payload (<= 1 MiB)
~~~

The fixed record is `52 + H`, plus payload. Records are unique and sorted by
`(occurred_at_ms, event_id)`.

An AuditSummaryPage record is:

~~~text
run_id[16]
started_at_ms u64
completed_at_ms u64
run_kind u16
outcome u16
mark_generation u64
scanned_count u64
candidate_count u64
reclaimed_count u64
reclaimed_bytes u64
error/evidence digest H
~~~

Summary records are `76 + H` and sorted by `(completed_at_ms, run_id)`.

`GcRunSummary` uses the same summary record as its body and identity
`database_id[16] || run_id[16]`. Detail payload schemas are permanent per event
kind and reject unstable maps/strings.

`CorruptGcEvidence` identity is `database_id[16] || evidence_id[16]`; body is:

~~~text
detected_at_ms u64
stable_error_class u16
observed_entry_type u8 or zero
flags u8
observed_artifact_kind u16 or zero
reserved u16 = 0
physical_offset u64 or zero
physical_length u32 or zero
reserved u32 = 0
write_sequence u64 or zero
expected_hash H or zero
observed_hash H or zero
run_id[16] or zero
control_kind u16 or zero
reserved u16 = 0
control_identity_digest H or zero
context_length u32
evidence_count u16 (<= 64)
reserved u16 = 0
CanonicalConfigValueV1 redacted context
evidence ArtifactHashes
~~~

Fixed body is `68 + 3H`, plus context and `evidence_count * H`, capped at
1 MiB. Optional fields are zero exactly when their flag/class says absent.
Context uses permanent keys/enums and never contains secrets or unbounded raw
data. Corrupt evidence remains rooted for detail retention, then enters ordinary
quarantine.

`AuditPin` identity is `database_id[16] || pin_id[16]`; body is:

~~~text
created_at_ms u64
expires_at_ms u64 or zero
creator_identity_digest H
reason u16
flags u16 = 0
artifact_count u32 (1..4,096)
artifacts_length u32
reserved u32 = 0
sorted exact GcArtifact hashes
~~~

Fixed body is `32 + H`, plus `artifact_count * H`, capped at 1 MiB. A root-only
low-volume system pin record roots it; deleting that record unroots the
immutable pin. Pins never authorize reclaim and cannot target arbitrary user
data.

Retention compaction writes new detail/summary pages and AuditCatalog, publishes
the inactive hard control, then unroots old optional detail. Active controls,
quarantine, current mark/inventory/Void state, unresolved proposals, incomplete
receipt recovery, corrupt evidence, latest two runs, and pins are outside the
optional 4 GiB budget.

## 14. Corruption, Cancellation, Migration, and B-Tree Policy

- If both quarantine controls fail, destructive GC state is
  `needs_fresh_mark`; all candidates are protected and sweep is disabled.
- If both Void controls fail, all free space is unavailable; append-only writes
  may continue if durable storage permits. No gap/hot-tail fallback authorizes
  reuse.
- If mark/inventory controls fail, resume is abandoned and restarted; active
  quarantine/Void authority remains unchanged.
- Deterministic authoritative B-tree corruption emits/deduplicates the Round 14
  RepairTicket and path latch, continues only independent diagnostic branches,
  and makes the mark incomplete.
- Corrupt derived index ownership conservatively protects the complete owner
  generation while authoritative mark may continue.
- Explicit cancellation publishes no quarantine. A consistent canceled mark
  checkpoint may remain resumable; sweep cancellation stops before the next
  batch and reconciles any already published proposal/catalog.
- Scratch exhaustion and memory refusal lose time, not data. There is no silent
  durable-to-temporary downgrade.
- V3-to-v4 migration copies no GcArtifact, workspace, candidate/grace, Void,
  receipt, evidence, or audit state. Destination starts `never_marked` with an
  empty/non-authoritative allocator and appends until a fresh complete mark,
  later mark, and receipt-backed reclaim establish v4 free extents.
- A byte-for-byte v4 physical copy preserves GcArtifacts but treats missing
  external work as nonresumable. Logical backup/import does not transfer active
  GC/Void authority.

## 15. AGIS State-Machine and Proof Matrix

Before any destructive writer is enabled, proof must include:

1. hand-authored AGCA/control/manifest/page/delta/journal/checkpoint/workspace/
   proposal/receipt/Void/audit fixtures for both hash widths and all platforms;
2. a model checker that interrupts every step in mark completion, A/B
   publication, sweep, receipt recovery, Void claim, claim settlement,
   retirement flush, inventory checkpoint, and retention compaction;
3. the invariant that every modeled crash state either retains data or leaks
   space, with no transition authorizing earlier reuse;
4. duplicate logical keys, lower incarnations, Void reuse, sequence gaps,
   relocation, pointer/control replacement, repair, and journal-loss recovery;
5. dirty startup with fake gaps, damaged hot tail, missing lower revisions,
   stale Void cache, overlapping extents, torn claims, and corrupt controls;
6. exact two-complete-mark/frozen-grace/reachable-again behavior across restart,
   config changes, zero grace, request/task pins, and root expiry;
7. tiny memory/scratch limits, forced external runs, checkpoint/reboot resume,
   cancellation at every loop, mutation convergence/starvation, and no swap;
8. authoritative versus derived B-tree damage, path latches, repair, mandatory
   fresh mark, and protected owner-family behavior;
9. audit budget/age/minimum-run/pin/evidence retention and recovered receipts
   without double counting; and
10. a `/tmp/codex` real database plus a production-shaped copy under an 8 GiB
    hard ceiling, with rapid writes/replacements/restarts, two marks, sweep,
    crash recovery, Void claim/reuse, verify, and byte-for-byte post-reopen
    validation.

Architecture/lint gates prohibit:

- direct stable-key replacement outside LocatorReplacementCoordinator except a
  typed recovery proof path;
- direct VoidManager registration from gap scan, GC candidate, or raw hot-tail
  tuples;
- reclaim without active Quarantine and Void closure validation;
- warning-and-continue success for incomplete authoritative mark/backup; and
- a second CLI/HTTP/scheduled/embedded GC traversal implementation.

### Round 13 verdict

Round 13 is closed. Every physical byte considered for reuse is tied to one
verified incarnation, two complete mark generations, frozen grace, final
reachability/pin checks, a proposal, a receipt-backed active Void generation,
and a durable allocator claim. A crash or corrupt/missing artifact can abandon
work, disable reuse, or leak disk, but cannot convert uncertainty into free
space.

Round 14 may assign capability bits and low-volume control/config formats. It
may not weaken these publication barriers, infer destructive defaults from a
malformed stored policy, or let an incompatible binary write before capability
negotiation completes.

---

# Round 14: Controls, Capabilities, Configuration, and Cutover

**Status:** Self-answered, adversarially reviewed, and closed. These decisions
complete the preimplementation format and policy freeze. No production v4
writer may exist until its independent fixtures and crash oracles are green.

## 1. Evidence and Boundary

This round closes real gaps in the current implementation:

- Windows `sync_parent_dir` currently returns success without issuing a
  durability operation.
- lifecycle loading currently logs and substitutes defaults after parse
  failure, contrary to the approved per-property fail-closed resolver;
- cache, shutdown, spill, and index-buffer settings are read independently
  from environment variables, sometimes with invalid values silently ignored;
- task/index/repair/migration controls use unrelated JSON/system-record shapes;
  and
- there is no one capability admission gate before a writable open or peer
  activation.

Round 14 does not turn high-volume state into system JSON. Index pages remain
`IndexArtifact`; destructive GC state remains `GcArtifact`; namespace and
semantic objects retain their Round 10 codecs. The controls here are small,
strictly bounded coordination records stored through the existing root-only
system-record mechanism. One `ControlStore` owns their codec, A/B selection,
publication, verification, and recovery.

## 2. Common Low-Volume Control Framing

Every new control payload has its own permanent four-byte magic but shares this
checked prefix. Integers are little-endian:

| Offset | Size | Field | V1 rule |
| ---: | ---: | --- | --- |
| 0 | 4 | kind-specific magic | Registry below |
| 4 | 2 | schema version | Exactly `1` |
| 6 | 2 | header length | Exactly `32` |
| 8 | 4 | total length | `36 + body_length`, checked |
| 12 | 4 | flags | Kind-specific known bits only |
| 16 | 8 | control sequence | Nonzero, monotonically increasing per identity |
| 24 | 4 | body length | Exact, at most the kind cap |
| 28 | 4 | reserved | Zero |
| 32 | `B` | body | Exact canonical kind body |
| `32 + B` | 4 | CRC32 | CRC-32/ISO-HDLC over all prior bytes |

The canonical KV key is:

~~~text
H("aeordb.system-control-key.v1\0" ||
  control_kind u16 LE || identity_length u16 LE || identity || slot u8)
~~~

`identity_length <= 4,096`. Mutable controls use slots `0` and `1`; immutable
records use slot `2` and require `control_sequence = 1`. Any other slot is
invalid. An A/B reader validates both complete payloads and body identity,
selects the higher sequence, accepts equal byte-identical copies, and returns
`AMBIGUOUS_CONTROL` for equal-sequence disagreement. Updating a mutable control
writes/verifies the inactive slot and hard-publishes its locator before the old
slot can enter ordinary quarantine.

All bodies begin with `database_id[16]`, followed by the identity again when it
is not implied by a fixed singleton. Key/body disagreement is corruption.
Unknown kind, schema, flag, enum, reserved byte, duplicate row, noncanonical
order, trailing byte, arithmetic overflow, or cap violation is rejected before
allocation. A control's failure policy comes from its SystemFamily descriptor;
the common codec never changes a failure into absence.

Permanent v1 kinds are:

~~~text
0x0001 AIRG IndexRegistryControl       singleton
0x0002 AIOP IndexOperationControl      IndexId H || operation_id[16]
0x0003 AIDG IndexDegradedControl       IndexId H
0x0010 ALLG LifecycleLastKnownGood     singleton
0x0011 ALDG LifecycleDiagnostics       singleton
0x0012 ARLG RuntimeLastKnownGood       singleton
0x0013 ARDG RuntimeDiagnostics         singleton
0x0020 ARTK RepairTicket               ticket_id[16]
0x0021 APWL PathWriteLatch             canonical path digest H
0x0030 AMLE MigrationLease             migration_id[16]
0x0031 AMPR MigrationProgress          migration_id[16]
0x0032 ALRM LegacyRootMapControl       migration_id[16]
0x0033 ALRP LegacyRootMapPage          migration_id[16] || page_ordinal u64
0x0040 ATPN TaskPin                    task_id[16]
0x0041 ASMJ SemanticMutationSegment    segment_ordinal u64
0x0042 ARTX RootPublicationPrepare     transaction_id[16]
0x0043 ARAC RootAdmissionCommit        NamespaceRoot hash H
0x0050 ADLT DurabilityLatch            singleton
0x0051 ASPC EmergencySpillCatalog      singleton
0x0052 ACUT SideBySideCutoverControl   migration_id[16]
~~~

Unassigned values and all kind-specific enum values are permanently reserved;
IDs are never recycled.

## 3. Index Controls

### 3.1 Registry

`IndexRegistryControl` body is:

~~~text
database_id[16]
registry_generation u64
semantic_state_id H
source_namespace_root H
entry_count u32 (<= 65,535)
entries_length u32
previous_registry_digest H or zero
entries
~~~

Fixed length is `32 + 3H`. Entries are sorted uniquely by `IndexId`:

~~~text
IndexId H
index_kind u8: 1 field, 2 scope, 3 value-store, 4 NVT
state u8: 1 absent, 2 building, 3 active, 4 reconciling,
          5 degraded, 6 needs_rebuild, 7 retiring
flags u16: bit 0 exact-complete, bit 1 fallback-required
active_manifest H or zero
target_manifest H or zero
coverage_namespace_root H or zero
coverage_runtime_sequence u64
last_operation_id[16] or zero
stable_reason u16
reserved u16 = 0
~~~

Entry length is `32 + 4H`. Active manifests are immutable and fully validated
before registry publication. Registry replacement is the sole active-generation
switch; an operation/degraded control is detail and cannot activate an index.
The canonical registry digest participates in `SemanticStateV1` only through
the immutable index definitions, never through mutable coverage or task state.

### 3.2 Build and Reconciliation

One `IndexOperationControl` replaces separate route/task formats:

~~~text
database_id[16]
IndexId H
operation_id[16]
operation_kind u16: 1 build, 2 rebuild, 3 reconcile, 4 compact
state u16: 1 queued, 2 running, 3 checkpointed, 4 publishing,
           5 complete, 6 canceled, 7 failed
created_at_ms u64
updated_at_ms u64
requested_namespace_root H
definition_id H
base_manifest H or zero
target_manifest H or zero
checkpoint_artifact H or zero
captured_runtime_sequence u64
reconciled_through_sequence u64
completed_work u64
total_work_hint u64
stable_reason u16
retry_class u16
error_evidence_hash H or zero
~~~

Fixed body length is `88 + 7H`. The operation state machine is monotonic except
that a failed/canceled operation may be superseded by a new operation ID.
Checkpoint advancement follows Round 12's durable mutation boundary. No
generic task checkpoint string may stand in for this typed control.

### 3.3 Degraded State

`IndexDegradedControl` body is:

~~~text
database_id[16]
IndexId H
observed_registry_generation u64
detected_at_ms u64
stable_reason u16
fallback_mode u16: 1 authoritative_scan, 2 exact_partial_plus_scan,
                   3 unavailable
affected_manifest H or zero
evidence_hash H or zero
last_verified_namespace_root H or zero
retry_after_ms u64 or zero
~~~

Body length is `44 + 4H`. Registry state remains the authority. This control
deduplicates diagnostics and drives metrics/repair; deleting or corrupting it
cannot make a degraded index complete. Every HTTP, CLI, scheduled, and embedded
index operation calls the same `IndexCoordinator` and `ControlStore`.

## 4. Lifecycle, Runtime, Repair, Migration, and Pin Controls

### 4.1 Last-Known-Good and Diagnostics

`LifecycleLastKnownGood` and `RuntimeLastKnownGood` use one exact body shape:

~~~text
database_id[16]
config_kind u16: 1 lifecycle, 2 runtime
config_schema u16
activated_at_ms u64
source_namespace_root H
source_file_content_hash H
canonical_config_length u32
reserved u32 = 0
policy_fingerprint[32]
CanonicalConfigValueV1 bytes
~~~

Fixed length is `68 + 2H`, plus at most 1 MiB of canonical config. The body
stores recoverable canonical policy, not merely a digest. Publication order is
validated config bytes, immutable namespace root, LKG inactive slot, then the
normal authority/root activation in one coordinated hard boundary. LKG never
contains environment or CLI overrides. `source_namespace_root` is diagnostic
provenance, not a typed GC edge; the control pins its canonical policy bytes but
does not retain an otherwise dead namespace tree.

The corresponding diagnostics body is:

~~~text
database_id[16]
config_kind u16
aggregate_state u16: 1 valid, 2 degraded_override, 3 degraded_fallback,
                     4 partially_disabled, 5 fatal
observed_at_ms u64
current_file_root H or zero
current_file_content_hash H or zero
effective_policy_fingerprint[32]
source_row_count u16
disabled_capability_count u16
detail_length u32
CanonicalConfigValueV1 detail
~~~

Fixed length is `68 + 2H`; detail cap is 1 MiB. Detail keys and stable error/
source IDs are frozen canonical enums. It contains no secret values. Diagnostics
are observational: corrupt diagnostics are regenerated and never substitute
for LKG or effective policy.

### 4.2 Repair Tickets and Path Latches

`RepairTicket` body contains `database_id[16]`, ticket ID, created/updated time,
state (`open`, `repairing`, `resolved`, `superseded`), stable corruption class,
authority family ID, affected namespace root, canonical path digest, complete
`PhysicalIncarnationIdV1` or zero, up to 64 sorted evidence ArtifactHashes, and
a bounded canonical diagnostic payload. Maximum body is 1 MiB. Each variable
vector has an explicit `u32` byte length; its rows are count-checked before
allocation. The ticket ID and immutable evidence never change; state changes
use the ticket's A/B control sequence.

`PathWriteLatch` body is exact and small:

~~~text
database_id[16]
canonical_path_digest H
latched_at_ms u64
ticket_count u16 (1..64)
state u16: 1 write_blocked, 2 repair_exclusive
reserved u32 = 0
sorted ticket_id[16] rows
~~~

Fixed length is `32 + H`, plus rows. A latch is checked by the single namespace
mutation coordinator after authorization/path normalization and before any
append. Only a verified repair transition may clear it. Missing/corrupt latch
state for an open authoritative ticket remains write-blocked; it never falls
through to ordinary mutation.

### 4.3 Migration Lease, Progress, and Legacy Roots

`MigrationLease` body contains logical database ID, migration ID, source and
destination physical-instance IDs, holder boot ID, fencing token,
acquired/renewed/expires timestamps, source header sequence, and state. A format
migration requires both headers to carry the same logical database ID; the
instance IDs distinguish the two physical files during cutover. Lease
acquisition and renewal are A/B hard publications. A holder must re-read and
match the fencing token before every authority-changing phase; expiry never
permits two holders without a new larger token.

`MigrationProgress` body contains the same identities/fencing token plus exact
source/destination format versions, source/destination header sequences,
phase, copied/reconciled write-sequence watermarks, namespace/entity/byte
counters, current immutable checkpoint hash, root-map control hash, effective
config and SystemFamily fingerprints, last error evidence, and updated time.
All scalar fields have fixed widths; every optional hash is a fixed zero/nonzero
slot and the maximum body is 4 KiB.

`LegacyRootMapControl` body is:

~~~text
database_id[16]
migration_id[16]
logical_database_id[16]
source_physical_instance_id[16]
destination_physical_instance_id[16]
source_format u16 = 3
destination_format u16 = 4
flags u32 = 0
map_generation u64
page_count u32
record_count u32
first_page_hash H or zero
last_page_hash H or zero
complete_map_digest H
~~~

Body length is `104 + 3H`. A page body starts with those five IDs, page ordinal,
previous/next page hashes, row count/length, and sorted rows:

~~~text
legacy_root_hash H
namespace_root_v1_hash H
semantic_availability u16: 1 complete, 2 content_only
content_only_reason u16
captured_source_write_sequence u64
~~~

Rows are `12 + 2H`; pages are capped at 1 MiB. The control is published only
after the complete chain and digest validate. It is finite migration evidence,
not an on-read growing provenance map. Mapped NamespaceRoot hashes are lookup
targets, not GC roots: they follow ordinary live/retained/pending/reclaimed
lifecycle. A retained map hit whose target was reclaimed resolves through
bounded RootExpiry evidence and never resurrects or pins the old closure.

### 4.4 Durable Task Pins

`TaskPin` body is:

~~~text
database_id[16]
task_id[16]
task_kind u16
state u16: 1 active, 2 releasing, 3 released
created_at_ms u64
renewed_at_ms u64
expires_at_ms u64 or zero for engine-owned nonexpiring task
fencing_token u64
root_count u32 (<= 4,096)
artifact_count u32 (<= 4,096)
sorted root hashes
sorted typed ArtifactHashes
~~~

Fixed length is 76 bytes. Request pins remain in-memory scoped guards; task
pins are durable because work survives requests/restarts. A task must publish
its pin before releasing request admission, renew it while active, and clear it
only after its final authority/checkpoint transition. Expired pins are not
silently ignored: recovery validates owning task state before release.

## 5. Mutation Journal and First-Authority Root Admission

### 5.1 One Semantic/Control Mutation Stream

`SemanticMutationSegment` body is:

~~~text
database_id[16]
segment_ordinal u64
first_publication_sequence u64
last_publication_sequence u64
record_count u32
records_length u32
previous_segment_hash H or zero
records
~~~

Fixed length is `48 + H`. Records are ordered uniquely by publication sequence
and MutationId:

~~~text
publication_sequence u64
MutationId H
family_id u16
operation u16
flags u32
authority_before H or zero
authority_after H or zero
semantic_state_before H or zero
semantic_state_after H or zero
namespace_root_before H or zero
namespace_root_after H or zero
operation_id[16]
~~~

Record length is `32 + 7H`. It records every successful namespace, semantic,
index-registry, lifecycle, snapshot/fork, task-pin, migration, repair-latch, and
destructive-control publication. The stream is buffered and segment-synced; it
accelerates cache invalidation, index/GC reconciliation, SSE, and diagnostics,
but is not authority. A gap is recovered by comparing the globally durable
publication sequence and typed controls/roots. No consumer interprets a missing
record as proof that nothing changed.

`MutationId` is stable across retries and derived from operation ID, canonical
target identity, expected-before authority, and intended-after authority. The
single `NamespaceMutationCoordinator` allocates the publication sequence,
writes the authoritative state and mutation record, publishes them through one
durability batch, and fans out post-commit cache/SSE/index work. Routes, plugin
hosts, SDK helpers, sync, repair, migration, and maintenance cannot publish an
independent semantic/control mutation.

### 5.2 Prepared Root Transaction

`RootPublicationPrepare` is immutable and has body:

~~~text
database_id[16]
transaction_id[16]
created_at_ms u64
target_namespace_root H
target_semantic_state H
typed_closure_digest H
authority_kind u16: 1 HEAD, 2 snapshot, 3 fork, 4 sync-base,
                    5 migration-map
root_format u16 = 1
authority_identity_length u16
reserved u16 = 0
expected_authority_before H or zero
expected_authority_after H
intended_header_slot_sequence u64
intended_publication_sequence u64
authority_identity bytes
~~~

Fixed length is `64 + 5H`, plus at most 4,096 identity bytes. Dependencies,
semantic state, NamespaceRoot, and prepare record are append-synced and fully
verified before the authority transaction begins. A prepare record alone never
admits a root and may later enter ordinary quarantine.

### 5.3 Atomic Root Admission Commit

`RootAdmissionCommit` is immutable, keyed by the NamespaceRoot hash, and has:

~~~text
database_id[16]
namespace_root H
transaction_id[16]
publication_started_at_ms u64
authority_kind u16
root_format u16 = 1
flags u32: bit 0 recovered from selected authority
authority_identity_digest H
authority_after H
selected_header_slot_sequence u64
publication_sequence u64
prepare_payload_hash H
~~~

Body length is `64 + 4H`. The timestamp is diagnostic; the selected, synced
header/control generation is durability authority. On a root's first admission,
the coordinator places the authority update and commit record into the same
KV/header publication boundary. There is no selected state in which only one
is visible. HEAD selection, snapshot/fork creation, migration-map publication,
and sync-base adoption all use this primitive.

Admission rules are:

1. a currently selected, closure-valid typed authority admits its exact root;
2. a former/retained root requires its closure-valid admission commit;
3. a prepared root without either is not externally resolvable, even if dirty
   KV rebuild finds its bytes;
4. recovery may write a `flags.recovered` commit only from an already selected,
   fully verified authority whose header/control publication proves the root;
5. commit records enter the root's quarantine closure and are reclaimed with
   it; RootExpiry evidence then supplies the bounded post-reclaim response; and
6. duplicate roots share one commit keyed by root, while later authorities do
   not append redundant provenance.

This is not an unbounded per-read or per-HEAD history. It is one collectable
first-admission witness per distinct NamespaceRoot.

## 6. Durability Latch and Emergency Spill Catalog

`DurabilityLatch` body contains database ID, latch generation, first/latest
failure times, severity, read-only state, failed durability operation, stable OS
error class/code, last selected header sequence, last durable write/publication
sequences, emergency spill catalog digest or zero, evidence digest, and a
bounded redacted diagnostic. A serious durability failure latches the entire
database read-only before another write is admitted. Recovery requires explicit
verify/repair and a successful hard-publication probe; restart alone does not
clear it.

The serious classes are the previously approved inability to durably append or
sync committed bytes, ambiguous/torn authority selection, persistent media or
filesystem I/O error, read-only filesystem, no-space/quota condition that
prevents emergency flushing, write-sequence exhaustion/collision, or inability
to preserve required dirty state. Retryable interruption, temporary resource
admission, client cancellation, and an individual malformed request do not
latch the database.

`EmergencySpillCatalog` is written only during explicit recovery after external
spill discovery. It identifies the database, every validated spill artifact,
source location class, creation sequence/time, ordered replay status, and final
repair receipt. Startup still scans every approved user-data and `/tmp` fallback
location before writable admission. Discovery aborts startup with the exact
repair command; repair prompts in oldest-first order unless `--yes`. The primary
database control cannot be treated as proof that no external spill exists,
because the reason for spilling may have been failure of that database device.

## 7. DatabaseHeader Capability and Registry Freeze

Round 2's 512-byte DatabaseHeader v4 is corrected before any writer exists:
the former 156-byte reserve now contains the writer capability floor, selected
SystemFamily registry identity, and writer fence shown in the amended Round 2
table. Forty-four bytes remain reserved and zero. The header remains version 4;
this is completion of an unwritten format, not a migration of emitted v4 bytes.

The permanent capability assignments are:

| Bit | Capability | Reader obligation | Writer obligation |
| ---: | --- | --- | --- |
| 0 | `WholeEntityV1` | Decode/skip v1 framing and write sequence | Emit and preserve v1 framing |
| 1 | `NamespaceRootV1` | Resolve exact typed roots | Publish only through coordinator |
| 2 | `SemanticStateV1` | Decode exact semantic closure | Preserve/rebuild exact dependencies |
| 3 | `SystemFamilyRegistryV1` | Classify using selected fixture | Route every protected operation through it |
| 4 | `SystemControlV1` | Select/validate A/B controls | Use ControlStore hard publication |
| 5 | `RootAdmissionV1` | Enforce admission witness rules | Use first-authority transaction |
| 6 | `GlobalPublicationSequenceV1` | Interpret sequence coverage | Allocate/publish monotonic ranges |
| 7 | `IndexArtifactV1` | Decode or safely reject artifacts | Preserve typed derived generations |
| 8 | `IndexDefinitionV1` | Apply frozen definitions | Never reinterpret a definition ID |
| 9 | `IndexPagedStoreV1` | Traverse pages/directories | Publish immutable COW generations |
| 10 | `IndexCoverageV1` | Enforce coverage/fallback | Journal/reconcile every mutation |
| 11 | `SparseNvtV1` | Treat NVT only as a hint | Maintain disposable sparse hints |
| 12 | `GcArtifactV1` | Validate/protect GC family | Use typed GC artifacts only |
| 13 | `PhysicalInventoryV1` | Understand incarnations | Journal/reconcile replacement identity |
| 14 | `DurableMarkWorkspaceV1` | Validate mark checkpoints | Maintain bounded resumable marks |
| 15 | `QuarantineTwoMarkV1` | Protect candidate state | Require two complete marks/frozen grace |
| 16 | `ReceiptBackedVoidV1` | Exclude claims/invalid catalogs | Reuse only receipt-backed claimed extents |
| 17 | `RootExpiryV1` | Return bounded expiry state | Publish evidence only after receipt |
| 18 | `LifecycleConfigV1` | Resolve strict policy | Preserve per-property precedence/capture |
| 19 | `ConfigLastKnownGoodV1` | Validate fallback controls | Publish LKG before activation |
| 20 | `RepairPathLatchV1` | Honor read/write degradation | Fence affected mutations during repair |
| 21 | `SideBySideMigrationV1` | Read mapping/cutover evidence | Never upgrade v3 in place |
| 22 | `DurabilityLatchSpillV1` | Surface latch/spill state | Enter read-only and preserve dirty bytes |
| 23 | `DurableTaskPinV1` | Trace active task roots | Publish/release pins in typed order |

Bits 24 through 255 are reserved zero until permanently assigned. API-only
features such as APOS position tokens are negotiated at the API protocol and do
not consume a database capability unless they later persist bytes.

Admission is capability-based, never a semver comparison:

- a read-only opener must implement every set `required_reader_capabilities`
  bit and the selected header/entity/hash codecs;
- a writable opener must satisfy both stored bitsets and must have writer
  support for every required-reader bit;
- setting a newly required feature updates both A/B headers through a hard
  publication before dependent bytes/authority can become visible;
- bits are monotonic in ordinary operation; clearing requires an offline
  downgrade that proves no dependent bytes, controls, policies, or invariants
  remain; and
- an unknown set bit rejects before repair, migration, background workers,
  dirty rebuild publication, or ordinary writer admission. A diagnostic raw
  scanner may open without mutating.

Every v4 database begins with bits 0 through 6, 18, 19, 21, and 22 in both
bitsets. Feature bits 7 through 17, 20, and 23 are added before their first
activation. A migration destination may predeclare its final set, but the v3
source never receives v4 bits.

### 7.1 Canonical SystemFamily Fixture

The Round 10 matrix is the normative semantic source. P0 must expand it into:

~~~text
aeordb-lib/spec/fixtures/system-family-registry-v1.bin
aeordb-lib/spec/fixtures/system-family-registry-v1.manifest.json
~~~

The binary contains every descriptor required by each row/matcher under the
already-frozen `ASFR` codec and canonical ordering. The manifest records every
source row, expanded descriptor keys, exact length/hex/fingerprint, and the
independent fixture-builder revision. The final selected-width fingerprint is
copied into the plan fixture manifest and both v4 header slots before a writer
test can pass. This digest is a mechanical result of already-ratified bytes, not
an open policy choice.

A binary carries a catalog of every registry version it supports. Opening a
database selects the exact version/fingerprint from its header; it never
silently applies the binary's newest registry to old bytes. Future compatible
binaries may retain v1 and add v2. A safety correction that cannot honor v1
must reject writable v1 open and require an explicit capability-gated
migration. Build-time overlap/completeness tests and a repository grep gate
ensure all GC, backup, sync, export, repair, migration, event, cache, and path
denial consumers agree with the selected fixture.

## 8. Writer Fencing and Peer/Cluster Negotiation

Before enabling local writes, startup hard-publishes a strictly larger
`writer_fence_epoch` in both-header A/B order. Every write reservation, task,
migration lease, buffered index mutation, GC publication, and commit guard
captures that epoch and rechecks it at authority publication. Work from a prior
process/epoch may be diagnosed or resumed only after acquiring the new fence;
it cannot publish from a stale handle.

The peer hello/accept transcript contains and authenticates:

~~~text
database_id[16]
database_header_version u16
hash_algorithm u16
selected_header_sequence u64
writer_fence_epoch u64
required_reader_capabilities[32]
required_writer_capabilities[32]
supported_reader_capabilities[32]
supported_writer_capabilities[32]
system_family_registry_version u16
system_family_registry_fingerprint[64]
root_format IDs and semantic/artifact codec sets
protocol feature bits
nonce/transcript digest under the existing authenticated channel
~~~

Before transferring or activating authority, each side proves database
identity, exact selected registry support, source readability, destination
writer support, required semantic/module closure, and route-specific transfer
policy. Unsupported bits, unknown codec, registry mismatch, or incomplete
dependency returns a typed incompatibility without changing destination state.

Read-only peers may inspect/transfer only state they can decode and are
authorized to receive. A destination peer may omit validated derived indexes
and node-local GC state exactly as the SystemFamily policy permits, but it must
construct its own compatible controls before root activation. No peer may write
bytes requiring a capability absent from the destination's stored writer floor.

This negotiation does not claim distributed consensus or multi-primary
linearizability. AeorDB's current peer model remains independent physical
databases with authenticated transfer and local authority publication. The
rules prevent incompatible mixed writers and foreign control authority; a
future shared-writer cluster needs a separate consensus/fencing plan.

## 9. Operational Configuration Contract

### 9.1 File, Schema, and Precedence

Operational budgets live at `/.aeordb-config/runtime.json`; lifecycle/deletion
policy remains in `lifecycle.json`. New runtime writes use strict schema v1:

~~~json
{
  "schema_version": 1,
  "memory": {},
  "cache": {},
  "index": {},
  "garbage_collection": {},
  "io": {},
  "query": {},
  "durability": {},
  "maintenance": {},
  "recovery": {},
  "shutdown": {}
}
~~~

Known properties may be omitted; unknown/duplicate properties, wrong types,
negative values, overflow, noncanonical paths, and violated cross-property
constraints reject the source. Canonical persisted byte quantities are `u64`
bytes and durations use the unit named by the property. Persisted `"auto"` is
allowed only where the table says auto; JSON `null` means unbounded only for
`gc.mark_scratch_max_bytes`.

Environment/CLI quantities accept a base-10 integer plus optional `KiB`, `MiB`,
`GiB`, or `TiB` suffix; suffix multiplication is checked. No decimals, ambiguous
`KB`/`GB`, negative values, or platform `usize` are accepted. Paths are native
input converted to a normalized absolute OS path, no-follow validated when
used, and never become database-semantic bytes.

Each property resolves independently in this exact order:

~~~text
built-in/auto default < validated stored file < validated environment < CLI
~~~

A present invalid winning value is an error and never falls through. A broken
lower-precedence stored file does not make a property unavailable when a valid
environment or CLI value supplies it; degradation remains visible. If the file
is malformed, unspecified properties resolve from validated LKG, then prior
append history. Built-in defaults apply to a genuinely missing file, not to a
present invalid file without validated fallback. This is the same resolver
contract already approved for lifecycle policy.

### 9.2 Exact Properties and Defaults

Let `R` be detected physical RAM, `H = clamp(floor(R/2), 1 GiB, 8 GiB)`, and
`S = max(768 MiB, floor(3H/4))`. On the 16 GiB production host, `H = 8 GiB` and
`S = 6 GiB`. Auto values are recomputed only at process start and are returned
as resolved integers. Writable v4 startup requires at least 2 GiB detected RAM;
smaller systems may use bounded diagnostics but receive no impossible auto
budget.

| Property | Default | Valid range/invariant |
| --- | ---: | --- |
| `memory.soft_limit_bytes` | `S` | 512 MiB..hard minus emergency reserve |
| `memory.hard_limit_bytes` | `H` | 1 GiB..min(128 GiB, detected RAM minus 512 MiB) |
| `memory.host_available_floor_bytes` | min(2 GiB, max(512 MiB, R/8)) | 256 MiB..R/2 |
| `memory.emergency_reserve_bytes` | min(256 MiB, H/8) | 64 MiB..H/4 |
| `cache.index_clean_max_bytes` | min(2 GiB, H/4) | 0..H/2 |
| `cache.index_clean_ttl_seconds` | 300 | 1..86,400 |
| `cache.directory_max_bytes` | min(512 MiB, H/16) | 0..H/4 |
| `cache.kv_resident_max_bytes` | min(2 GiB, H/4) | 64 MiB..H/2 |
| `cache.query_plan_max_bytes` | min(256 MiB, H/32) | 0..H/8 |
| `index.mutation_buffer_max_bytes` | min(1 GiB, H/8) | 16 MiB..H/4 |
| `index.flush_after_mutations` | 262,144 | 1..16,777,216 |
| `index.flush_after_seconds` | 30 | 1..300 |
| `index.publication_batch_max_bytes` | 256 MiB | 1 MiB..1 GiB |
| `garbage_collection.mark_memory_preferred_bytes` | 128 MiB | 64 MiB..H/4 |
| `garbage_collection.mark_memory_minimum_bytes` | 64 MiB | 32 MiB..preferred |
| `garbage_collection.mark_scratch_free_reserve_bytes` | max(8 GiB, min(64 GiB, 2% filesystem capacity)) | 1 GiB..50% filesystem |
| `garbage_collection.mark_scratch_max_bytes` | `null` | null or >= 1 GiB |
| `garbage_collection.checkpoint_after_seconds` | 300 | 30..3,600 |
| `garbage_collection.checkpoint_after_dirty_bytes` | 1 GiB | 64 MiB..64 GiB |
| `garbage_collection.mark_workspace_root` | private sibling auto path | normalized absolute path or `"auto"` |
| `io.read_prefetch_bytes` | 2,621,440 (10 chunks) | one chunk..64 MiB |
| `io.read_coalesce_max_bytes` | 16 MiB | prefetch..256 MiB |
| `query.per_request_memory_bytes` | min(128 MiB, H/64) | 8 MiB..H/8 |
| `query.global_memory_bytes` | min(1 GiB, H/8) | per-request..H/4 |
| `query.position_scan_buffer_bytes` | 8 MiB | 256 KiB..64 MiB |
| `durability.group_commit_max_bytes` | 64 MiB | 1 MiB..1 GiB |
| `durability.group_commit_max_delay_ms` | 100 | 0..1,000 |
| `maintenance.max_concurrent_tasks` | max(1, min(2, logical CPUs/4)) | 1..32 |
| `recovery.emergency_spill_dir` | OS user-data directory | normalized absolute path or `"auto"` |
| `recovery.emergency_spill_max_bytes` | 4 GiB | 64 MiB..1 TiB |
| `shutdown.operation_wait_seconds` | 600 | 0..86,400 |

GiB/MiB above are binary units. All cache, query, index, and GC reservations
are children of the same global coordinator and cannot sum independently above
the process ceiling. `cache.kv_resident_max_bytes` requires a mapped/paged KV
reader; it is not permission to materialize every fixed KV bucket as `Arc`
bytes. Query/position scans stream within their buffer and return typed resource
admission failure rather than partial semantic results.

The global coordinator reserves `memory.emergency_reserve_bytes` exclusively
for durability flush, health/status, spill, shutdown, and small bounded recovery
work. At soft limit it evicts clean derived data, flushes bounded dirty state,
shrinks spillable work, and pauses maintenance admission. At hard limit it
admits no new amplifying work, pauses/cancels safely spillable maintenance, and
preserves streaming reads plus small durable writes while the reserve remains.
Allocator owners must reserve before growth; RSS sampling is a backstop, not
the primary admission algorithm.

### 9.3 Exact Environment and CLI Names

The mechanical mapping is one-to-one. Prefix each dotted property, upper-case
it, and replace dots with underscores for environment; replace underscores
with hyphens after `--` for CLI. The complete v1 namespaces are therefore:

~~~text
AEORDB_MEMORY_*                  --memory-*
AEORDB_CACHE_*                   --cache-*
AEORDB_INDEX_*                   --index-*
AEORDB_GARBAGE_COLLECTION_*      --garbage-collection-*
AEORDB_IO_*                      --io-*
AEORDB_QUERY_*                   --query-*
AEORDB_DURABILITY_*              --durability-*
AEORDB_MAINTENANCE_*             --maintenance-*
AEORDB_RECOVERY_*                --recovery-*
AEORDB_SHUTDOWN_*                --shutdown-*
~~~

For example, `index.flush_after_mutations` is
`AEORDB_INDEX_FLUSH_AFTER_MUTATIONS` / `--index-flush-after-mutations`, and
`recovery.emergency_spill_dir` is `AEORDB_RECOVERY_EMERGENCY_SPILL_DIR` /
`--recovery-emergency-spill-dir`. The resolver generates accepted names from
the checked property registry; handwritten aliases are rejected except for a
single transition release mapping existing
`AEORDB_INDEX_CACHE_MAX_BYTES`, `AEORDB_INDEX_CACHE_CLEAN_TTL_SECS`,
`AEORDB_EMERGENCY_SPILL_DIR`, `AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES`, and
`AEORDB_SHUTDOWN_OPERATION_WAIT_SECS`. Conflicting old/new names are an error.
Deprecated use is visible in diagnostics and removed after the transition.

### 9.4 Activation Classes and Semantic Identity

- **Startup-bound:** process soft/hard/emergency limits and recovery spill
  directory. A persisted update is validated and reported `pending_restart`;
  it does not claim to constrain already allocated memory or change startup
  scan roots mid-process.
- **Next-run captured:** GC memory/scratch/checkpoint/workspace values and
  maintenance concurrency. An active run keeps its immutable checkpointed
  policy; a new run resolves current values.
- **Dynamically staged:** cache budgets/TTL, index buffer/flush/publication
  budgets, query budgets, prefetch/coalescing, durability group thresholds,
  spill maximum, host-available floor, and shutdown wait. Increases activate
  after validation. Decreases first block new reservations and evict/flush;
  only then does the effective value change. Failure remains visible as
  `pending_convergence`, never a false success.

Runtime and lifecycle resource policy is not part of namespace/index semantic
identity. Exact parsers, converters, tokenization, expansion, result ordering,
and semantic limits live in Round 10/11 definitions. Operational exhaustion may
delay, spill, fall back authoritatively, or return a typed retryable/unavailable
error; it may never produce a different result and label it complete.

### 9.5 Invalid-State Capability Matrix and Observability

If required memory/durability startup properties remain unresolved after valid
CLI/environment/LKG/history resolution, writable startup is refused while raw
diagnostics remain available. An unresolved cache property disables that cache.
An unresolved GC property disables the dependent mark/sweep operation. An
unresolved query budget rejects only memory-amplifying query/position work;
bounded direct/streaming reads continue. No invalid optional property is allowed
to disable unrelated storage.

`GET /system/runtime`, CLI status/verify, health, metrics, SSE administrative
events, and Dashboard expose persisted validity/schema, complete effective
values, each source, LKG identity/age, deprecated aliases, startup pending
values, convergence state, per-owner reservations/dirty/clean/evictable/pinned
bytes, soft/hard pressure, maintenance pauses, and every capability disabled by
an unresolved property. Secret paths may be root-only/redacted, but their
validity and source remain visible.

The lifecycle override names are likewise exact:

~~~text
AEORDB_LIFECYCLE_SNAPSHOT_WRITES_ENABLED
  --lifecycle-snapshot-writes-enabled
AEORDB_LIFECYCLE_SNAPSHOT_RETENTION_AUTO_MONTHS
  --lifecycle-snapshot-retention-auto-months
AEORDB_LIFECYCLE_SNAPSHOT_RETENTION_MANUAL_MONTHS
  --lifecycle-snapshot-retention-manual-months
AEORDB_LIFECYCLE_GARBAGE_COLLECTION_PENDING_DELETE_GRACE_SECONDS
  --lifecycle-garbage-collection-pending-delete-grace-seconds
~~~

## 10. One Cross-Platform Durability Coordinator

All durable writers use one `DurabilityCoordinator`. It exposes typed
dependency append, grouped commit, A/B control/header publication, durable
replace, preallocation, spill, and cutover primitives. Callers cannot invoke
raw `sync_data`, `sync_all`, rename, in-place overwrite, or Void registration.
Architecture checks permit those OS calls only in the coordinator and its
platform adapters. Every result is propagated; logging a failure is not
completion.

### 10.1 Commit Classes

There are only three durability classes:

1. **Hard authority:** user-visible file/delete/rename/merge/batch/blob commit,
   HEAD/snapshot/fork, auth/control mutation, configuration activation,
   migration cutover, allocator claim, quarantine/Void publication, repair,
   and any response promising committed state. Success waits for the containing
   hard group.
2. **Recoverable soft state:** buffered derived-index mutations, mutation/
   retirement acceleration journals, metrics, and disposable task progress.
   Loss may cost work or space but cannot lose acknowledged user data, admit a
   root, make an incomplete index complete, or authorize reclamation.
3. **Disposable:** clean caches, NVT hints, transient query/prefetch buffers,
   and recomputable diagnostics. These never block shutdown durability.

Hard operations may group for at most the configured 100 ms or 64 MiB. The
coordinator writes all dependencies, issues one platform data barrier, writes
and verifies authority-last bytes, issues the authority barrier, then completes
every waiter. One failure fails every waiter in that group and enters the
approved latch when serious. Cancellation after admission cannot cancel the
durability obligation; it only discards the client response after completion.

Chunk PUT may acknowledge staged/deduplicated chunk presence without an
individual barrier only when its response explicitly says `staged`. A later
`/blobs/commit` hard group validates every exact chunk and durably includes all
new chunk bytes before publishing the FileRecord/root and returning committed.
No commit trusts only an in-memory hash map or first-chunk identity.

### 10.2 Hard Publication Algorithm

For one database-file group:

1. acquire writer fence and reserve globally monotonic write/publication
   sequences plus append or already-published VoidClaim ranges;
2. write every complete dependency with checked `write_all`/positioned-write
   loops; short write, zero progress, range mismatch, or offset overflow fails;
3. flush language/runtime buffers and issue a data barrier covering dependencies
   and file-length metadata required to retrieve them;
4. read back/validate required framing, identities, checksums, and closure;
5. write all authority/control/KV mutations and the inactive DatabaseHeader
   slot, with the final slot naming the exact durable frontier;
6. issue the hard authority barrier and re-read the selected slot/control;
7. only then update post-commit caches/events/metrics and wake success waiters;
8. on any ambiguity, preserve dirty bytes through the spill protocol, latch
   read-only when serious, and never report success.

`sync_data` is permitted for an already allocated dependency extent only when
the platform contract guarantees required size/allocation metadata. File
creation, extension without prior durable allocation, truncation, authority
slot, cutover journal, and replacement use the platform's full metadata
barrier. Preallocation failure is handled as ordinary checked I/O; sparse-file
behavior is never inferred from apparent length alone.

### 10.3 Platform Adapters

- **Linux and other fsync-capable Unix:** positioned writes; `fdatasync` for
  eligible dependency data; `fsync` for authority/file metadata; same-filesystem
  atomic rename/replace; then `fsync` on every changed parent directory. An
  `EINTR` retry occurs only where the syscall contract leaves the operation
  retryable; uncertain completion is revalidated rather than blindly repeated.
- **macOS:** the same ordering, with `fcntl(F_FULLFSYNC)` for hard authority and
  cutover files when supported, ordinary `fsync` as the explicit weaker
  fallback reported in diagnostics, and parent-directory `fsync`. Release
  qualification records the actual capability used.
- **Windows:** database file handles use `FlushFileBuffers` for hard data and
  authority barriers. Durable replace uses same-volume `ReplaceFileW` when the
  target exists or `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` where
  applicable, followed by target reopen/verification. `sync_parent_dir` opens
  the directory with `CreateFileW` and `FILE_FLAG_BACKUP_SEMANTICS`, requests a
  native buffer flush, and returns the actual error/capability; it may never
  return success without a native operation. Because Windows/filesystem
  combinations do not universally prove same-volume directory-entry flush,
  strict multi-file protocols additionally use the recoverable cutover journal
  below. Unsupported directory flush is visible and is not relabeled durable.

Remote/network filesystems, FUSE layers, and storage devices may weaken an OS
barrier. Startup records filesystem/volume identity and runs a nondestructive
capability probe for required operations. A reported syscall success remains
the strongest guarantee the OS/device exposes; AeorDB does not claim protection
from lying hardware. Unsupported required primitives refuse the affected hard
multi-file operation. Main database authority remains in-file A/B state so
normal commits do not depend on directory rename durability.

### 10.4 Error, Retry, and Latch Rules

Transient `Interrupted` is retried at the lowest proven-safe boundary with a
bounded no-progress guard. Resource admission retries happen before writes.
After any write/barrier whose completion is uncertain, the coordinator probes
the exact bytes/control and either proves completion idempotently or fails
closed; it does not replay an authority mutation by assumption.

No-space/quota errors may trigger one bounded emergency flush/spill attempt and
free-space recheck. They are not infinite retry conditions. Permission,
read-only filesystem, invalid handle, media I/O, checksum disagreement, lost
device, and repeated no-progress failures do not become transient because a
timer elapsed. Serious failure uses the Round 6 latch policy. Every ignored
`Result`, `.ok()`, empty error arm, warning-and-default, `unwrap` in non-test I/O,
or success after failed cleanup/sync is part of the explicit repository-wide
error-squelch gate in the final campaign.

## 11. Side-by-Side V3-to-V4 Cutover

The source remains byte-for-byte v3 and online during copy. The v4 destination
is a same-filesystem sibling preserving the same logical database ID, with a
distinct migration physical-instance ID, two complete
512-byte headers, v1 entities, exact controls, capability floors, registry
fixture, and mutation-capture state. No tool grows or shifts the source in
place.

`SideBySideCutoverControl` exists in both migration databases and in a small
pre-created external `cutover.acut` A/B journal. The journal uses the common
32-byte control prefix, `ACUT`, the shared logical database ID, source and
destination physical-instance IDs, canonical path digests, migration/fence
IDs, source/destination verified header sequences,
expected file identities/sizes, and states:

~~~text
1 copying
2 final_freeze
3 destination_verified
4 source_renamed_to_backup
5 destination_installed
6 reopened_verified
7 complete
8 rolled_back
~~~

It is created and directory-synced before online migration begins; later state
changes are in-place A/B writes plus file barriers. It never contains secrets.
Its purpose is recovery evidence, not a third database authority.

Cutover is:

1. copy/convert while source writers continue and durable mutation capture
   records exact source sequence;
2. enter a short exclusive source write freeze, wait for admitted writes,
   hard-flush source, and capture its final selected header/sequence;
3. drain mutations, build exact root map/controls, hard-flush and full-verify
   destination, and record `destination_verified`;
4. stop source service handles, rename canonical source to a uniquely named
   preserved v3 backup, then record the observed state;
5. rename verified v4 destination to canonical path, reopen by path, validate
   database/header/capabilities/registry/HEAD/controls, and record
   `reopened_verified`;
6. start v4 in a bounded read-only validation window, exercise health/read/
   query/index/repair probes, and permit lossless rollback to the preserved v3
   source while no v4 write has been acknowledged;
7. require explicit operator acceptance before enabling v4 writes, then mark
   complete. After the first acknowledged v4 write, the v3 backup is evidence
   and recovery input, not a current rollback target. Backup deletion is never
   automatic.

Unix syncs the parent after each namespace change. Windows uses its real native
flush attempt plus write-through replace and the journal. At every restart the
cutover recovery command inspects journal slots and actual files by logical
database ID, physical-instance control, format/header identity, and verified
content, not filename alone:

- before source rename, source remains authority and migration may resume;
- after source rename but before destination install, install the already
  verified destination or durably restore source if destination validation now
  fails;
- after destination install, reopen/verify it before serving and retain source
  backup;
- any conflicting IDs, two plausible canonical authorities, missing claimed
  file, or journal disagreement aborts with a typed repair command and changes
  nothing.

Crash tests interrupt every write, barrier, journal slot, close, rename, reopen,
and service transition. The acceptable result is old v3 service, verified v4
service, or an explicit nonserving recoverable state with both copies retained;
never a guessed authority or deleted only-good copy.

The initial release does not claim a live reverse v4-to-v3 migration. If a
future rollout wants reversible canary writes, it must first add and prove a
durable compatibility journal that rejects every mutation v3 cannot represent,
replays all acknowledged compatible operations, and hard-verifies the resulting
v3 HEAD. Without that separate proof, rollback ends when v4 writes are enabled;
recovery afterward repairs or migrates the newest v4 authority forward.

## 12. Round 14 AGIS Proof Matrix and Architecture Gates

Before a v4 writer or migration cutover can ship, tests must prove:

1. hand-authored fixtures for every control kind, both hash widths, A/B
   selection, equal-sequence ambiguity, every enum/reserved/length boundary,
   and the expanded SystemFamily fixture on Linux, macOS, and Windows;
2. read-only/writable admission for every capability bit, unknown bits,
   registry versions/fingerprints, monotonic feature activation, attempted bit
   clearing, and diagnostic-only unsupported open;
3. peer negotiation for every compatible/incompatible reader/writer/registry/
   codec/module combination, proving zero destination mutation on refusal;
4. default/file/environment/CLI/LKG/history resolution for every runtime and
   lifecycle property, including malformed lower source plus valid higher
   override, strict unknowns, old-name conflicts, relational validation,
   dynamic convergence, next-run capture, and restart-pending state;
5. a modeled global memory coordinator under tiny budgets and the 6/8 GiB
   production profile: every owner reserves, clean indexes/directories/KV pages
   evict, dirty buffers flush, maintenance pauses, status remains responsive,
   and no complete query silently changes result;
6. first-root admission with faults before/after every dependency, prepare,
   KV mutation, commit witness, inactive header write, barrier, read-back, cache,
   and event. Dirty rebuild never admits a prepared orphan and never loses a
   fully selected first authority;
7. repair latch, migration lease/fence, task pin, semantic mutation journal,
   LKG, durability latch, and spill discovery through torn slots, missing
   acceleration records, stale epochs, restart, and explicit recovery;
8. group commit with concurrent file/blob/batch/control operations, cancellation,
   timeout/disconnect, short writes, `EINTR`, disk full, quota, permission,
   read-only remount, lost device, sync error, read-back mismatch, and shutdown;
   no waiter succeeds before its exact hard frontier;
9. Linux directory-fsync, macOS full-sync/fallback reporting, and real Windows
   file/replace/directory-handle paths. The Windows test fails if the adapter
   performs no syscall or maps unsupported/error to success;
10. side-by-side migration from verified v3 fixtures and a copied production
    database, with a crash at every cutover transition and deterministic old,
    new, or recoverable-nonserving result while the v3 backup remains intact;
11. `/tmp/codex` HTTP and embedded real-world runs covering write/query/index,
    two GC marks/sweep/Void reuse, restart, config override/degradation,
    snapshot/fork/sync, memory pressure, verify, and clean shutdown; and
12. release-candidate crash/fixture/migration tests on native Linux, macOS, and
    Windows filesystems, not cross-compiled unit tests alone.

Repository gates reject:

- OS sync/rename/overwrite calls outside `DurabilityCoordinator` adapters;
- direct control keys/codecs outside `ControlStore`;
- direct namespace/semantic/HEAD publication outside the mutation coordinator;
- independent environment parsing outside `ConfigResolver`;
- hard-coded protected-family lists outside the checked fixture;
- cache/index/GC/query growth without a memory reservation;
- any ignored durability/config/repair result or warning-success path; and
- any CLI/HTTP/scheduled/plugin/SDK route that implements a second state
  machine instead of adapting to the shared service.

### Round 14 verdict

Round 14 is closed. Every persisted coordination record has one typed bounded
codec and failure policy; readers and writers are admitted by explicit
capabilities and selected registry bytes; operational policy has one strict
per-property resolver; memory owners share one ceiling; hard success crosses
one tested durability boundary; and v3-to-v4 cutover always retains a recoverable
old copy. The current Windows no-op and warning/default behavior are explicit
implementation blockers, not accepted compatibility behavior.

---

# Final Synthesis: Ratification-Ready AeorDB V4/NVT Campaign

**Status:** All design rounds are closed. This is the regenerated campaign
source for owner ratification and later conversion into formal parent/child plan
files. No unresolved owner policy decision remains.

## 1. Non-Negotiable Invariants

The implementation and every review are governed by these invariants:

1. The production v3 source is never upgraded, repaired, shifted, or tested in
   place. Format migration writes a separate v4 database and preserves v3.
2. A successful user-visible mutation means its exact dependencies and
   authority are durably selected. Authority is always dependencies-first and
   selected-last through one coordinator.
3. Logical identity and physical incarnation are distinct. Every stable-key
   replacement is journaled/reconciled; a lower incarnation is never inferred
   free from KV absence or a raw gap.
4. Uncertainty may leak space or force read-only/degraded operation. It may not
   lose acknowledged data, admit an orphan root, claim a partial index complete,
   or reclaim earlier.
5. One admitted `NamespaceRootV1` binds one namespace tree and one exact
   complete/content-only semantic state. Historical reads never consult current
   mutable parser/config/plugin aliases.
6. Only current authority or a committed first-admission witness makes a root
   externally resolvable. Internal directory/page hashes never masquerade as
   `/`.
7. `root_hash` is an explicit read selector, not a stored/signed/base64 root
   cursor. Every successful read returns root state and advisory expiry.
   Pagination position is separate APOS state plus request `limit`.
8. Current authorization is reapplied before historical existence, metadata,
   counts, timing, snippets, positions, or EXPLAIN become observable.
9. Converter definitions own semantics. Values and query bounds use the same
   canonical typed comparison and fixed coordinate; no correctness depends on
   floating-point order or locale/process defaults.
10. Posting pages are the ordered source. Sparse NVT maps normalized ranges to
    nearby page hints, tolerates gaps/staleness/resolution loss, and is always
    validated by exact page scan/recheck.
11. Index mutation is asynchronous and coverage-aware. A query uses a complete
    compatible generation, an exact partial-plus-authoritative fallback, or a
    typed unavailable result; blob/file commit does not mutate whole indexes.
12. Search-hit locations are generated on request by bounded source scanning;
    no positional index is required. The returned root/content identity feeds
    exact range fetch for bot workflows.
13. Listing order is directories first in both ascending and descending modes,
    with alphabetical order inside each category and APOS continuity across the
    category boundary.
14. Destructive GC requires one complete discovery mark, a later complete mark,
    frozen per-candidate grace, final reachability/pin/incarnation checks, and a
    receipt-backed Void publication.
15. Mark/inventory state is bounded and durably resumable through verified
    external workspaces. Missing/corrupt scratch abandons work, never safety.
16. Reusable space comes only from the active receipt-backed Void catalog minus
    durable claims. Gap scan, hot tail, candidate state, and locator absence are
    evidence, not allocator authority.
17. All memory-amplifying owners reserve through one coordinator. The 16 GiB
    reference host runs AeorDB at 6 GiB soft/8 GiB hard with emergency headroom;
    indexes, KV pages, directories, queries, and maintenance are bounded/evictable.
18. One strict per-property resolver owns stored/default/environment/CLI/LKG
    policy. A valid higher override may bypass a malformed lower source, but the
    degradation is visible and no invalid present value silently becomes a
    default.
19. Database capabilities and the selected binary-owned SystemFamily fixture
    gate every read/write/peer activation before mutation. Semver and plugin
    claims cannot bypass the stored reader/writer floor.
20. Serious durability failure latches the database read-only and attempts to
    preserve dirty state in approved spill locations. Startup discovers spills
    and requires explicit ordered repair; errors are never warning-success.
21. Side-by-side migration preserves logical database identity and distinguishes
    physical instances. V3 rollback is lossless only before any v4 write is
    acknowledged; after acceptance the old copy is evidence, not current state.
22. HTTP, CLI, scheduled, plugin, sync, repair, migration, and embedded SDK
    surfaces are adapters over the same service/state machines, never parallel
    implementations.

## 2. Plan Supersession and Preservation Map

The formal parent generated from this synthesis will supersede
`bot-docs/plan/2026-07-16-nvt-field-index-refactor-and-migration.md` in full.
The following plans are incorporated/superseded only for the named scope:

- `indexing-engine.md`, `indexing-unified.md`, and
  `fuzzy-trigram-phonetic.md`: converter, page, cache, coverage, and NVT design;
- `query-engine.md`, `query-sorting-pagination.md`, its implementation plan,
  aggregations/projections/EXPLAIN: root-aware index planning/order/APOS rules;
- `document-parsers.md` and its implementation plan: immutable semantic
  definitions/dependencies and exact fallback behavior;
- both GC mark/sweep plans: physical incarnation inventory, bounded resume,
  quarantine, receipts, Void claims, expiry evidence, and audit retention;
- `search-hit-locators-for-agent-range-fetch.md`: root-bound on-demand locators
  and exact range-fetch continuity;
- `2026-06-15-durability-and-error-squelch-audit.md`: centralized durability,
  latch/spill policy, plus the still-required repository-wide squelch audit;
- backup/import, replication, tasks/reindex, system-table migration, API keys,
  events/SSE, and permissions only where the SystemFamily/read-view/control
  contracts change their behavior.

The KV remains deliberately out of refactor scope. `disk-resident-kvs*`,
`concurrent-kv-readers*`, B-tree, content-addressed FileRecord/chunk, media range
read, and general storage plans remain authoritative except where this campaign
adds physical-incarnation continuation metadata, capability admission, memory
budgeting, durability routing, or typed corruption policy. The NVT orders random
KV page offsets; the KV itself is not redesigned to be ordered.

Formalization creates one parent and these child plans, each with a
`superseded-by`/`incorporated-by` banner on stale documents rather than deleting
history:

~~~text
2026-08-03-aeordb-v4-nvt-gc-refactor.md
  /children/01-format-capabilities-and-fixtures.md
  /children/02-durability-controls-config-and-memory.md
  /children/03-namespace-semantic-roots-and-system-families.md
  /children/04-physical-inventory-gc-and-void.md
  /children/05-index-definitions-pages-and-nvt.md
  /children/06-async-coverage-query-pagination-and-locators.md
  /children/07-side-by-side-migration-cutover-and-rollout.md
  /children/08-verification-operations-docs-and-debt.md
~~~

## 3. Dependency Graph and Landing Strategy

~~~text
P0 evidence/oracles/fixture source
  -> P1 reader-first format + platform foundations
      -> P2 current-v3 pathway consolidation and fail-closed hardening
          -> P3 shadow v4 namespace/semantic/migration substrate
              -> P4 physical inventory + bounded GC + Void safety
              -> P5 index definitions + immutable pages + sparse NVT codecs
                    -> P6 async coverage/cache/publication runtime
                        -> P7 root-aware query/APOS/locators/API cutover
              P4 + P7 -> P8 integrated migration/canary/production cutover
                          -> P9 docs, retirement, evidence, debt deletion
~~~

P4 and P5 may proceed in parallel only after P3 freezes shared artifact/control
APIs. They may not concurrently edit shared format registries, `engine/mod.rs`,
Cargo manifests, common errors, or fixture manifests; one integration owner
lands those tiny changes. Every phase is a small-series branch with target-
failing tests first, reviewable commits at contract/reader/writer/activation
boundaries, and a pushed green phase snapshot. No phase hides unrelated cleanup.

## 4. Executable Phases

### P0: Evidence, Independent Oracles, and Freeze Inputs

**Entry:** current `development` commit recorded; worktree/user changes preserved;
all production/evidence databases copied before inspection.

**Owned outputs:** `bot-docs/plan/`, `aeordb-lib/spec/fixtures/`, a standalone
`tools/v4-reference/` fixture/reference implementation with no dependency on
production serializers, benchmark/operation-ledger fixtures, and test-only
fault/model harnesses.

**Work:**

- capture v3 headers/entities/KV/root/query/list/search/fetch/index/GC/backup/
  sync/startup/blob-commit behavior from synthetic and copied real databases;
- run old-versus-old twice, classify nondeterministic fields, and create the
  explicit intended-divergence allowlist;
- expand the exact ASFR fixture and mechanically freeze its bytes/fingerprint;
- hand-author both hash-width fixtures for common header, all Round 10-14
  artifacts/controls, APOS, malformed cases, and canonical values;
- build independent converter/order/query/pagination/root-state/GC-eligibility/
  cutover reference models;
- inventory every raw sync/rename/write, ignored Result/default-on-error,
  protected path list, direct HEAD/root/control mutation, environment parser,
  cache owner, stable-key replacement, and whole-index load;
- record target latency/RSS/I/O baselines without converting them into
  correctness exceptions; and
- regenerate the formal parent/children/supersession banners from this decision
  log and require owner ratification before P1 production work.

**Exit:** fixture bytes/manifest agree independently on all platforms; every
current producer/consumer has an owner row; all known defects have target-
failing tests; no unresolved policy placeholder, `TBD`, or serializer-generated
golden exists.

**Rollback:** documentation/test-only; remove the unratified fixture branch.

### P1: Reader-First Format and Platform Foundations

**Owned modules:** new bounded format/registry/control readers, DatabaseHeader
v3/v4 and EntityHeader v0/v1 dispatch, platform durability adapters,
`ConfigResolver`, and `MemoryCoordinator` interfaces. Existing production write
callers are not rerouted yet.

**Work:**

- implement readers against P0 bytes before writers; reject unknown capabilities,
  kinds, versions, padding, overflow, noncanonical order, and trailing bytes;
- implement selected SystemFamily fixture loading/classification and capability
  admission in diagnostic/read-only mode;
- implement `ControlStore` decode/A/B selection without production publication;
- implement Linux/macOS/Windows durability probes and test-only publication,
  including the Windows native directory-handle path and unsupported result;
- implement strict runtime/lifecycle source parsing and complete effective
  diagnostics without yet changing current callers; and
- implement reservation/accounting APIs plus owner metrics with a no-op
  compatibility adapter only inside this phase.

**Exit:** production writers still emit only v3/v0; all golden/mutation/fuzz
reader cases pass identically on native Linux/macOS/Windows; unsupported writable
open refuses before mutation; no legacy fixture is rewritten by open/verify.

**Rollback:** modules are unreachable from production writers and can be removed
without touching persisted state.

### P2: Current-V3 Consolidation and Fail-Closed Hardening

This is a deployable safety phase before format migration, split into narrow
P2a durability, P2b config/memory, P2c mutation/system-family, and P2d error-
squelch snapshots.

**Owned modules:** `durability`, append/hot-tail/storage write plumbing,
`lifecycle_config`, new runtime resolver, caches/KV snapshots, directory/system/
version operations, task/index/GC adapters, backup/sync/import, server routes,
plugin host writes, and metrics/health/dashboard contracts.

**Work:**

- route every acknowledged v3 write through grouped `DurabilityCoordinator`;
  implement latch/spill discovery/repair and remove Windows/no-op success;
- replace per-module environment/default parsing with one resolver while
  retaining documented transitional names;
- replace full fixed-page KV snapshots and unbounded directory/index values
  with mapped/lazy/budgeted ownership under the 6/8 GiB coordinator;
- introduce `SystemFamilyRegistry`, `NamespaceMutationCoordinator`,
  `LocatorReplacementCoordinator`, and shared event/metric acknowledgement;
- route HTTP, CLI, scheduled, plugin, sync, repair, migration, and SDK producers
  through those facades while retaining v3 bytes and established semantics;
- make protected-family incompleteness, lifecycle corruption, B-tree damage,
  and durability failure typed/fail-closed at the approved scope; and
- complete the repository-wide error-squelch audit, classify each occurrence as
  intentionally disposable/retryable/fatal, remove unowned ignored errors, and
  add lint/grep gates with reviewed allowlist comments.

**Exit:** full current-format behavior suite plus real `/tmp` HTTP/SDK run and
S1/S2/S3 bounded soaks pass; 16 GiB host profile stays below 8 GiB hard with
responsive health; every mutation emits one operation ID/event/metric; no raw
durability/control/path-list bypass remains. Production may deploy this phase
without creating v4 data.

**Rollback:** revert the phase binary; persisted v3 bytes remain readable. A
latched/spilled database follows explicit repair and is never cleared merely to
make rollback start.

### P3: Shadow V4 Namespace, Semantics, and Migration Substrate

**Owned modules:** v4 writer codecs, semantic definition/catalog compiler,
`SemanticReadViewResolver`, root publication coordinator, typed controls,
migration lease/progress/root-map/cutover journal, and shadow-clone CLI. No index
or GC high-volume writer is activated here.

**Work:**

- implement exact v1 entity/header/root/semantic/control writers that reproduce
  P0 fixtures and use P2 durability/memory/registry services;
- implement first-authority prepare/admission and one root resolver for every
  read/maintenance surface, including current authorization and root states;
- construct v4 destinations with preserved logical database ID, separate
  physical migration instance, complete protected-family policy, and finite
  legacy-root map;
- capture source mutations durably under bounded disk/free-space policy while
  source GC is suspended; checkpoint/cancel/resume with fencing;
- compile current and provable historical semantic worlds, otherwise exact
  content-only states; and
- expose verify/dry-run/ETA/operation-ledger diagnostics, but keep destination
  shadow-only and refuse service write admission/cutover.

**Exit:** copied v3 databases produce deterministic fully verified shadow v4
roots and root maps; every first-authority crash point yields old authority or
unadmitted orphan; all reads from a selected shadow root reproduce intended v3
content/semantics or the approved content-only error; source remains unchanged.

**Rollback:** delete only the identified destination/workspace after preserving
diagnostics; source lease/GC suspension is durably released and source resumes.

### P4: Physical Inventory, Bounded GC, and Void Safety

**Owned modules:** GcArtifact codecs, replacement/retirement journal,
inventory, mark workspace/checkpoint, quarantine/expiry/audit, sweep receipts,
Void catalog/claims, repair tickets/path latches, and GC CLI/API/dashboard.

**Work:**

- land all GC readers/fixtures before writers and replacement journaling before
  any v4 stable-key replacement;
- build bounded external-sort inventory and dense/spilled mark state with
  durable checkpoint resume and mutation convergence;
- make typed B-tree continuation/corruption policy protect uncertain closures;
- publish incomplete/canceled work nowhere; publish candidate state only after
  complete reconciliation and exact policy capture;
- implement two-mark/frozen-grace/final-guard sweep proposal, deletion state,
  receipt-backed Void publication, recovery receipt, and allocator claims; and
- prove root expiry/audit/evidence retention and migration destination
  `never_marked` behavior.

**Exit:** model/fault tests establish that every crash/corruption either retains
data or leaks space; 8 GiB hard-ceiling production-shaped run completes/resumes;
two full marks and a sweep/reuse cycle survive reopen and full byte verification.
No production cutover occurs yet.

**Rollback:** disable destructive GC and append allocation only. Previously
published v4 quarantine/Void remains interpreted by compatible readers; never
run an older writer lacking stored capabilities.

### P5: Index Definitions, Immutable Pages, and Sparse NVT

**Owned modules:** Converter/FieldIndex definitions, built-in conformance bundle,
IndexArtifact envelope/manifests/pages/directories/journals/checkpoints, NVT
tiles, scope/value/document state, and independent codec tests.

**Work:**

- implement exact typed equality/order/token/phonetic converters and query
  literal compilation against definitions;
- implement immutable COW page chains/directories with coordinate-first total
  order, links/fences/ranks/physical spans, stable PageIds, and no whole-index
  load;
- implement sparse fixed-point NVT insertion/query-range hints, backward anchor
  search, stale/corrupt fallback, and rebuildability;
- implement document/scope/value identity, tombstone/state, mutation journal,
  checkpoint, and shadow compaction; and
- build v1 indexes against immutable shadow roots without activating production
  pointers.

**Exit:** independent evaluator and production agree on every definition/value/
coordinate/order; page scans match reference with empty/stale/corrupt NVT in
both directions; memory stays bounded; artifacts reproduce fixtures after
restart/compaction/shuffled build.

**Rollback:** discard shadow derived artifacts and controls. Namespace/user data
is untouched.

### P6: Asynchronous Coverage, Cache, and Publication Runtime

**Owned modules:** shared mutation stream, IndexCoordinator workers, coverage
planner/state, memtables/spill/reconciliation, page publication/compaction,
cache admission/eviction, task controls, and index metrics/health.

**Work:**

- emit one typed mutation from every namespace producer before HEAD visibility;
- publish the coverage transition with authority, then parse/index asynchronously
  through bounded task pins/checkpoints and mutation catch-up;
- batch all index kinds through one 262,144-mutation/30-second/budget-triggered
  path and one page publication coordinator;
- replace whole-index/value maps with lazy page/value reads and clean LRU/TTL/
  byte-accounted eviction; dirty state spills or flushes before eviction;
- install exact complete/partial/degraded/fallback plan contracts and reconcile
  crashes/gaps against global publication sequence; and
- prove `/blobs/commit`, raw SDK batch, merge, sync, plugin write, and ordinary
  PUT share the same fast mutation acknowledgement and do no synchronous parser
  or whole-index mutation.

**Exit:** high-concurrency writes with many configured indexes remain bounded,
acknowledged content is exact after crashes, index generations converge, and
commit latency excludes derived work. Dropping all clean index cache reduces RSS
near baseline without changing query results.

**Rollback:** deactivate v1 index registry pointers and use authoritative v4
source evaluation. Derived artifacts remain quarantinable; user data/roots stay
valid.

### P7: Root-Aware Query, Pagination, Locators, and API Cutover

**Owned modules:** query/search/sort/aggregate/EXPLAIN planners, listing,
`ResolvedReadView`, APOS codecs, hit locators/snippet/range continuity, HTTP and
embedded schemas, filtered SSE event projection, web/client integration specs,
and API docs drafts.

**Work:**

- compile predicates into exact index coordinates/ranges, choose plans by
  measured page/posting/cardinality cost, intersect by scope-local document
  identity, and exact-recheck every candidate;
- implement page-chain scans from sparse hints and authoritative fallback for
  absent/stale/degraded coverage;
- apply directories-first listing and each route's frozen total order with APOS
  root/order/result binding, request limit, and legacy cursor transition;
- accept `root_hash` on every approved read, return root metadata on every
  success, and preserve one root through search -> locator -> snippet/range;
- generate opt-in match byte/codepoint/line ranges by bounded source scan,
  including CRLF rules, without positional indexes;
- make virtual fields actual index consumers: exact equality for all, approved
  richer filename/path strategies, and no trigram for hash/content type;
- emit authorized full-path SSE mutations and root/generation metadata rather
  than relationship-only invalidation; and
- update server/client/UI consumers together so no route quietly falls back to
  current HEAD/current semantics.

**Exit:** every query/list/search/aggregate/pagination result matches the
independent model across roots, directions, mutations, restarts, cache eviction,
and damaged hints; bot E2E finds text, receives ranges, fetches only snippets,
and reproduces exact bytes; unauthorized roots/paths leak no observable state.

**Rollback:** select no v1 index generation and disable new APOS/locator feature
advertisement while preserving v4 direct reads. Do not run an incompatible old
writer.

### P8: Integrated Migration, Canary, and Production Cutover

**Entry:** P4 and P7 complete; native platform release candidates green;
production source backup/capacity/maintenance approvals explicit; no evidence DB
is ever the migration target.

**Work:**

- preflight source strict verify, storage/filesystem/durability capability,
  destination + workspace + v3 backup + mutation journal capacity, host memory,
  protected families, modules, and rollback boundary;
- clone v3 to v4 online, build v1 indexes against captured roots, catch up both
  namespace and index journals, run two non-destructive complete v4 marks, and
  keep v4 sweep disabled before cutover;
- compare all intended behaviors/operation ledgers against P0 and classify every
  remaining divergence; no sorting/log filtering hides unexplained differences;
- rehearse cutover and pre-write rollback on a copied production database, then
  on a canary under real clients and load;
- perform the Round 14 journaled cutover, reopen full-verify, and serve a bounded
  read-only validation window while v3 remains a lossless rollback;
- obtain explicit operator acceptance, enable v4 writes, announce the rollback
  boundary, and monitor memory, durability, queue/index coverage, GC state,
  health latency, blob throughput/commit, query correctness, disk/Void, and
  repair latches;
- only after stable operation run the first production complete mark; wait for
  the later complete mark plus grace before any production sweep; and
- build/install/package native Linux, macOS, and Windows binaries with the same
  fixtures/capabilities. Commit small phase snapshots and push each completed
  integration step before deployment.

**Stop/rollback:** before v4 writes, any unexplained mismatch, failed barrier,
memory breach, corrupt control/tree, missing module/family, or health starvation
returns service to the untouched v3 source. After a v4 write is acknowledged,
do not rename stale v3 back; latch/read-only as required and repair/migrate the
newest v4 state forward. A separately proven reverse journal is future scope.

**Exit:** accepted production v4 service remains under configured memory,
survives dirty restart, maintains query/index coverage, completes marks without
unsafe sweep, and has an operator evidence packet plus retained v3 backup.

### P9: Documentation, Retirement, and Completion Evidence

**Work:**

- update `docs/` API/SDK/bot `SKILL.md`, root/search/range/APOS behavior,
  configuration, memory, lifecycle/GC/Void, format/capability, migration,
  rollback boundary, verify/repair/spill, backup/sync, and platform durability;
- update Dashboard/admin CLI for complete config sources, capability/registry,
  memory owners, index coverage/cache, mark/checkpoint/quarantine/Void/claims,
  latches/spills, migration/cutover, and actionable repair commands;
- add supersession banners and cross-links to every incorporated plan;
- retain v0/v3 readers, fixtures, and v3 backup policy for the declared support
  window; remove v0 writers/mutation paths only after that policy and real
  rollback window close;
- delete compatibility aliases, duplicate lists/state machines, whole-index
  caches, direct stable-key replacement, and approved temporary feature gates;
- run the complete native release matrix, real `/tmp` test, copied-production
  migration, and S1/S2/S3/crash campaigns; and
- publish `dod-evidence.md` with baseline commit, fixture digests, test/soak
  commands/results, divergence rulings, performance/RSS evidence, deployments,
  retained debt, and exact recovery/rollback instructions.

**Exit:** documentation and binaries agree; grep/architecture gates are zero or
carry explicit reviewed allowlist reasons; no stale plan appears current; a new
operator can migrate, monitor, repair, and recover without implementation lore.

## 5. Verification Spine and Commands

Every ordinary local Rust command uses at most six jobs. Existing scripts are
updated to honor `CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-6}` before campaign use.
Linux developer/CI gates are, with phase-specific target tests run first:

~~~bash
cargo fmt --all -- --check
timeout 30m cargo test -j 6 -p aeordb --all-targets
timeout 30m cargo test -j 6 -p aeordb-cli --all-targets
timeout 45m cargo test -j 6 --workspace --all-targets
timeout 30m cargo clippy -j 6 --workspace --all-targets -- -D warnings
cargo build -j 6 --release --bin aeordb
~~~

Long tests are named/ignored harnesses with their own watchdog, progress file,
database copy, seed, and artifact directory; they are not hidden inside the
ordinary 30/45-minute gates:

~~~bash
AEORDB_SOAK_DB=/tmp/codex/aeordb-v4-s1/soak.aeordb \
  AEORDB_SOAK_HOURS=12 ./scripts/soak.sh s1
AEORDB_SOAK_DB=/tmp/codex/aeordb-v4-s2/soak.aeordb \
  AEORDB_SOAK_DURATION_SECS=43200 ./scripts/soak.sh s2
AEORDB_SOAK_DB=/tmp/codex/aeordb-v4-s3/soak.aeordb \
  AEORDB_SOAK_DURATION_SECS=43200 ./scripts/soak.sh s3
./scripts/crash_inject_soak.sh 100
~~~

Each phase adds exact test binaries for format fixtures, root-publication model,
GC model, durability fault matrix, memory-pressure workload, query reference,
and migration/cutover. Their final command names are checked into the child plan
when their Cargo targets are created; no plan invents a command for a nonexistent
target and then treats it as evidence.

Native `wyatt-mac` and `win11vm` qualification builds/tests, rather than Linux
cross-compiles, must run the same fixture manifest and platform durability/
cutover suites. A Linux release alone cannot clear a persisted-format gate.
Every significant API/SDK/storage phase also starts a real database below
`/tmp/codex`, exercises HTTP and embedded APIs with real JSON/text/binary files,
reopens/verifies it, and preserves the command/result ledger.

### Measurable Release Gates

- **Correctness:** zero unexplained reference/result/fixture divergence; no
  partial result labeled complete; all acknowledged writes readable after every
  modeled crash point.
- **Memory:** under an 8 GiB cgroup/job-object ceiling, the production-shaped
  workload remains alive and responsive; coordinator accounting identifies all
  material owners and stays within the ceiling including GC/index/query overlap.
- **Durability:** every success waiter is at or below the selected hard frontier;
  injected barrier/rename/read-back failure returns error and the correct
  latch/recovery state, never success.
- **Blob commit:** already-present-chunk commit reads zero file-content bytes and
  performs no synchronous parser/index work; work scales with manifest/chunk
  metadata and the required durability barriers, measured by phase counters.
- **Index/query:** clean-cache eviction changes latency only; results remain
  exact. Page reads and resident bytes are bounded by the selected plan/range,
  not total index cardinality.
- **NVT:** empty, stale, sparse, collision-heavy, wrong-resolution, and corrupt
  hints all produce the reference result or typed incomplete error.
- **GC:** no incomplete mark changes active quarantine; no candidate/receipt
  ambiguity enters Void; every failure direction is retained data or leaked
  space. Workspace and RSS stay inside captured budgets.
- **Health:** health/readiness/config/memory progress remain schedulable under
  upload/index/GC pressure; after P0 calibration, production target is p99 below
  one second and no sample above five seconds absent an OS-level stalled I/O
  report.
- **Performance:** unaffected baseline operations may not regress more than 10%
  at p50 or p95 without an explicit evidence-backed divergence ruling. Safety
  barriers are not removed to pass a timing target.
- **Migration:** source checksum/size/header are unchanged before cutover;
  destination full verify and behavior ledger pass; every crash yields old,
  new-read-only, or typed recoverable state; no v3 rollback is attempted after
  acknowledged v4 writes.

## 6. Worker Ownership and Review Protocol

When implementation is authorized, worker briefs are generated from the child
plans with explicit owned and forbidden paths. Logical ownership is:

| Workstream | Primary ownership | Forbidden without integration owner |
| --- | --- | --- |
| Format/fixtures | v4 codecs, fixture/reference tools | query/GC algorithms |
| Durability/config/memory | platform adapters, resolver, coordinator, metrics | artifact schemas |
| Namespace/semantics | read view, mutation/root coordinator, semantic compiler | GC/Index page bodies |
| GC/physical | GC artifacts, inventory, Void, repair | converter/query semantics |
| Index | definitions, pages, NVT, async worker/cache | namespace authority/GC sweep |
| Query/API | planner, APOS, locators, route/client/UI adapters | persisted codecs |
| Migration/rollout | clone/capture/cutover/operations/docs | changing frozen formats |

Shared registries, `engine/mod.rs`, common errors, Cargo manifests, route
assembly, docs navigation, and fixture manifests have one integration owner.
Workers request those edits rather than racing them. Review is contract-first:
reader/fixture, malformed cases, writer, crash behavior, activation, then
performance. A production serializer cannot review its own generated golden;
the reference/fixture owner and subsystem owner must both sign the bytes.

At each meaningful snapshot:

1. run the narrow target tests;
2. record intentional divergence and resource evidence;
3. commit the coherent small change;
4. at completed phase/integration boundaries, run broad gates and push; and
5. deploy only a green committed release/debug artifact through the checked
   install/deploy scripts, with backup/health/restart monitoring appropriate to
   the phase.

## 7. Required Evidence Artifacts

The campaign is incomplete without versioned artifacts for:

~~~text
baseline-environment.json
baseline-behavior-and-performance.json
intended-divergences.yaml
persisted-producer-consumer-inventory.json
format-fixture-manifest.json
system-family-registry-v1.manifest.json
capability-matrix.json
error-squelch-inventory.yaml
memory-owner-budget-report.json
durability-platform-report.json
root-publication-model-report.json
gc-model-and-fault-report.json
query-reference-report.json
migration-operation-ledger.json
cutover-crash-report.json
production-canary-report.json
dod-evidence.md
~~~

Each report names source commit, binary hash, database-copy checksum, platform/
filesystem, config sources, random seeds, commands, start/end times, timeout,
result, and preserved logs. Evidence databases are never committed or pushed.

## 8. Final Adversarial Corrections Incorporated

The last AGIS/planning-cap pass found and corrected these cross-round issues:

1. numeric canonical bytes remain the approved little-endian representation;
   typed converter comparison, not a big-endian byte shortcut, owns order;
2. postings order by coordinate first, preserving the NVT's range geometry;
3. the mark mutation record hash count was corrected from seven to six;
4. Void extents no longer reference the receipt that references their catalog,
   removing a content-hash cycle; the receipt owns actual durable commit time;
5. raw gap/hot-tail tuples never authorize reuse, and claimed Void is removed
   durably before overwrite;
6. capability writer floor/registry/fence consume the planned v4 header reserve
   before any writer exists, leaving 44 bytes future reserve;
7. group-commit settings are named as hard acknowledgement policy, not
   misleading “soft flush” policy;
8. auto memory limits remain valid on the supported low-memory floor while
   yielding the approved 6/8 GiB production envelope;
9. `runtime.json` received an explicit SystemFamily classification and strict
   LKG/source behavior;
10. side-by-side format migration preserves logical database identity and uses
    separate physical-instance IDs; and
11. v3 backup rollback ends before the first acknowledged v4 write unless a
    future reverse journal is separately designed and proven.

## 9. Remaining Outputs Versus Decisions

There are no remaining owner-only decisions. The following values are not open
policy; they are mechanical outputs that P0 must compute and freeze before a
writer exists:

- exact ASFR expanded fixture bytes/fingerprint;
- complete golden CRC/hash/hex values;
- baseline timing/RSS/I/O distributions and calibrated regression envelopes;
- copied-production capacity/ETA; and
- native platform capability probe results.

If those outputs expose an impossible byte layout, unsupported required
durability guarantee, unbounded state, or conflicting invariant, implementation
stops and reopens a numbered design round in this decision log. A worker may
not silently alter a field, capability, order, error, migration rule, or safety
barrier because code made the approved contract inconvenient.

### Final synthesis verdict

Rounds 1 through 14 and the final synthesis are complete. The plan now states
the desired AeorDB architecture, exact persisted/wire/control contracts,
migration and rollback boundaries, phase dependencies, ownership, verification
oracles, operational budgets, and stop conditions. The next action is owner
ratification followed by mechanical generation of the formal parent/child plan
files; implementation remains gated until that artifact set and P0 fixtures are
reviewed.

---

# Round 15: Full-Plan Gauntlet, Fault Review, and Normative Corrections

**Date:** 2026-08-03
**Baseline reviewed:** `5d3e284652f9fec7a5c843f1946132574af4d469`
**Status:** CLOSED by a full AGIS/planning-cap correctness pass. This round is
normative and supersedes every conflicting clause in Rounds 1 through 14 and
the first Final Synthesis. The corrected synthesis remains subject to owner
ratification before formal plan generation or implementation.

## 1. Review Scope and Honest Verdict

The earlier cross-round synthesis was a substantial review, but it was not a
fresh, independent gauntlet over the entire completed plan. This pass did that
work. It reapplied territory mapping, persistent-format arithmetic, state-
machine fault analysis, security/API review, migration/rollback review,
execution-hotspot review, and AGIS `test_protocol`/`thorough_review` against the
decision log and the live source tree.

The inspected territory included:

- every Round 10 through 14 persistent family and the Round 1 through 9
  contracts those rounds claim to preserve;
- current namespace, system, config, permission, conflict, snapshot, fork,
  task, peer, sync, plugin, backup, repair, GC, Void, index, query, HTTP, CLI,
  and embedded producer/consumer paths;
- 93 Axum route registrations and 154 direct root/list/query call-site hits;
- 161 existing Rust `*_spec.rs` files plus the current Cargo target registry;
- the high-collision implementation files, including `storage_engine.rs`,
  `directory_ops.rs`, `query_engine.rs`, `index_store.rs`, `disk_kv_store.rs`,
  `gc.rs`, `btree.rs`, `batch_commit.rs`, `append_writer.rs`,
  `server/engine_routes.rs`, `server/mod.rs`, and `server/sync_routes.rs`; and
- the current error-suppression surface: 61 `let _ =`, 118 `.ok()`, 84
  `unwrap_or_default`, 133 `if let Ok`, 146 `Err(_)`, 199 `.unwrap()`, and 36
  `.expect()` production-source occurrences. These counts are candidates, not
  automatic defects; the plan must classify every one it touches and the
  repository-wide audit must classify the remainder.

Mechanical checks found balanced Markdown fences and internally correct
length arithmetic for the sampled RootAdmission, RootPublicationPrepare,
SemanticMutation, RootExpiry, SweepReceipt, and VoidClaim bodies. They also
found one real formula error: the stated `SweepProposal` fixed body is
`32 + 2H`, not `40 + 2H`.

### Gauntlet verdict before correction

The pre-Round-15 plan was **not implementation-ready**. The most serious issue
was a crash-consistency hole between logical root expiry and physical byte
reclamation. The remaining issues were persistent-format omissions,
classification gaps, coverage-journal contradictions, route omissions, and
phase/test plans too broad to execute safely.

| Severity | Finding | Consequence without correction |
| --- | --- | --- |
| Critical | Logical root retirement was represented only by physical-incarnation candidates. | A pending historical root's children could be reclaimed while it was readable, or a crash after locator removal could return missing/corrupt instead of deterministic `410`. |
| Critical | V4 migration depends on a physical-instance ID absent from the DatabaseHeader. | Cutover recovery could not disambiguate files when controls were missing or damaged. |
| High | Several controls called “exact” had prose-only bodies or ambiguous physical storage. | Independent readers, repair, and cross-platform fixtures could not be implemented from the plan. |
| High | The live descendant `.aeordb-permissions` and `/.aeordb-conflicts/` families were absent from the SystemFamily matrix. | GC, backup, migration, sync, indexing, and authorization could disagree about real persisted state. |
| High | The semantic mutation stream was both recoverable-soft and required in the hard authority batch. | The design could reintroduce synchronous indexing work and still fail to define gap recovery. |
| High | Root-aware API coverage omitted multi-root, hash/raw, plugin, share, and historical aggregate routes. | One request could still mix HEAD/historical state or expose content by hash knowledge. |
| Medium | `VoidClaim.state` implied mutation of an immutable content-addressed record. | Crash recovery could disagree about which claim version was authoritative. |
| Medium | Coverage used process-local `runtime_boot_id` terminology despite exact immutable root coverage. | Portable historical coverage and migration semantics were ambiguous. |
| Medium | Exact-hash converter collision behavior and aggregate APOS validation were underspecified. | Digest collisions or synthetic aggregate positions could produce false confidence or unbounded validation. |
| Medium | P2 and several later phases remained integration megaphases. | Workers would collide in the largest modules and rollback units would be too large. |

The rest of this round closes those findings. No deferred item below is an
owner-policy question.

## 2. Logical Root Lifecycle Is Separate from Physical Reclamation

Round 13's `expired_namespace_root_closure` physical candidate cannot also be
the authority that makes a root stop being readable. Logical roots and physical
incarnations have different identities, race guards, and crash boundaries.
They are now two state machines.

### 2.1 Permanent GC registry additions

Add these permanent `GcArtifactV1` IDs:

~~~text
0x0006 RootLifecycleActiveControl       stable A/B
0x0017 RootLifecycleManifest
0x0028 RootCandidatePage
0x0037 RootRetirementCommit
0x0038 VoidClaimSettlementReceipt
0x0039 RootObjectReclaimProof
~~~

Directory role `8 root_candidates` orders complete NamespaceRoot hashes.
`RootLifecycleActiveControl` uses the existing exact A/B control body and
targets `RootLifecycleManifest`. There are therefore six, not five, GC active
controls. Capability bit 17 is renamed from `RootExpiryV1` to
`RootLifecycleRetirementV1`; its reader obligation is to validate logical
lifecycle authority and return retired-root state, and its writer obligation is
to retire logically before omitting a closure from physical marking. No v4
writer exists yet, so these IDs and semantics do not reinterpret emitted bytes.

`QuarantineManifest` field `52 + 5H` is renamed from
`RootExpiryCatalogManifest` to `captured_root_lifecycle_manifest`. It records
the immutable lifecycle basis used by that physical mark but does not select
logical lifecycle authority. The manifest length remains
`100 + 6H + D*H`.

Identity/generation/cap rules are exact: lifecycle active-control identity is
`database_id[16] || slot u8`; lifecycle-manifest identity is
`database_id[16] || generation u64`; RootCandidate pages use the common
`database_id[16] || catalog_id[16] || role u16 || page_id u64` identity;
retirement identity is stated below; and settlement-receipt identity is
`database_id[16] || claim_id[16]`. Root-object-proof identity is
`database_id[16] || namespace_root H || proof_id[16]`. Manifest/page generation is the owning
lifecycle generation, retirement generation is its final complete mark, and
settlement generation is the selected result Void generation; root-object proof
generation is its final physical-inventory generation. All are nonzero.
Manifest, retirement, settlement, and root-object-proof bodies are capped at
1 MiB; candidate
pages retain the common 1 MiB target/16 MiB hard cap.

### 2.2 Exact logical lifecycle bodies

`RootLifecycleManifest` identity is `database_id[16] || lifecycle_generation
u64`. Its body is:

~~~text
flags u32 = 0
required_capabilities[32]
lifecycle_generation u64
published_at_ms i64
source_complete_mark_generation u64
authority_root_set_digest H
candidate_directory_root H or zero
root_expiry_catalog_manifest H or zero
next_candidate_page_id u64
candidate_count u64
pending_count u64
retired_evidence_count u64
candidate_logical_bytes u64
expiry_logical_bytes u64
~~~

The exact body length is `108 + 3H`. Generation must match the envelope.
Empty-root/count/byte/high-water invariants are canonical. Candidate and expiry
directories are independently optional.

One `RootCandidatePage` record is:

~~~text
namespace_root_hash H
state u8 = 1 pending_delete
flags u8 = 0
reason u16
pending_since_ms i64
first_unreachable_generation u64
last_confirmed_unreachable_generation u64
grace_at_pending_ms u64
authority_root_set_digest H
admission_commit_payload_hash H
~~~

The fixed length is `36 + 3H`; order is root hash. The pending time, grace,
first generation, authority digest, and admission evidence are immutable after
first publication; only `last_confirmed_unreachable_generation` advances on a
later complete mark. Reachability removes the row. An incomplete mark cannot
add, confirm, remove, or retire a root candidate.

`RootRetirementCommit` identity is
`database_id[16] || namespace_root H || retirement_id[16]`. Its body is:

~~~text
database_id[16]
namespace_root_hash H
retirement_id[16]
committed_at_ms i64
pending_since_ms i64
grace_at_pending_ms u64
final_mark_generation u64
reason u16
flags u16 = 0
reserved u32 = 0
prior_root_lifecycle_manifest H
authority_root_set_digest H
admission_commit_payload_hash H
~~~

The exact body length is `72 + 4H`. It references the prior lifecycle
manifest, never the manifest that will contain its own hash, so no content-hash
cycle exists. In this body, namespace root, prior lifecycle, authority-set, and
admission hashes are evidence/identity digests, not traversed GC edges. The
expiry record's `retirement_commit_hash` is the typed edge that retains the
commit. Otherwise the commit would retain its old pending lifecycle, which in
turn could retain the very root closure being retired.

The pre-writer `RootExpiryCatalogManifest` body is also replaced so mandatory
logical-retirement evidence is not confused with optional post-reclaim
evidence:

~~~text
flags u32 = 0
required_capabilities[32]
retention_ms u64
optional_byte_budget u64
directory_root H or zero
next_page_id u64
record_count u64
logical_bytes u64
mandatory_record_count u64
mandatory_logical_bytes u64
optional_record_count u64
optional_logical_bytes u64
oldest_retired_at_ms i64
newest_retired_at_ms i64
~~~

The exact body length is `124 + H`. Mandatory plus optional counts/bytes equal
the totals. Mandatory records are state 1 and may exceed the optional budget;
state 2 records are optional retention evidence and alone obey age/byte
compaction. Empty-root/count/byte/time/high-water invariants are canonical.

The `RootExpiryPage` record is replaced before any writer exists with:

~~~text
namespace_root_hash H
retired_at_ms i64
last_pending_since_ms i64
final_mark_generation u64
reason u16
state u8: 1 logically_retired, 2 physically_reclaimed
flags u8: bit 0 root-object reclaim proof present
reserved u32 = 0
retirement_commit_hash H
root_object_reclaim_proof_hash H or zero
evidence_expires_at_ms i64
~~~

The exact length is `40 + 3H`; order is root hash. State 1 requires a zero
proof and `evidence_expires_at_ms = 0`, meaning the record is not eligible for
age/budget eviction. State 2 requires the exact proof and a checked evidence
expiry no earlier than physical reclaim time. This prevents bounded evidence
compaction from reopening a logically dead root whose bytes still exist.

`RootObjectReclaimProof` body is:

~~~text
database_id[16]
namespace_root_hash H
retirement_commit_hash H
completed_at_ms i64
final_physical_inventory_manifest H
root_incarnation_set_digest H
root_incarnation_count u64
sweep_receipt_set_digest H
sweep_receipt_count u64
absence_proof_digest H
~~~

The exact body length is `40 + 6H`. The selected final inventory must be newer
than every named receipt and prove that no valid physical incarnation with the
NamespaceRoot logical key remains active, retired-but-resurrectable, orphaned,
or quarantined. Incarnation and receipt counts are nonzero. The set/receipt
digests are non-edge evidence over canonical
sorted identities. Every hash in this proof is a non-traversed evidence digest;
the RootExpiry record separately carries typed edges to both the retirement
commit and this proof. This prevents each retained expiry record from pinning a
large historical physical-inventory generation.
This proof concerns the NamespaceRoot entity itself, not every descendant in
its former closure. Descendants may drain through later physical batches, but
without any root-object incarnation they cannot reopen that historical view.

### 2.3 Correct transition and crash order

The lifecycle rules are now:

1. `live` roots are reached by current authoritative roots or durable pins.
2. A structurally admitted, non-authoritative root absent from a selected
   lifecycle candidate/expiry catalog is `retained`.
3. A complete mark may publish it as `pending_delete`, freezing pending time,
   grace, first generation, authority digest, and admission evidence.
4. Every pending root's entire namespace, semantic, definition, and admission
   closure remains a physical mark root. Pending never means physically dead.
5. A later complete mark may retire it only after grace, unchanged identity,
   no current authority, no task/request pin, and a final guard under the same
   root-state coordinator used by authority creation.
6. Retirement first writes the immutable retirement commit and new expiry
   catalog, removes the candidate, and hard-publishes the new lifecycle control.
   That publication is the instant new admissions begin returning
   `ROOT_EXPIRED`; no locator or byte is removed in this transaction.
7. A crash before that control leaves the root pending/readable. A crash after
   it leaves the root retired/non-readable but physically intact. Recovery can
   leak space, but cannot reopen it.
8. Only later complete physical marks omit the retired closure. Each exact
   incarnation then independently enters the existing two-complete-mark,
   frozen-grace physical quarantine before a sweep may remove its locator or
   expose a Void extent.
9. After all physical incarnations of the NamespaceRoot object have durable
   receipts and a later complete inventory proves their absence, the immutable
   RootObjectReclaimProof is published. A new lifecycle/expiry generation then
   advances the record to `physically_reclaimed` and starts bounded evidence
   retention. A crash before this update keeps stronger indefinite logical
   evidence.

Request and authority creation both acquire a root guard, establish their pin
or intended authority, and recheck lifecycle state. A request/authority that
wins keeps the root; retirement that wins rejects the newcomer. No “pull back
from quarantine” path is needed after retirement because retirement is the
linearization boundary.

Before bit 17 activation, v4 contains no retirement state and valid former
roots are conservatively retained. Activation hard-publishes an empty valid
lifecycle first and then sets the monotonic capability. Once set, missing or
corrupt lifecycle authority disables destructive GC and blocks non-live
historical admission with a typed historical/lifecycle failure; it never
silently classifies a retired-looking root as retained. Current closure-valid
authoritative roots remain readable.

After a physical sweep/recovery, successful, changed, and reachable candidate
identities are cleared by a new compacted candidate map or delta. Failed,
pinned, or policy-skipped candidates remain with their original pending state.
Crash recovery performs the same idempotent clear from the receipt before a
batch can be proposed again.

## 3. Physical GC and Void Corrections

### 3.1 Sweep formula

The `SweepProposal` fields sum to:

~~~text
flags/codec/reserved       8
created_at_ms              8
quarantine manifest        H
mark generation            8
count/records length       8
proposal digest            H
                            = 32 + 2H
~~~

Its exact body length is therefore
`32 + 2H + count*(24 + 2H)`, not `40 + 2H + ...`.

### 3.2 Immutable Void claims

`VoidClaim` is immutable reservation evidence and no longer carries a mutable
`state`. Its exact body is:

~~~text
flags u32 = 0
claim_codec u16 = 1
reserved u16 = 0
created_at_ms i64
requesting_boot_id[16]
requesting_task_or_batch_id[16]
source_void_manifest_hash H
extent_count u32 (1..4,096)
extents_length u32
sorted extent records
~~~

The fixed length remains `56 + H`. Presence in the selected Void catalog means
outstanding. Settlement/abandonment is represented by omission from a later
hard-published Void catalog, never by another immutable claim with the same
logical ID.

`VoidClaimSettlementReceipt` identity is
`database_id[16] || claim_id[16]`; body:

~~~text
flags u32: bit 0 recovered
outcome u16: 1 settled, 2 recovered, 3 abandoned_to_quarantine
reserved u16 = 0
settled_at_ms i64
source_void_manifest_hash H
result_void_manifest_hash H
used_extent_count u32
unused_extent_count u32
used_bytes u64
returned_bytes u64
evidence_digest H
~~~

The body length is `40 + 3H`. The result manifest omits the claim and returns
only proven unused subranges. Unexplained bytes go through inventory and normal
quarantine. The selected Void catalog is allocator authority; the receipt is
idempotent audit/recovery evidence and enters ordinary evidence retention.
Its source/result manifest and evidence hashes are non-traversed evidence
digests, not GC edges; otherwise a bounded audit receipt would accidentally pin
old allocator generations. Recovery trusts only the selected active Void
control and independently revalidates any still-present evidence target.

## 4. DatabaseHeader V4 Has Real Migration Identity and Breathing Room

The gauntlet rejects shipping a brand-new v4 header with only 44 spare bytes.
This migration is already side-by-side, so now is the inexpensive point to
reserve space and place physical identity in its natural authority record.

Each v4 DatabaseHeader slot is **1,024 bytes**, and the data region begins at
offset 2,048. Offsets 0 through 463 retain the approved Round 2/Round 14 fields.
The tail is:

| Offset | Size | Field | V4 rule |
| ---: | ---: | --- | --- |
| 464 | 16 | `physical_instance_id` | Random nonzero ID for this physical database file |
| 480 | 540 | reserved | All zero |
| 1020 | 4 | `slot_crc32` | CRC-32/ISO-HDLC over bytes 0..1019 |

`slot_length` at offset 5 is exactly `1024`. A byte-for-byte evidence/backup
copy necessarily retains the physical ID and is diagnostic/read-only while the
original may exist. Before a copied file is adopted as an independently
writable database, clone/restore admission hard-publishes a new physical ID and
writer fence in both slots while preserving `database_id`; an uncontrolled raw
copy may not bypass that adoption. A logical clone, restore, replica, or
v3-to-v4 destination therefore gets a new ID. The v3 source has no such header
field; migration derives and records a source evidence identity from its
verified file identity without modifying v3. Every destination/cutover check
reads the v4 physical ID from the header, not only from a mutable control.

All Round 2, Round 14, P0 fixture, cutover, size, backup-header, and “two
512-byte slots/data start 1024” text is superseded by this section. V4 remains
format version 4 because no v4 writer has shipped.

## 5. System Controls Have One Discoverable Physical Representation

The phrase “existing root-only system-record mechanism” and the direct
`aeordb.system-control-key` formula left control storage ambiguous. V1 freezes
one representation:

~~~text
/.aeordb-system/controls/v1/
  <control-kind-4-lowercase-hex>/
  <blake3(identity)-64-lowercase-hex>/
  a.ctrl | b.ctrl | i.ctrl
~~~

- Mutable controls use `a.ctrl` and `b.ctrl`; immutable controls use only
  `i.ctrl` with `control_sequence = 1`.
- Identity digest is
  `BLAKE3("aeordb.system-control-identity.v1\0" || kind u16 LE ||
  identity_length u16 LE || identity)`.
- Each file is an ordinary v1 FileRecord with `FLAG_SYSTEM` and content type
  `application/vnd.aeordb.system-control`; its complete content is the Round 14
  control payload. The payload repeats and validates the full identity, so a
  digest collision is corruption, not aliasing.
- The prior custom canonical-KV-key formula is superseded. Normal FileRecord
  path keys, the system tree, and `LocatorReplacementCoordinator` own physical
  publication/retirement. Only `ControlStore` may write this subtree.
- The protected subtree is enumerable by ordinary strict system traversal,
  physical inventory, verify, repair, backup policy, and migration. Generic
  file APIs, sync clients, plugins, and indexing cannot mutate it.
- ControlStore suppresses recursive semantic-mutation generation for its own
  post-commit detail records only through a reviewed typed family rule, not a
  path-string exception.

`RootAdmissionCommit` is high-churn/collectable despite sharing this codec. It
is one immutable file per distinct admitted root, is rooted with that root's
closure, and is removed only after logical retirement plus physical
quarantine. P0 must measure this write/directory amplification; it may page the
system directory internally without changing the public identity/path
contract. It must not be described as “low volume.”

### 5.1 Missing control bodies are now exact

All integers below are little-endian. Optional fixed hashes/IDs are all-zero
only when the corresponding flag/state permits absence. Canonical config
payloads reject trailing bytes. The common control framing and per-kind caps
remain Round 14 authority.

#### `RepairTicket`

~~~text
database_id[16]
ticket_id[16]
created_at_ms i64
updated_at_ms i64
state u16: 1 open, 2 repairing, 3 resolved, 4 superseded
corruption_class u16
authority_family_id u16
flags u16: bit 0 root present, bit 1 path present, bit 2 incarnation present
affected_namespace_root H or zero
canonical_path_digest H or zero
incarnation_present u8
reserved[7] = 0
PhysicalIncarnationIdV1 or all zero
evidence_count u16 (0..64)
reserved u16 = 0
evidence_length u32 = evidence_count * H
diagnostic_length u32
reserved u32 = 0
sorted unique evidence ArtifactHashes
CanonicalConfigValueV1 diagnostic
~~~

Fixed length is `104 + 4H`; total is
`104 + 4H + evidence_count*H + diagnostic_length`, capped at 1 MiB. Ticket and
evidence identity are immutable across A/B state revisions.

#### `MigrationLease`

~~~text
database_id[16]
migration_id[16]
source_physical_instance_id[16]
destination_physical_instance_id[16]
holder_boot_id[16]
fencing_token u64
acquired_at_ms i64
renewed_at_ms i64
expires_at_ms i64
source_header_sequence u64
state u16: 1 held, 2 releasing, 3 released, 4 expired
source_format u16 = 3
destination_format u16 = 4
flags u16 = 0
reserved u32 = 0
~~~

Fixed length is 132 bytes. Times are nonnegative; expiry is greater than
renewal for held leases. A new holder uses a strictly larger fencing token.

#### `MigrationProgress`

~~~text
database_id[16]
migration_id[16]
source_physical_instance_id[16]
destination_physical_instance_id[16]
fencing_token u64
source_format u16 = 3
destination_format u16 = 4
phase u16: 1 preflight, 2 copy, 3 reconcile, 4 final_freeze,
           5 destination_verify, 6 cutover, 7 read_only_validation,
           8 operator_acceptance
state u16: 1 pending, 2 running, 3 paused, 4 complete, 5 failed, 6 canceled
flags u32: bit 0 source GC suspended, bit 1 source write freeze held,
           bit 2 destination full-verified
source_header_sequence u64
destination_header_sequence u64
copied_through_write_sequence u64
captured_through_publication_sequence u64
reconciled_through_publication_sequence u64
namespace_count u64
entity_count u64
copied_bytes u64
updated_at_ms i64
source_capture_head H or zero
checkpoint_artifact H or zero
legacy_root_map_control_payload_hash H or zero
effective_config_fingerprint H
system_family_registry_fingerprint H
last_error_evidence H or zero
~~~

Fixed length is `156 + 6H`; cap 4 KiB. State/phase/flag combinations are
checked. Counters and watermarks never decrease under one fencing token.

#### `LegacyRootMapPage`

The page body is exactly:

~~~text
database_id[16]
migration_id[16]
logical_database_id[16]
source_physical_instance_id[16]
destination_physical_instance_id[16]
page_ordinal u64
previous_page_hash H or zero
next_page_hash H or zero
row_count u32
rows_length u32 = row_count * (12 + 2H)
sorted rows
~~~

Fixed length is `96 + 2H`; total is
`96 + 2H + row_count*(12 + 2H)`, capped at 1 MiB. Ordinals begin at zero,
links are bidirectionally consistent, and the control's count/first/last/digest
cover the complete chain.

#### `DurabilityLatch`

~~~text
database_id[16]
latch_generation u64
first_failure_at_ms i64
latest_failure_at_ms i64
severity u16: 1 serious
state u16: 1 read_only_latched, 2 repair_verifying, 3 cleared
failed_operation u16
os_error_class u16
os_error_code i32
flags u32: bit 0 spill catalog present
last_selected_header_sequence u64
last_durable_write_sequence u64
last_durable_publication_sequence u64
emergency_spill_catalog_payload_hash H or zero
evidence_digest H
diagnostic_length u32
reserved u32 = 0
redacted CanonicalConfigValueV1 diagnostic
~~~

Fixed length is `88 + 2H`; total is `88 + 2H + diagnostic_length`, capped at
1 MiB. A clear transition requires the explicit repair/probe evidence already
approved; deleting the control or restarting does not clear the latch.

#### `EmergencySpillCatalog`

~~~text
database_id[16]
catalog_generation u64
discovered_at_ms i64
state u16: 1 discovered, 2 replaying, 3 complete, 4 aborted
flags u16 = 0
row_count u32
rows_length u32
repair_receipt_hash H or zero
rows
~~~

Fixed length is `44 + H`. Each row is:

~~~text
source_location_class u16: 1 OS user data, 2 configured fallback, 3 temp fallback
replay_state u16: 1 pending, 2 applied, 3 duplicate, 4 quarantined, 5 failed
path_encoding u16: 1 Unix raw bytes, 2 Windows UTF-16LE
flags u16 = 0
created_at_ms i64
creation_sequence u64
file_length u64
BLAKE3 complete-file digest[32]
path_length u32
reserved u32 = 0
native path bytes
~~~

Row fixed length is 72 bytes. Rows sort by creation time, sequence, digest, and
path bytes. Paths are no-follow revalidated before use and never transferred as
portable database authority.

#### `SideBySideCutoverControl` and external `cutover.acut`

The common database control body is:

~~~text
database_id[16]
migration_id[16]
logical_database_id[16]
source_physical_instance_id[16]
destination_physical_instance_id[16]
fencing_token u64
state u16: 1 copying, 2 final_freeze, 3 destination_verified,
           4 source_renamed_to_backup, 5 destination_installed,
           6 reopened_verified, 7 complete, 8 rolled_back
flags u16 = 0
source_format u16 = 3
destination_format u16 = 4
reserved u32 = 0
source_header_sequence u64
destination_header_sequence u64
source_expected_size u64
destination_expected_size u64
updated_at_ms i64
source_file_identity H
destination_file_identity H
source_path_digest H
destination_path_digest H
last_error_evidence H or zero
~~~

Fixed length is `140 + 5H`.

Path and file identities are not ad hoc strings:

~~~text
path_digest =
  H("aeordb.cutover-path.v1\0" || path_encoding u16 || native path bytes)

file_identity =
  H("aeordb.cutover-file-evidence.v1\0" ||
    PlatformFileIdentityDescriptorV1 || format u16 ||
    logical_database_id || physical_instance_id_or_evidence_id[16] ||
    exact selected header slot bytes || expected_size u64 ||
    final durable write sequence u64)
~~~

`PlatformFileIdentityDescriptorV1` is exactly 56 bytes: platform `u16`
(`1 Unix`, `2 Windows`), schema `u16 = 1`, flags `u32` (bit 0 generation
present, bit 1 birth present), volume identity `[16]`, file identity `[16]`,
and birth identity `[16]`. Unix volume identity is the first 16 BLAKE3 bytes of
canonical `st_dev || statfs fsid`; file identity is `st_ino u64 || inode
generation u64-or-zero`; birth identity is signed seconds/nanoseconds or zero.
Windows uses volume serial/identity zero-padded to 16, the 128-bit
`FILE_ID_INFO` file ID, and creation `FILETIME u64` plus eight zero bytes.
Unavailable flagged fields are zero. The v3 source receives a random nonzero
16-byte migration evidence ID stored in every cutover control; v4 uses its
header physical ID. Unsupported
stable file identity blocks online cutover and permits only an explicit offline
copy procedure. It is same-host recovery evidence, not portable database
identity. It is recomputed after every rename/reopen, which detects path
replacement without requiring a multi-terabyte full-file hash while writes are
frozen.

Descriptor bytes remain exact journal evidence, but same-physical-file
comparison follows the native identity contract rather than raw descriptor
equality. On Unix schema 1, volume, file/generation, and available birth
evidence must all match to guard against inode reuse. On Windows schema 1,
volume identity plus `FILE_ID_INFO.FileId` is the same-file key; birth remains
recorded evidence but is not part of same-file comparison because
`ReplaceFileW` preserves the replaced destination's creation time while the
resulting file retains the replacement file's file ID. Cutover recomputes and
records the complete descriptor after each reopen/replace boundary.

The external journal is exactly 2,048 bytes: two independent 1,024-byte slots.
Each slot has `ACUT` at 0, schema `u16 = 1` at 4, slot length `u16 = 1024` at
6, slot sequence `u64` at 8, body length `u32 = 140 + 5H` at 16, flags zero at
20, eight reserved zero bytes at 24, the identical body at 32, zero padding
through byte 1019, and CRC-32/ISO-HDLC at 1020 over bytes 0..1019. A/B
selection uses the same equal-sequence ambiguity rule as database controls.

### 5.2 Shared persistent enum registries

Zero is invalid unless named `none`; unassigned values are reserved and never
reused. These registries close the generic `stable_reason`, corruption, audit,
task, mutation, retry, and durability fields used by Rounds 13 through 15:

~~~text
StableReasonV1
0x0000 none/success where the owning state permits it
0x0001 requested
0x0002 source_changed
0x0003 incomplete_coverage
0x0004 dependency_unavailable
0x0005 unsupported_definition_or_format
0x0006 corrupt_derived_artifact
0x0007 corrupt_authority
0x0008 resource_admission
0x0009 canceled
0x000A shutdown
0x000B retryable_io
0x000C permanent_io
0x000D stale_fence
0x000E invalid_configuration
0x000F root_unavailable
0x0010 rebuild_required
0x0011 repair_required
0x0012 migration_reset
0x0013 capture_gap
0x0014 policy_disabled
0x0015 integrity_mismatch
0x0016 uncertain_completion
0x0017 collision_alarm
0x0018 unknown_protected_family

RetryClassV1
0 none, 1 immediate, 2 bounded_backoff, 3 after_dependency,
4 after_repair, 5 never

RootRetirementReasonV1
1 ordinary_gc_unreachable, 2 explicit_operator_retirement

RepairCorruptionClassV1
1 entity_framing, 2 entity_integrity, 3 locator_missing,
4 locator_identity_mismatch, 5 btree_missing_node,
6 btree_malformed_node, 7 btree_cycle_or_order,
8 authority_missing_or_ambiguous, 9 semantic_closure,
10 system_family_incomplete, 11 physical_overlap_or_bounds,
12 control_ambiguous_or_corrupt, 13 unsupported_required_format

GcAuditEventKindV1
1 mark_started, 2 mark_completed, 3 mark_canceled,
4 root_pending, 5 root_retired, 6 sweep_proposed,
7 sweep_committed, 8 sweep_recovered, 9 void_claimed,
10 void_claim_settled, 11 void_claim_recovered,
12 corrupt_evidence, 13 retention_compacted, 14 gc_disabled

GcRunKindV1
1 mark, 2 sweep, 3 physical_inventory, 4 audit_compaction,
5 void_reconcile, 6 root_lifecycle_reconcile

GcOutcomeV1
1 complete, 2 canceled, 3 failed, 4 skipped, 5 recovered

GcErrorClassV1
1 framing, 2 checksum, 3 bounds_or_overlap, 4 missing_edge,
5 wrong_identity, 6 ambiguous_control, 7 incomplete_authority_walk,
8 workspace_tamper_or_loss, 9 policy_unavailable, 10 unsupported_codec

AuditPinReasonV1
1 operator_investigation, 2 repair_evidence, 3 migration_verification,
4 support_bundle

TaskKindV1
1 index_build, 2 reindex, 3 gc_mark, 4 gc_sweep,
5 physical_inventory, 6 backup_export, 7 backup_import,
8 format_migration, 9 repair, 10 compaction, 11 peer_sync

MutationOperationV1
1 create, 2 replace, 3 delete, 4 activate, 5 deactivate,
6 promote, 7 restore, 8 reconcile, 9 retire, 10 repair

DurabilityOperationV1
1 dependency_append, 2 data_barrier, 3 authority_write,
4 authority_barrier, 5 authority_readback, 6 header_ab,
7 control_ab, 8 parent_directory_sync, 9 durable_replace,
10 preallocation, 11 void_claim, 12 emergency_spill,
13 cutover_journal, 14 cutover_rename, 15 shutdown_flush

OsErrorClassV1
1 interrupted_no_progress, 2 no_space, 3 quota, 4 read_only,
5 permission, 6 media_io, 7 device_lost, 8 invalid_handle,
9 unsupported_durability, 10 checksum_readback, 11 short_write,
12 timeout_unknown, 13 other_persistent_io
~~~

`IndexRegistryControl`, `IndexOperationControl`,
`IndexDegradedControl`, `content_only_reason`, sweep outcome detail, and other
generic `stable_reason u16` fields use `StableReasonV1`. `retry_class` uses
`RetryClassV1`; root candidate/retirement/expiry reason uses
`RootRetirementReasonV1`; repair tickets and corrupt-GC evidence use the named
corruption/error registries; audit detail/summary/pins use the GC audit/run/
outcome/pin registries. Unknown values reject the owning persisted record.

## 6. System Families and Index Visibility Are Complete

The selected SystemFamily registry gains two live families discovered by the
territory pass:

| ID | Canonical family/matcher | Sem | GC | Logical backup | Peer | Data/client | Rule |
| ---: | --- | :---: | :---: | :---: | :---: | :---: | --- |
| `0x0019` | exact `/.aeordb-permissions` plus descendant exact file/link `.aeordb-permissions` | - | T | I | I | I under normal path auth | Authorization metadata remains user-visible/index-visible under existing behavior; malformed state fails affected authorization closed. |
| `0x001A` | exact `/.aeordb-conflicts` plus prefix `/.aeordb-conflicts/` | - | Q | I | O | O except typed `/sync/conflicts` APIs | Unresolved conflict state survives logical backup/migration, is not recursively peer-replicated as foreign conflict authority, and malformed metadata cannot be silently dismissed. |

Family `0x0043` explicitly includes the complete
`/.aeordb-system/controls/v1` exact root/prefix subtree and every Round 14
control kind. The
existing permissions family `0x0012` remains the centralized hashed permission
store; it does not replace descendant `.aeordb-permissions` links/files.

The previously reserved descriptor byte at offset 21 is now:

~~~text
IndexPolicy:
0 NotApplicable
1 IncludeUnderOrdinaryScope
2 ExcludeFromAllIndexes
3 CanonicalProjectionOnly
~~~

Offset 22 remains two reserved zero bytes. Descriptors sharing a family ID must
share IndexPolicy. The semantic projection fingerprint includes matcher,
semantic role, and IndexPolicy. Ordinary unmatched user data defaults to
`IncludeUnderOrdinaryScope`; `.aeordb-permissions` explicitly preserves that
policy; controls, conflicts, credentials, GC, logs, and operational system
state are excluded; parser/index config sources are canonical projections, not
ordinary documents.

Corrected scope/internal policy therefore calls the selected registry and no
longer owns a second hard-coded list. The approved v1 byte/NUL/glob semantics
remain unchanged. The old explicit `.aeordb-system`/`.aeordb-config`/
`.aeordb-indexes`/`.aeordb-logs` predicate is retained only as the named v0
migration evaluator used to reproduce old IDs. P0's registry completeness gate
enumerates every protected path literal, EntryType, KV domain, control kind,
and external workspace in source and requires exactly one selected family or
an explicit ordinary-user-data classification.

## 7. Coverage and Mutation Semantics Do Not Block User Commits

The three correctness-bearing index manifest prefixes retain their exact
widths but receive portable names:

~~~text
source_head_hash       -> source_namespace_root
runtime_boot_id[16]    -> coverage_epoch_id[16]
runtime_sequence u64   -> coverage_publication_sequence u64
~~~

`coverage_epoch_id` is a durable nonzero build/reconciliation lineage ID, not a
process identity. `coverage_publication_sequence` is a destination-local
catch-up assertion. Exact byte-identical `source_namespace_root` plus exact
definition/dependency identity is the correctness boundary. The same renames
apply to `IndexRegistryControl.coverage_runtime_sequence` and all coverage
comparisons. Formulas do not change.

The semantic mutation stream is **recoverable soft state**:

1. The hard authority transaction allocates and durably publishes its global
   publication sequence, namespace/semantic/control authority, and required
   root-admission state.
2. Only after that hard commit does the coordinator append the typed mutation
   record and fan out cache, SSE, index, and diagnostic work.
3. Loss or a gap never means “nothing changed.” A consumer compares its exact
   source NamespaceRoot/control identities and publication watermark with the
   selected authority and reconstructs missing work by immutable root/system-
   family diff. It may rebuild when bounded diff is unavailable.
4. An index manifest is exact only for the NamespaceRoot it names. Partial
   generation use follows the already-frozen exact-covered-plus-authoritative-
   scan proof; otherwise fallback/rebuild is mandatory.
5. Migration capture uses the same journal only as acceleration. Final freeze
   reconciles exact source namespace, semantic, protected-family, snapshot,
   fork, and approved persisted-root state; a missing segment is never treated
   as an empty delta.

This supersedes “write authoritative state and mutation record in one
durability batch,” “emit mutation before HEAD visibility,” and the process-
local `control_epoch` as durable coverage. Config/plugin capture may still use
an in-process epoch as a race detector, but persisted identities and final
recheck are authority. No user write waits for parser execution, posting/NVT
mutation, index-registry publication, or a soft journal sync.

## 8. Exact Equality, APOS, and Read-Route Corrections

### 8.1 Exact converter collision boundary

`typed_exact_blake3_v1` emits this 33-byte posting key:

~~~text
CanonicalSourceValueV1 type tag u8 ||
BLAKE3("aeordb.typed-exact-posting.v1\0" || complete CanonicalSourceValueV1)
~~~

Point lookup uses it only to find candidates. Every `eq`/`in` candidate is
rechecked against the complete canonical source value in the pinned
ValueStore/RecordRevision. A digest collision can add scan work but cannot
produce a false match. This converter never serves range/order semantics. P0
adds a collision-injection oracle that substitutes a test digest function and
proves exact recheck.

### 8.2 APOS validation is route-specific

File/list/query/search positions resolve the FileKey and RecordRevision in the
selected authorized result universe and recompute the complete tuple.
Aggregate positions instead validate the canonical group tuple, its synthetic
group hash in the FileKey slot, and the exact aggregate input-root identity in
the revision slot, then seek by the aggregate tuple. They do not attempt to
resolve a synthetic FileKey as a file and do not scan all prior groups merely
to validate a token.

The phrase “legacy cursor transition” in P7 is removed. The approved
coordinated cutover/no-dual-decoder policy stands; old in-flight tokens may
fail.

### 8.3 One explicit route matrix

Every route and embedded equivalent is classified at build/test time:

| Class | Surfaces | Root contract |
| --- | --- | --- |
| Single-root namespace | `GET/HEAD /files`, `GET/HEAD /files/*`, query, search, fetch, download, symlink read/resolution, file-reading plugin invocation, authorized portal/share listing/file reads, and embedded equivalents | Omitted selector captures HEAD once. Supplied `root_hash` resolves one `ResolvedReadView`. Every successful shape returns root metadata/headers after authorization. |
| Historical aliases | Existing `snapshot` and `version` selectors on file/version reads | `root_hash`, `snapshot`, and `version` form one selector union; more than one is `INVALID_ROOT_SELECTOR`. Snapshot/version resolve through the same root resolver and migration map. No precedence guessing. |
| Multi-root | sync diff, backup diff, version/file history, comparisons | Use named `from`/`to`/`since`/`current` roots and return a root set or per-result root. They never manufacture one misleading response root. |
| Content staging | blob config/check/chunk PUT and sync chunk transport | No namespace root selector; these are authenticated staged-content/protocol operations and do not prove namespace reachability. |
| Hash retrieval | `GET /blobs/{hash}` | FileRecord retrieval requires a selected root and proof that its stored path resolves to that exact revision under current authorization. Raw Chunk/Directory/internal entry retrieval is root/admin diagnostic-only; ordinary hash knowledge is not read authority. |
| Operational/auth/system | health, metrics, users, groups, keys, config, tasks, GC, repair, peers, plugin deploy/list | No namespace selector unless a typed operation explicitly names source roots. |
| Mutation | PUT/PATCH/DELETE/copy/rename/merge/blob commit/sync apply/restore/promote | Reject generic `root_hash`. Historical source selection uses a route-specific `source_root_hash`/snapshot field and creates a new current root through the mutation coordinator. |

POST selectors live in JSON and GET selectors in query parameters. `after` or
`before` still requires explicit `root_hash`; its APOS root must match. Root
existence/state, timing, counts, errors that distinguish hidden objects, and
root metadata remain behind each route's existing concealment policy.

Historical selection never resurrects historical authority. Current account,
group, API-key, share-link, revocation, and centralized permission state is
validated first. Descendant `.aeordb-permissions` from the selected root may
further restrict that view but may not expand current grants; effective access
is the intersection. A normal share credential captures current HEAD but may
not submit an explicit non-live `root_hash`; historical sharing requires a
future explicit share policy/record rather than inheriting accidental access to
old file contents. JWT/API callers with current historical path authority may
select retained roots normally.

P7 must generate an architecture fixture from all 93 route registrations. A
new route fails CI until it declares one class, selector schema, authorization
owner, response-root shape, and `ResolvedReadView`/no-root proof. Direct
`head_hash`, historical tree walk, or raw hash fetch in a namespace handler is
a forbidden bypass.

## 9. Migration Is Complete Across Producers and Capacity Failure

Online capture and final reconciliation cover every authoritative producer,
not only regular files: files/directories/symlinks/deletes, system and auth
families, config and semantic objects, snapshots/forks, sync bases and peer
state according to policy, tasks/pins, conflicts, plugins, restore/promote,
repair, import, and maintenance controls. Node-local state is explicitly reset
or destination-created; it is never silently omitted.

Add a strict `migration` object to `runtime.json`:

| Property | Default | Range/behavior |
| --- | ---: | --- |
| `migration.capture_max_bytes` | 64 GiB | 1 GiB..4 TiB |
| `migration.capture_free_reserve_bytes` | `max(16 GiB, min(128 GiB, 5% filesystem capacity))` | 1 GiB..50% filesystem |
| `migration.checkpoint_after_seconds` | 300 | 30..3,600 |

The complete runtime GC registry also gains the bounded lifecycle properties
that Round 13 said Round 14 would freeze but Round 14 omitted:

| Property | Default | Range/behavior |
| --- | ---: | --- |
| `garbage_collection.root_expiry_retention_seconds` | 2,592,000 (30 days) | 3,600..315,576,000 |
| `garbage_collection.root_expiry_max_bytes` | 256 MiB | 1 MiB..16 GiB; applies only to physically reclaimed evidence |
| `garbage_collection.root_lifecycle_hard_max_bytes` | 1 GiB | 64 MiB..64 GiB; includes pending candidates and mandatory logically-retired evidence |

Logically retired evidence is mandatory until physical reclaim and is outside
the optional expiry age/byte budget. If adding a candidate or retirement would
cross the lifecycle hard cap, the engine retains the root, publishes no new
retirement, reports `root_lifecycle_capacity_blocked`, and continues work that
can clear/reclaim existing state. It never evicts mandatory evidence. Logical
candidate admission is deterministic root-hash order and adds only complete
rows that fit; deferred roots remain retained and remain physical mark roots.
Capacity deferral does not make reachability results partial and never makes a
physical candidate eligible sooner. Logical
retirement runs only while destructive GC is enabled and its lifecycle/physical
authority is healthy; when GC is disabled, former roots remain retained as
already approved. Candidate eligibility continues using
`pending_since + max(grace_at_pending, current configured grace)`.

Health, metrics, CLI status/verify, administrative SSE, and the Dashboard expose
retained/pending/logically-retired/physically-reclaimed counts and bytes,
mandatory versus optional evidence bytes, oldest pending/retired age, selected
lifecycle generation, active mark basis, cap utilization, capacity-blocked
state, and reclaim progress/ETA. These are bounded manifest/control reads, not
an on-request full root scan.

These are next-run captured and use the existing generated environment/CLI
name rule, including `AEORDB_MIGRATION_*` / `--migration-*`; this adds
`migration` to Round 14's complete runtime namespace list. Capture is bounded
disk state, not process memory. Reaching a limit
does not fail or roll back an acknowledged source write. It durably marks the
shadow migration `needs_full_reconcile`, stops optional capture, and either
performs a complete source authority/family diff during final freeze or aborts
the shadow migration. Critical free-space pressure aborts/cleans the identified
destination workspace while preserving source authority. Source GC suspension
remains durably owned by the lease and is released on success, cancel, or
verified rollback.

The destination preserves HEAD, snapshots, forks, approved retained roots,
semantic/config closure, protected families, symlinks, deletions/history
required by those roots, and exact content/chunks. Active source maintenance
tasks are drained or canceled under fencing; only explicitly portable task
state is reconstructed. Persisted peer/sync cursors that cannot be mapped get a
typed full-resync reset. Unknown externally held v3 hashes receive the already-
approved reset/unavailable behavior; migration never guesses a mapping.

Cutover preflight budgets destination, workspace, capture, preserved v3 backup,
and free-space reserve separately. Source checksum/size/header remain unchanged
before rename, and source GC suspension/capture state is visible in health,
status, verify, and the operation ledger.

Round 14's automatic filesystem reserve expressions are also corrected so a
default cannot violate its own maximum on a small volume:

~~~text
gc mark scratch reserve =
  clamp(max(8 GiB, min(64 GiB, 2% capacity)), 1 GiB, floor(capacity/2))

migration capture free reserve =
  clamp(max(16 GiB, min(128 GiB, 5% capacity)), 1 GiB, floor(capacity/2))
~~~

If capacity is below 2 GiB, the dependent operation is unavailable with a
typed capacity diagnostic instead of inventing an out-of-range default.

The root-only configuration routes are exact: `GET`, full-replacement `PUT`,
and RFC 7396 `PATCH` exist for both `/system/runtime` and
`/system/lifecycle`. PUT/PATCH validate the complete resulting stored document
before one coordinated publication; unknown/duplicate/invalid fields fail and
leave the prior file/LKG/effective state unchanged. Responses return persisted
validity, complete effective values and sources, pending-restart/convergence,
LKG identity, and disabled capabilities. Environment/CLI overrides remain
effective but are never written into the stored JSON. The embedded SDK, CLI,
Dashboard, API docs, and `SKILL.md` expose the same contract.

## 10. Executable Phasing and Ownership Corrections

The dependency graph remains, but these landing units replace oversized phase
descriptions:

~~~text
Pre-P0 owner ratification -> formal parent/child generation -> generated-plan review

P0a inventory/baseline/intended-divergence ledger
P0b independent format/reference tools and hand-authored fixture manifest
P0c machine contract registry and final freeze evidence

P1a bounded readers and malformed fixtures
P1b capability/SystemFamily admission
P1c platform durability probes and ControlStore read-only selection

P2a v3 DurabilityCoordinator/latch/spill
P2b ConfigResolver + MemoryCoordinator + diagnostics
P2c SystemFamily classification and strict traversal/error boundary
P2d NamespaceMutation/LocatorReplacement/event-metric facades
P2e producer migration waves:
    core DirectoryOps -> blob/batch -> version/backup/sync ->
    system/plugin -> maintenance/repair
P2f repository error-squelch classification and residual architecture gate

P3a v4 header/entity/control shadow writers
P3b semantic roots/read resolver/admission/lifecycle readers
P3c clone/capture/root-map/cutover shadow tooling

P4 physical inventory + logical lifecycle + mark -> sweep/Void -> repair UI
P5 definitions/converters -> pages/directories -> sparse NVT
P6 soft mutation/coverage -> bounded workers/cache -> activation/fallback
P7 route matrix/APOS -> planners/locators -> web/SDK/docs coordinated cutover
P8 rehearsal -> copied-production proof -> canary -> operator cutover
P9 retirement/docs/evidence/debt removal
~~~

Each arrow is a green, pushed, revertable snapshot. P5 may begin after P3a's
registries/common codecs are frozen; P4 also requires P3b's selected-root and
logical-lifecycle contracts/readers. P4 and P5 may overlap only after those
respective prerequisites are green. P2 producer
waves, P3, P6, and P7 are not assigned concurrently across
`storage_engine.rs`, `directory_ops.rs`, `server/engine_routes.rs`,
`server/mod.rs`, `query_engine.rs`, `index_store.rs`, common errors, Cargo
targets, or fixture registries. One integration owner owns each hotspot and
hands it off at a recorded commit.

P2 may persist only the already-needed v3-compatible LKG/diagnostic,
durability-latch, spill-catalog, repair-ticket, and path-latch control payloads
as system-flagged v0 FileRecords at the new ControlStore paths. It does not emit a
v4 header, NamespaceRoot, RootAdmission, GC, Void, or index control. P3 converts
those transition records byte-for-byte into v4 FileRecord framing after strict
validation. Because a pre-P2 binary would ignore a persistent read-only latch,
the checked deploy/install script must inspect transition controls and refuse a
downgrade while any active latch/spill/repair state exists. Manual use of an
older writer in that state is explicitly unsupported and prominently diagnosed;
rollback uses a P2-compatible binary until repair clears the state. “V3 bytes
remain readable” is not permission for an old writer to bypass new safety
authority.

Before formal generation, rebase/merge the latest `development`, rerun the
producer/consumer and protected-literal inventories, and record drift as an
absorb/roadmap addendum. Repeat at each phase entry. A changed route, family,
writer, persistent caller, or recent hardening fix cannot be dismissed as
“baseline drift.”

## 11. AGIS Test Protocol Applied to Every Phase

### 11.1 Core hypotheses and disproof oracles

| Hypothesis | Minimal falsifying oracle |
| --- | --- |
| Acknowledged authority survives every modeled crash. | Interrupt before/after each dependency barrier, authority write, inactive-slot publication, read-back, and waiter wake; reopened state is old/new/typed-recoverable, never false success. |
| Pending/retired roots never lose readable bytes or reopen. | Race request/authority pins with both marks and retirement; crash at every lifecycle, locator, Void, receipt, and expiry update; result is readable pending, deterministic 410, or leaked space. |
| Indexes are accelerators only. | Delete/corrupt every clean NVT/index/cache/journal combination and compare against independent authoritative evaluation. |
| Async coverage never delays blob/user acknowledgement. | Trace parser/posting/NVT/control work during ordinary PUT, merge, raw batch, sync, plugin write, and existing-chunk blob commit; none occurs in the hard waiter path. |
| Exact hash/token shortcuts cannot lie. | Inject digest/coordinate collisions and forged APOS values; complete source/tuple/root recheck prevents false result. |
| Migration cannot lose producer classes. | Mutate every family during copy, force capture exhaustion and crashes, then compare source/destination authority and operation ledgers. |
| Bounded memory is real. | Run GC + query + index + upload under an 8 GiB cgroup/job object; every owner reserves before growth, health remains schedulable, and no swap-dependent success is accepted. |
| Platform fixtures mean the same bytes. | Independent Linux, native macOS, and native Windows readers consume hand-authored fixtures and reject the same malformed corpus. |

Tests use Given/When/Then names, deterministic seeds, a per-test watchdog, and
an independent oracle that does not call the production serializer/planner.
Ordinary unit/property tests target under ten seconds; longer model, crash,
soak, migration, and copied-production tests are explicit ignored harnesses
with progress files and outer timeouts. A timeout is failure with preserved
state, not a hung CI worker.

### 11.2 Red/green phase commands

These commands are implementation obligations, not evidence that nonexistent
targets pass today. The first commit in the named landing unit creates the
target/script and records its expected red failure; the unit exits only when it
is green:

| Unit | Required narrow command |
| --- | --- |
| P0 | `timeout 2m ./scripts/plan/check-v4-contracts.sh` |
| P1 | `timeout 5m cargo test -j 6 -p aeordb --test v4_format_fixture_spec` |
| P2 | `timeout 10m cargo test -j 6 -p aeordb --test v3_contract_facade_spec` |
| P3 | `timeout 15m cargo test -j 6 -p aeordb --test v4_root_migration_spec` |
| P4 | `timeout 20m cargo test -j 6 -p aeordb --test gc_v4_model_spec` |
| P5 | `timeout 15m cargo test -j 6 -p aeordb --test index_v1_reference_spec` |
| P6 | `timeout 15m cargo test -j 6 -p aeordb --test coverage_runtime_spec` |
| P7 | `timeout 15m cargo test -j 6 -p aeordb --test root_api_reference_spec` |
| P8 | `timeout 30m cargo test -j 6 -p aeordb-cli --test cutover_fault_spec` |
| P9 | `timeout 2m ./scripts/plan/check-v4-debt.sh` |

The broad fmt/test/clippy/build, native-platform, S1/S2/S3, crash, `/tmp`, and
production-copy gates in the Final Synthesis remain mandatory after the narrow
gate. No target above is inserted into an ordinary fast suite if it exceeds
that suite's bounded runtime.

### 11.3 Recent-fix regression ledger

P0 maps each current hardening commit to named existing and new guarding specs;
at minimum:

| Fix | Existing regression inputs | New campaign obligation |
| --- | --- | --- |
| `779b91c` blob backpressure | `upload_commit_spec`, `upload_e2e_spec`, `shutdown_spec` | Queue admission, duplicate commit, timeout/disconnect, and graceful-shutdown model remain bounded. |
| `67cd185` commit latency | `upload_commit_spec`, `multi_index_spec`, metrics specs | Existing-chunk commit reads no content and waits for no derived index work. |
| `cc2c4c6` GC/cache residency | `gc_spec`, `cache_and_hardlinks_spec`, `metrics_spec` | Clean page/index cache evicts under global pressure without result change. |
| `4ecbd31`, `01f0a6e` B-tree/GC safety | `tree_walker_spec`, `corruption_hardening_spec`, `header_repair_spec` | Incomplete authoritative traversal prevents mark publication and creates typed repair evidence. |
| `a5bdd3e` HEAD counters | `engine_counters_spec`, `portal_spec`, `health_spec` | Reopen/rebuild counts current HEAD separately from revisions and survives damaged branches conservatively. |
| startup/readiness/shutdown hardening | `health_spec`, `shutdown_spec`, `resilience_features_spec` | `starting` progress/ETA and `server_ready` remain schedulable; admitted writes drain, reads obey bounded shutdown policy. |
| directories-first pagination | `directory_listing_spec`, `directory_listing_http_spec`, pagination specs | Ascending and descending pages never move directories behind files or duplicate/omit entries. |
| content hash/reindex | `content_hash_spec`, `reindex_spec`, query specs | Stored whole-file hash migrates/resaves and exact virtual-field lookup remains indexed. |
| media/range/coalesced reads | streaming/range/download specs | Range/seek semantics and contiguous coalescing remain bounded and byte exact. |

P0 may expand this ledger; it may not delete a recent fix because the v4
architecture makes its old implementation obsolete. The behavior remains or a
ratified divergence names why.

## 12. Final Structural and Security Gates

Before P1, a machine-readable contract registry must prove:

- every EntryType, KV tag, artifact/control kind, capability bit, family ID,
  enum ID, route class, and config property is unique or explicitly scoped;
- every exact format has magic/version/identity/body/cap/CRC, checked arithmetic,
  canonical order, reserve-zero, malformed/trailing behavior, both hash-width
  fixtures, and a producer/consumer owner;
- every typed hash slot declares whether it is a graph edge, identity,
  fingerprint, evidence digest, or hint;
- every dynamic record is discoverable and has a retention/compaction owner;
- every hard state transition has crash-before/crash-after recovery and one
  authority selector;
- every ignored cleanup result is classified: required durability/authority
  cleanup propagates failure; optional temp/telemetry cleanup warns and records
  debt without rewriting primary success semantics;
- every namespace read is in the route matrix and applies authorization before
  root state or result observables;
- every mutation producer uses the namespace and durability coordinators;
- every stable-key replacement uses the locator/retirement coordinator; and
- no generated plan text still asserts 512-byte v4 slots, data start 1024,
  RootExpiry only after receipt, hard-batched semantic mutation journals,
  process-local coverage authority, legacy APOS transition, or the incorrect
  SweepProposal formula.

Security tests include cross-user/root probing, share-link historical views,
forged root/APOS/hash values, system/conflict/permission path aliases,
symlink traversal, plugin host reads/writes, raw chunk/hash denial, malformed
protected state, and concealment-equivalent timing/count/error behavior.

## 13. Final Gauntlet Verdict

After these corrections, every issue found by the full-plan pass has a
normative contract, persistence identity, crash direction, implementation
owner, migration rule, rollback boundary, and falsifying test. The plan remains
faithful to AeorDB's design:

- namespace/content authority remains separate from disposable indexes;
- NVT remains a sparse approximate landing/range aid, never answer authority;
- KV remains unordered implementation machinery;
- persisted state is append/COW with authority-last publication;
- uncertainty retains data or leaks space rather than authorizing reuse;
- ordinary writes acknowledge durable user state without waiting for derived
  indexing; and
- historical bot search-to-range-fetch uses one explicit immutable root.

The decision log is now **gauntlet-complete and ready for owner ratification**.
It is not permission to implement. Formal parent/child plans must be regenerated
from Rounds 10 through 15, and the first machine contract check must demonstrate
that all superseded phrases listed above are absent from the generated plans.

## 14. Owner Ratification

On 2026-08-03, the owner explicitly ratified all ten owner-level policy areas
identified by the Round 15 gauntlet review:

1. the DatabaseHeader v4 layout and side-by-side migration model, including the
   1,024-byte header slot and 2,048-byte data start;
2. logical root retirement and physical GC semantics;
3. historical-root authorization and share behavior;
4. protected system-control storage under `/.aeordb-system/controls/v1/`;
5. authoritative system-family coverage and visibility;
6. the user-commit, asynchronous indexing, and mutation-journal boundary;
7. API compatibility, selector, APOS, and diagnostic-access behavior;
8. migration failure, capture, reconciliation, and source-GC policy;
9. the proposed operational defaults and runtime configuration surface; and
10. the rollout, resource, native-platform, crash/soak, production-copy, and
    FS-Server1 safety gates.

The owner also delegated exact offsets, tags, checksums, fixture arithmetic,
and other mechanical persistent-format details to implementation, subject to
the Phase 0 contract fixtures and correctness gates. This ratification closes
the policy approval gate. It does not waive any prerequisite, proof, migration,
or rollout gate, and it does not itself authorize production deployment.

# Round 16: Formal Parent/Child Plan Generation

**Date:** 2026-08-03
**Baseline:** `5d3e284652f9fec7a5c843f1946132574af4d469`
**Status:** CLOSED; formal executable campaign artifacts generated and reviewed
**Implementation status:** Not started

## 1. Owner Direction

The owner ratified the Round 15 policy list verbatim:

> 1-10: I agree with all of these

After being told that the next step was formal parent/child generation, the
owner directed:

> Let's do it

This authorized plan formalization, not production implementation or deployment.

## 2. Generated Campaign

The formal parent is:

`bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor.md`

Its executable children are:

1. `children/01-format-capabilities-and-fixtures.md`;
2. `children/02-durability-controls-config-and-memory.md`;
3. `children/03-namespace-semantic-roots-and-system-families.md`;
4. `children/04-physical-inventory-gc-and-void.md`;
5. `children/05-index-definitions-pages-and-nvt.md`;
6. `children/06-async-coverage-query-pagination-and-locators.md`;
7. `children/07-side-by-side-migration-cutover-and-rollout.md`; and
8. `children/08-verification-operations-docs-and-debt.md`.

Each child freezes outcome, owned and forbidden territory, start dependencies,
landing units, one-command narrow proof, current regression inputs, real-world
and resource proof, rollback, and mechanical definition of done. Eight separate
progress ledgers hold mutable owner/commit/handoff/test/evidence state.

The old 2026-07-16 NVT plan is marked superseded in full. Historical indexing,
query, parser, GC, locator, durability, backup, replication, task, system-table,
event, API-key, and permission plans carry partial-incorporation banners for the
exact contracts moved into this campaign.

## 3. Formalization Review

The review artifact is:

`bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/formalization-review.md`

The baseline still matched `origin/development`. The live source inventory still
contained 93 Axum route registrations and 161 Rust spec files. Mechanical checks
passed for whitespace, Markdown fences, relative links, YAML banner parsing,
uniform child execution sections, current-versus-planned test target names,
superseded assertions, and unresolved policy markers.

The adversarial pass found and corrected five formalization defects:

1. P3a format-writer ownership was corrected from Child 03 to Child 01 while
   Child 03 retained v4 root/semantic ownership.
2. This ratified decision source became a required tracked, append-only P0
   prerequisite rather than an unversioned local dependency.
3. Per-child progress ledgers were added for last-green commits, hotspot
   handoffs, gates, drift, evidence, and next actions.
4. The 24-hour grace default, fixed exactly-two-mark invariant,
   safety-asymmetric eligibility formula, and GC scratch reserve expression were
   made explicit in the formal plan.
5. Every child received uniform Outcome, Owned Territory, Landing Sequence,
   Verification, Rollback, and Definition of Done sections.

No new owner-policy decision was introduced. The final dependency graph has no
cycle and preserves one integration owner for shared registries and hotspots.

## 4. Execution Boundary

The formal campaign is ready for separate implementation authorization. The
first executable unit is Child 08 P0a, followed by Child 01 P0b/P0c. Before P0a,
the implementation owner must refresh `development`, re-run the drift inventory,
assign explicit file ownership, and update the progress ledgers.

No Cargo tests or release builds were run during formalization because no
production code changed and the named campaign test targets are intentionally
future red/green obligations. No database was opened or modified.
