# TokenCache v2 — in-memory token serving with dense indices

## Problem

`/tokens` on large chains (Base: ~4M tokens) is slow and expensive:

- Every page request runs a `COUNT` query with an `EXISTS` subquery on
  `component_balance_default`, plus an `OFFSET` scan. A full `get_all_tokens`
  sweep is ~310 pages, each paying both.
- The RPC `RpcCache` is keyed per request body, so each page is a separate
  cache entry and misses independently.
- Token queries compete for the small DB connection pool with snapshot
  endpoints.

PR #610 attempted an in-memory cache but was slower than Postgres because its
indexes were `BTreeMap<_, BTreeSet<Address>>`: a filter k-merged sorted
iterators over millions of heap-allocated `Bytes` values, `total` forced full
candidate materialization on every request, and the traded-ts index grew
without bound. It also missed quality updates written by the out-of-process
`analyze-tokens` job, and changed result ordering.

## Goals

- Serve `/tokens` without touching Postgres on the hot path.
- Sub-10ms query latency on 4M tokens; full-list sweep in seconds.
- New tokens visible same-block in the `index` process (write-through).
- Quality updates from `analyze-tokens` visible within ~1 minute (delta poll).
- Byte-identical results and ordering vs. the SQL path (`ORDER BY token.id`).

## Non-goals

- Caching in the `analyze-tokens` process (it keeps the SQL path).
- Day-bucketed traded-ts bitmaps (only if profiling shows the linear scan
  matters).
- API/DTO changes (none; clients unaffected).

## Design

`TokenCache` lives in `tycho-storage`, owned by `PostgresGateway`, optional
via the gateway builder (enabled for `index` and `rpc` commands only). The
existing SQL implementation of `get_tokens` remains as the disabled-path
fallback.

### Data layout

Per chain, one `RwLock<ChainTokenStore>` (never held across an await):

```rust
struct ChainTokenStore {
    tokens: Vec<Arc<Token>>,                     // append-only, DB-id order
    idx_by_address: HashMap<Address, u32>,       // address -> position
    quality_index: BTreeMap<i32, RoaringBitmap>, // quality -> positions
    last_traded: Vec<i64>,                       // unix secs, 0 = never
}
```

- Dense `u32` position replaces `Address` as the set element; indexes are
  `RoaringBitmap`s, so filters are word-wise AND/OR instead of per-element
  `Bytes` comparisons.
- Quality filter: union of `quality_index.range(min..=max)` bitmaps.
- Traded filter: compare `last_traded[i]` during candidate iteration.
- `total`: bitmap cardinality / one counting pass — no materialization.
- Page: iterate candidates, skip offset, take page_size, clone `Arc` targets.
- Address-filter queries bypass the indexes: hashmap lookup per address, then
  check quality / last-traded fields directly.

### Writes and freshness

- Write-through in `add_tokens` / `update_tokens` (append or update in place;
  on quality change move the bit between bitmaps) and
  `insert_component_balances` (`last_traded[i] = max(current, valid_from)`).
- Background delta poll every 60s:
  `WHERE token.modified_ts > $last_sync ORDER BY token.id`, upsert results.
  Catches `analyze-tokens` quality updates and makes `rpc`-only processes
  converge. `last_sync` only advances on success.
- Startup: per-chain paged full scan, ordered by `token.id`.
- Tokens are never deleted from the DB; no eviction.

### Migration

One new index: `CREATE INDEX ... ON token (modified_ts)`.

### Error handling

- Cache init failure at startup is fatal for `index`/`rpc` (matches the other
  enum caches; better than serving empty token lists).
- Unknown address in an address filter: absent from results, not an error.
- Delta poll failure: log, retry next tick.

## Memory budget

Base worst case: token vec ~0.5–1 GiB (accepted), bitmaps a few MB,
`last_traded` 32 MB. Pods have 16 Gi.

## Testing

- Unit tests on `ChainTokenStore`: quality moves, last-traded monotonicity,
  pagination boundaries, empty results.
- `serial_db` equivalence tests: same fixture queries through SQL path and
  cache path must return identical results, totals, ordering.
- Benchmark against a real-size dataset (dev ethereum DB) measuring startup
  load, per-query latency, and full paging sweep vs. the SQL path.
