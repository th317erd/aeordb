# AeorDB Index + Query Torture Test Results

Date: 2026-04-02T19:20:39Z
Database: /media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/

## Setup
4 indexes configured: age(u64), name(string), city(string), score(u64)

## Phase 1: Store 500 Indexed JSON Files
Stored: 500/500 in 43218ms (11.5 files/sec)

## Phase 2: Queries

| Query | Results | Latency |
|---|---|---|
| age > 50 | 230 | 10ms |
| name = Bob | 29 | 9ms |
| age > 30 AND city = NYC | 53 | 12ms |
| city = NYC OR city = LA | 114 | 13ms |
| age > 30 AND NOT name = Alice | 379 | 13ms |
| city IN (NYC,LA,Austin) | 155 | 10ms |
| (age>40 AND score>500) OR city=Austin | 167 | 11ms |

## Phase 3: Query Performance
200 simple AND queries: 2018ms (avg 10ms/query)
100 complex OR queries: 1000ms (avg 10ms/query)

## Phase 4: Delete + Query Verification
Deleted: 100/100
Files remaining (age > 0): 400 (expected ~400)

## Phase 5: Snapshot
Snapshot: 

## Storage

## Summary

| Metric | Value |
|---|---|
| Files stored | 500 |
| Indexes | 4 (age, name, city, score) |
| Simple query avg | 10ms |
| Complex OR query avg | 10ms |
| Files deleted | 100 |
| Files remaining | 400 |

