//! In-memory token store used to serve `get_tokens` without hitting Postgres.
//!
//! Tokens are held per chain in insertion order (ascending `token.id`), so a token's
//! position in the vector is a dense `u32` index. Filter indexes are `RoaringBitmap`s
//! over these positions, which makes filter evaluation a couple of bitmap operations
//! instead of per-address comparisons.
//!
//! Freshness relies on two mechanisms:
//! - write-through: `add_tokens`/`update_tokens`/balance inserts update the cache in the same
//!   process that writes the DB.
//! - delta refresh: `refresh` polls `token.modified_ts > last_sync`, picking up writes from other
//!   processes (e.g. quality updates from the token analysis cron).
use std::{
    collections::{BTreeMap, HashMap},
    ops::Bound,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};

use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection, RunQueryDsl};
use roaring::RoaringBitmap;
use tracing::{error, info};
use tycho_common::{
    models::{protocol::QualityRange, token::Token, Address, Chain, PaginationParams},
    storage::{StorageError, WithTotal},
};

use crate::postgres::{orm, schema, PostgresError};

/// Number of rows fetched per query during the initial full load.
const LOAD_BATCH_SIZE: i64 = 500_000;

/// Timestamp value for tokens that never appeared in a component balance.
/// `i64::MIN` sorts below any real threshold, matching the SQL `EXISTS` filter
/// which excludes such tokens.
const NEVER_TRADED: i64 = i64::MIN;

#[derive(Debug, Clone)]
pub(crate) struct TokenQuery {
    pub(crate) chain: Chain,
    pub(crate) addresses: Option<Vec<Address>>,
    pub(crate) quality_range: QualityRange,
    pub(crate) last_traded_ts_threshold: Option<NaiveDateTime>,
    pub(crate) pagination: Option<PaginationParams>,
}

#[derive(Default)]
struct ChainTokenStore {
    /// All tokens of the chain in ascending `token.id` order. Append-only.
    tokens: Vec<Arc<Token>>,
    /// Token address -> position in `tokens`.
    idx_by_address: HashMap<Address, u32>,
    /// Quality value -> positions of tokens with that quality.
    quality_index: BTreeMap<i32, RoaringBitmap>,
    /// Last traded timestamp (unix micros) per position, `NEVER_TRADED` if none.
    last_traded: Vec<i64>,
}

impl ChainTokenStore {
    /// Inserts or updates a token. With `overwrite` false an existing entry is left
    /// untouched, mirroring the `ON CONFLICT DO NOTHING` semantics of the token
    /// insert statement.
    fn upsert(&mut self, token: Token, overwrite: bool) {
        match self
            .idx_by_address
            .get(&token.address)
            .copied()
        {
            Some(idx) => {
                if !overwrite {
                    return;
                }
                let old = &self.tokens[idx as usize];
                if old.quality != token.quality {
                    if let Some(bitmap) = self
                        .quality_index
                        .get_mut(&(old.quality as i32))
                    {
                        bitmap.remove(idx);
                    }
                    self.quality_index
                        .entry(token.quality as i32)
                        .or_default()
                        .insert(idx);
                }
                self.tokens[idx as usize] = Arc::new(token);
            }
            None => {
                let idx = self.tokens.len() as u32;
                self.idx_by_address
                    .insert(token.address.clone(), idx);
                self.quality_index
                    .entry(token.quality as i32)
                    .or_default()
                    .insert(idx);
                self.tokens.push(Arc::new(token));
                self.last_traded.push(NEVER_TRADED);
            }
        }
    }

    fn update_last_traded(&mut self, address: &Address, ts: NaiveDateTime) {
        if let Some(&idx) = self.idx_by_address.get(address) {
            let ts = ts.and_utc().timestamp_micros();
            let current = &mut self.last_traded[idx as usize];
            *current = (*current).max(ts);
        }
    }

    /// Evaluates a query against the store.
    ///
    /// Results are in ascending `token.id` order and `total` counts all matches
    /// regardless of pagination, matching the SQL implementation of `get_tokens`.
    fn query(&self, query: &TokenQuery) -> WithTotal<Vec<Token>> {
        let candidates = self.candidate_bitmap(query);
        let ts_threshold = query
            .last_traded_ts_threshold
            .map(|ts| ts.and_utc().timestamp_micros());

        let (offset, limit) = query
            .pagination
            .as_ref()
            .map(|p| (p.offset().max(0) as usize, p.page_size.max(0) as usize))
            .unwrap_or((0, usize::MAX));

        match (candidates, ts_threshold) {
            // Bitmap cardinality gives the total for free; the page is a plain slice
            // of the candidate iterator.
            (Some(bitmap), None) => {
                let total = bitmap.len() as i64;
                let entity = bitmap
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|idx| (*self.tokens[idx as usize]).clone())
                    .collect();
                WithTotal { entity, total: Some(total) }
            }
            (Some(bitmap), Some(threshold)) => self.paginate_filtered(
                bitmap.iter().map(|idx| idx as usize),
                threshold,
                offset,
                limit,
            ),
            (None, None) => {
                let total = self.tokens.len() as i64;
                let entity = self
                    .tokens
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|token| (**token).clone())
                    .collect();
                WithTotal { entity, total: Some(total) }
            }
            (None, Some(threshold)) => {
                self.paginate_filtered(0..self.tokens.len(), threshold, offset, limit)
            }
        }
    }

    /// Combines the address and quality filters into a single bitmap of candidate
    /// positions. `None` means "no filter" (all tokens are candidates).
    fn candidate_bitmap(&self, query: &TokenQuery) -> Option<RoaringBitmap> {
        let mut candidates: Option<RoaringBitmap> = query
            .addresses
            .as_ref()
            .map(|addresses| {
                addresses
                    .iter()
                    .filter_map(|address| {
                        self.idx_by_address
                            .get(address)
                            .copied()
                    })
                    .collect()
            });

        let quality_bounds = (query.quality_range.min, query.quality_range.max);
        if quality_bounds != (None, None) {
            let lower = quality_bounds
                .0
                .map_or(Bound::Unbounded, Bound::Included);
            let upper = quality_bounds
                .1
                .map_or(Bound::Unbounded, Bound::Included);
            let mut quality_bitmap = RoaringBitmap::new();
            for (_, bitmap) in self.quality_index.range((lower, upper)) {
                quality_bitmap |= bitmap;
            }
            candidates = Some(match candidates {
                Some(address_bitmap) => address_bitmap & quality_bitmap,
                None => quality_bitmap,
            });
        }

        candidates
    }

    /// Single pass over candidate positions applying the last-traded filter,
    /// counting all matches and collecting the requested page.
    fn paginate_filtered(
        &self,
        positions: impl Iterator<Item = usize>,
        threshold: i64,
        offset: usize,
        limit: usize,
    ) -> WithTotal<Vec<Token>> {
        let mut total = 0usize;
        let mut entity = Vec::new();
        for pos in positions {
            if self.last_traded[pos] <= threshold {
                continue;
            }
            if total >= offset && entity.len() < limit {
                entity.push((*self.tokens[pos]).clone());
            }
            total += 1;
        }
        WithTotal { entity, total: Some(total as i64) }
    }
}

pub struct TokenCache {
    chains: HashMap<Chain, RwLock<ChainTokenStore>>,
    chain_ids: HashMap<i64, Chain>,
    /// Largest `token.modified_ts` this cache has seen; `refresh` polls rows newer
    /// than this.
    last_sync: RwLock<NaiveDateTime>,
}

impl TokenCache {
    pub async fn from_pool(pool: Pool<AsyncPgConnection>) -> Result<Self, StorageError> {
        let mut conn = pool
            .get()
            .await
            .map_err(|err| StorageError::Unexpected(err.to_string()))?;
        Self::from_connection(&mut conn).await
    }

    pub async fn from_connection(conn: &mut AsyncPgConnection) -> Result<Self, StorageError> {
        let start = std::time::Instant::now();
        let chain_rows: Vec<(i64, String)> = schema::chain::table
            .select((schema::chain::id, schema::chain::name))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let mut chain_ids = HashMap::new();
        let mut chains = HashMap::new();
        let mut last_sync = NaiveDateTime::default();
        for (chain_id, chain_name) in chain_rows {
            let chain = Chain::from_str(&chain_name).map_err(|_| {
                StorageError::Unexpected(format!("Unknown chain in chain table: {chain_name}"))
            })?;
            chain_ids.insert(chain_id, chain);

            let (store, max_modified_ts) = Self::load_chain(conn, chain, chain_id).await?;
            last_sync = last_sync.max(max_modified_ts);
            info!(
                chain = %chain,
                n_tokens = store.tokens.len(),
                elapsed = ?start.elapsed(),
                "Loaded token cache"
            );
            chains.insert(chain, RwLock::new(store));
        }

        Ok(Self { chains, chain_ids, last_sync: RwLock::new(last_sync) })
    }

    async fn load_chain(
        conn: &mut AsyncPgConnection,
        chain: Chain,
        chain_id: i64,
    ) -> Result<(ChainTokenStore, NaiveDateTime), StorageError> {
        let mut store = ChainTokenStore::default();
        let mut idx_by_db_id: HashMap<i64, u32> = HashMap::new();
        let mut max_modified_ts = NaiveDateTime::default();

        let mut last_db_id = i64::MIN;
        loop {
            let batch: Vec<(orm::Token, Address)> = schema::token::table
                .inner_join(schema::account::table)
                .filter(schema::account::chain_id.eq(chain_id))
                .filter(schema::token::id.gt(last_db_id))
                .order(schema::token::id.asc())
                .limit(LOAD_BATCH_SIZE)
                .select((orm::Token::as_select(), schema::account::address))
                .load(conn)
                .await
                .map_err(PostgresError::from)?;

            let batch_len = batch.len();
            for (orm_token, address) in batch {
                last_db_id = orm_token.id;
                max_modified_ts = max_modified_ts.max(orm_token.modified_ts);
                idx_by_db_id.insert(orm_token.id, store.tokens.len() as u32);
                store.upsert(to_model_token(&orm_token, &address, chain), true);
            }
            if (batch_len as i64) < LOAD_BATCH_SIZE {
                break;
            }
        }

        // Latest balance change per token, mirroring the SQL `EXISTS` filter on
        // `component_balance_default.valid_from`.
        let last_traded: Vec<(i64, NaiveDateTime)> = schema::component_balance_default::table
            .inner_join(schema::protocol_component::table)
            .filter(schema::protocol_component::chain_id.eq(chain_id))
            .select((
                schema::component_balance_default::token_id,
                schema::component_balance_default::valid_from,
            ))
            .order_by((
                schema::component_balance_default::token_id.asc(),
                schema::component_balance_default::valid_from.desc(),
            ))
            .distinct_on(schema::component_balance_default::token_id)
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        for (token_db_id, valid_from) in last_traded {
            if let Some(&idx) = idx_by_db_id.get(&token_db_id) {
                store.last_traded[idx as usize] = valid_from.and_utc().timestamp_micros();
            }
        }

        Ok((store, max_modified_ts))
    }

    pub(crate) fn query_tokens(
        &self,
        query: &TokenQuery,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        let store = self.store(&query.chain)?;
        let guard = store
            .read()
            .expect("token cache lock poisoned");
        Ok(guard.query(query))
    }

    /// Inserts tokens that are not yet cached; existing entries are left untouched,
    /// mirroring the `ON CONFLICT DO NOTHING` insert semantics.
    pub(crate) fn add_tokens(&self, tokens: &[Token]) {
        self.write_tokens(tokens, false);
    }

    /// Inserts or overwrites tokens with the given values.
    pub(crate) fn upsert_tokens(&self, tokens: &[Token]) {
        self.write_tokens(tokens, true);
    }

    fn write_tokens(&self, tokens: &[Token], overwrite: bool) {
        let mut by_chain: HashMap<Chain, Vec<&Token>> = HashMap::new();
        for token in tokens {
            by_chain
                .entry(token.chain)
                .or_default()
                .push(token);
        }
        for (chain, chain_tokens) in by_chain {
            let Ok(store) = self.store(&chain) else {
                error!(chain = %chain, "Token upsert for chain missing from token cache");
                continue;
            };
            let mut guard = store
                .write()
                .expect("token cache lock poisoned");
            for token in chain_tokens {
                guard.upsert(token.clone(), overwrite);
            }
        }
    }

    pub(crate) fn update_last_traded<'a>(
        &self,
        chain: &Chain,
        updates: impl Iterator<Item = (&'a Address, NaiveDateTime)>,
    ) {
        let Ok(store) = self.store(chain) else {
            error!(chain = %chain, "Balance update for chain missing from token cache");
            return;
        };
        let mut guard = store
            .write()
            .expect("token cache lock poisoned");
        for (address, ts) in updates {
            guard.update_last_traded(address, ts);
        }
    }

    /// Loads tokens modified since the last sync and upserts them, so the cache
    /// converges on writes made by other processes. Advances the sync marker only
    /// on success.
    pub async fn refresh(&self, conn: &mut AsyncPgConnection) -> Result<usize, StorageError> {
        let since = *self
            .last_sync
            .read()
            .expect("token cache lock poisoned");

        let rows: Vec<(orm::Token, Address, i64)> = schema::token::table
            .inner_join(schema::account::table)
            .filter(schema::token::modified_ts.gt(since))
            .order(schema::token::id.asc())
            .select((orm::Token::as_select(), schema::account::address, schema::account::chain_id))
            .load(conn)
            .await
            .map_err(PostgresError::from)?;

        let n_refreshed = rows.len();
        let mut max_modified_ts = since;
        let mut refreshed = Vec::with_capacity(n_refreshed);
        for (orm_token, address, chain_id) in rows {
            let Some(chain) = self.chain_ids.get(&chain_id) else {
                continue;
            };
            max_modified_ts = max_modified_ts.max(orm_token.modified_ts);
            refreshed.push(to_model_token(&orm_token, &address, *chain));
        }
        self.upsert_tokens(&refreshed);

        *self
            .last_sync
            .write()
            .expect("token cache lock poisoned") = max_modified_ts;
        Ok(n_refreshed)
    }

    /// Spawns a detached task calling `refresh` every `period`, so the cache picks
    /// up token writes from other processes (e.g. the token analysis cron).
    pub fn spawn_refresh_task(self: &Arc<Self>, pool: Pool<AsyncPgConnection>, period: Duration) {
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it, the cache was just loaded.
            interval.tick().await;
            loop {
                interval.tick().await;
                match pool.get().await {
                    Ok(mut conn) => {
                        if let Err(err) = cache.refresh(&mut conn).await {
                            error!(%err, "Token cache refresh failed");
                        }
                    }
                    Err(err) => error!(%err, "Token cache refresh could not get a connection"),
                }
            }
        });
    }

    fn store(&self, chain: &Chain) -> Result<&RwLock<ChainTokenStore>, StorageError> {
        self.chains
            .get(chain)
            .ok_or_else(|| StorageError::NotFound("Chain".to_string(), chain.to_string()))
    }
}

#[cfg(test)]
impl TokenCache {
    fn new_for_tests(chains: &[Chain]) -> Self {
        Self {
            chains: chains
                .iter()
                .map(|chain| (*chain, RwLock::new(ChainTokenStore::default())))
                .collect(),
            chain_ids: HashMap::new(),
            last_sync: RwLock::new(NaiveDateTime::default()),
        }
    }
}

fn to_model_token(orm_token: &orm::Token, address: &Address, chain: Chain) -> Token {
    let gas_usage: Vec<_> = orm_token
        .gas
        .iter()
        .map(|gas| gas.map(|value| value as u64))
        .collect();
    Token::new(
        address,
        orm_token.symbol.as_str(),
        orm_token.decimals as u32,
        orm_token.tax as u64,
        gas_usage.as_slice(),
        chain,
        orm_token.quality as u32,
    )
}

#[cfg(test)]
mod test {
    use chrono::DateTime;

    use super::*;

    fn make_token(seed: u8, quality: u32) -> Token {
        Token::new(
            &Address::from([seed; 20]),
            &format!("TOK{seed}"),
            18,
            0,
            &[Some(64_000)],
            Chain::Ethereum,
            quality,
        )
    }

    fn ts(secs: i64) -> NaiveDateTime {
        DateTime::from_timestamp(secs, 0)
            .unwrap()
            .naive_utc()
    }

    fn base_query() -> TokenQuery {
        TokenQuery {
            chain: Chain::Ethereum,
            addresses: None,
            quality_range: QualityRange::None(),
            last_traded_ts_threshold: None,
            pagination: None,
        }
    }

    fn store_with_tokens(qualities: &[u32]) -> TokenCache {
        let cache = TokenCache::new_for_tests(&[Chain::Ethereum]);
        let tokens: Vec<Token> = qualities
            .iter()
            .enumerate()
            .map(|(position, quality)| make_token(position as u8, *quality))
            .collect();
        cache.add_tokens(&tokens);
        cache
    }

    fn result_symbols(result: &WithTotal<Vec<Token>>) -> Vec<&str> {
        result
            .entity
            .iter()
            .map(|token| token.symbol.as_str())
            .collect()
    }

    #[test]
    fn test_query_all_preserves_insertion_order() {
        let cache = store_with_tokens(&[100, 0, 50]);

        let result = cache
            .query_tokens(&base_query())
            .unwrap();

        assert_eq!(result.total, Some(3));
        assert_eq!(result_symbols(&result), ["TOK0", "TOK1", "TOK2"]);
    }

    #[test]
    fn test_quality_range_filters() {
        let cache = store_with_tokens(&[100, 0, 50, 75, 10]);

        let min_only = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::min_only(50), ..base_query() })
            .unwrap();
        assert_eq!(min_only.total, Some(3));
        assert_eq!(result_symbols(&min_only), ["TOK0", "TOK2", "TOK3"]);

        let min_max = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::new(10, 75), ..base_query() })
            .unwrap();
        assert_eq!(result_symbols(&min_max), ["TOK2", "TOK3", "TOK4"]);
    }

    #[test]
    fn test_update_moves_quality_index_entry() {
        let cache = store_with_tokens(&[100, 100]);
        let mut updated = make_token(0, 5);
        updated.symbol = "TOK0v2".to_string();
        cache.upsert_tokens(&[updated]);

        let high = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::min_only(50), ..base_query() })
            .unwrap();
        assert_eq!(result_symbols(&high), ["TOK1"]);

        let low = cache
            .query_tokens(&TokenQuery { quality_range: QualityRange::new(0, 49), ..base_query() })
            .unwrap();
        assert_eq!(result_symbols(&low), ["TOK0v2"]);
    }

    #[test]
    fn test_add_does_not_overwrite_existing() {
        let cache = store_with_tokens(&[100]);
        let mut duplicate = make_token(0, 5);
        duplicate.symbol = "SHOULD_NOT_APPEAR".to_string();
        cache.add_tokens(&[duplicate]);

        let result = cache
            .query_tokens(&base_query())
            .unwrap();
        assert_eq!(result_symbols(&result), ["TOK0"]);
        assert_eq!(result.entity[0].quality, 100);
    }

    #[test]
    fn test_last_traded_filter_excludes_never_and_older() {
        let cache = store_with_tokens(&[100, 100, 100]);
        cache.update_last_traded(
            &Chain::Ethereum,
            [(&Address::from([0u8; 20]), ts(1_000)), (&Address::from([1u8; 20]), ts(2_000))]
                .into_iter(),
        );

        let result = cache
            .query_tokens(&TokenQuery { last_traded_ts_threshold: Some(ts(1_000)), ..base_query() })
            .unwrap();

        // TOK0 traded exactly at the threshold (strict `>` excludes it), TOK2 never.
        assert_eq!(result.total, Some(1));
        assert_eq!(result_symbols(&result), ["TOK1"]);
    }

    #[test]
    fn test_last_traded_is_monotonic() {
        let cache = store_with_tokens(&[100]);
        let address = Address::from([0u8; 20]);
        cache.update_last_traded(&Chain::Ethereum, [(&address, ts(2_000))].into_iter());
        cache.update_last_traded(&Chain::Ethereum, [(&address, ts(1_000))].into_iter());

        let result = cache
            .query_tokens(&TokenQuery { last_traded_ts_threshold: Some(ts(1_500)), ..base_query() })
            .unwrap();
        assert_eq!(result.total, Some(1));
    }

    #[test]
    fn test_pagination_boundaries() {
        let cache = store_with_tokens(&[100, 100, 100, 100, 100]);

        let page = |page_number: i64| {
            cache
                .query_tokens(&TokenQuery {
                    pagination: Some(PaginationParams::new(page_number, 2)),
                    ..base_query()
                })
                .unwrap()
        };

        assert_eq!(result_symbols(&page(0)), ["TOK0", "TOK1"]);
        assert_eq!(result_symbols(&page(1)), ["TOK2", "TOK3"]);
        assert_eq!(result_symbols(&page(2)), ["TOK4"]);
        assert!(page(3).entity.is_empty());
        // Total is independent of the requested page.
        assert_eq!(page(3).total, Some(5));
    }

    #[test]
    fn test_pagination_with_last_traded_filter() {
        let cache = store_with_tokens(&[100, 100, 100, 100]);
        for seed in [0u8, 2, 3] {
            cache.update_last_traded(
                &Chain::Ethereum,
                [(&Address::from([seed; 20]), ts(5_000))].into_iter(),
            );
        }

        let result = cache
            .query_tokens(&TokenQuery {
                last_traded_ts_threshold: Some(ts(1_000)),
                pagination: Some(PaginationParams::new(1, 2)),
                ..base_query()
            })
            .unwrap();

        assert_eq!(result.total, Some(3));
        assert_eq!(result_symbols(&result), ["TOK3"]);
    }

    #[test]
    fn test_address_filter_orders_by_insertion_and_ignores_unknown() {
        let cache = store_with_tokens(&[100, 100, 100]);

        let result = cache
            .query_tokens(&TokenQuery {
                addresses: Some(vec![
                    Address::from([2u8; 20]),
                    Address::from([0u8; 20]),
                    Address::from([9u8; 20]),
                ]),
                ..base_query()
            })
            .unwrap();

        assert_eq!(result.total, Some(2));
        assert_eq!(result_symbols(&result), ["TOK0", "TOK2"]);
    }

    #[test]
    fn test_address_and_quality_filters_combine() {
        let cache = store_with_tokens(&[100, 10, 100]);

        let result = cache
            .query_tokens(&TokenQuery {
                addresses: Some(vec![Address::from([0u8; 20]), Address::from([1u8; 20])]),
                quality_range: QualityRange::min_only(50),
                ..base_query()
            })
            .unwrap();

        assert_eq!(result_symbols(&result), ["TOK0"]);
    }

    #[test]
    fn test_unknown_chain_is_an_error() {
        let cache = store_with_tokens(&[100]);
        let result = cache.query_tokens(&TokenQuery { chain: Chain::Base, ..base_query() });
        assert!(matches!(result, Err(StorageError::NotFound(_, _))));
    }
}

/// Benchmark against a real database, comparing the cache path with the SQL path.
///
/// Read-only: connects without running migrations and forces a read-only session.
/// Run with:
///   DATABASE_URL=... cargo test -p tycho-storage --release --lib \
///     token_cache_benchmark -- --ignored --nocapture
#[cfg(test)]
mod benchmark {
    use std::time::Instant;

    use diesel_async::AsyncConnection;

    use super::*;
    use crate::postgres::PostgresGateway;

    fn rss_mib() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .map(|value| {
                            value
                                .trim()
                                .trim_end_matches(" kB")
                                .parse::<f64>()
                                .unwrap_or(0.0) /
                                1024.0
                        })
                })
            })
            .unwrap_or(0.0)
    }

    async fn read_only_connection(db_url: &str) -> AsyncPgConnection {
        let mut conn = AsyncPgConnection::establish(db_url)
            .await
            .expect("failed to connect");
        diesel::sql_query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
            .execute(&mut conn)
            .await
            .expect("failed to set session read-only");
        conn
    }

    struct Scenario {
        name: &'static str,
        quality: QualityRange,
        traded_days: Option<i64>,
        page: Option<(i64, i64)>,
        addresses: Option<Vec<Address>>,
    }

    #[tokio::test]
    #[ignore]
    async fn token_cache_benchmark() {
        let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let chain = std::env::var("BENCH_CHAIN")
            .map(|name| Chain::from_str(&name).expect("invalid BENCH_CHAIN"))
            .unwrap_or(Chain::Ethereum);
        let mut conn = read_only_connection(&db_url).await;

        let gateway = PostgresGateway::from_connection(&mut conn).await;
        assert!(gateway.token_cache.is_none(), "gateway must use the SQL path");

        let rss_before = rss_mib();
        let load_start = Instant::now();
        let cache = TokenCache::from_connection(&mut conn)
            .await
            .expect("cache load failed");
        let load_elapsed = load_start.elapsed();
        let rss_after = rss_mib();

        // Paginated bootstrap queries: an unpaginated query clones every cached token,
        // which is too much transient memory on multi-million token chains.
        let n_tokens = cache
            .query_tokens(&TokenQuery {
                chain,
                addresses: None,
                quality_range: QualityRange::None(),
                last_traded_ts_threshold: None,
                pagination: Some(PaginationParams::new(0, 1)),
            })
            .expect("query failed")
            .total
            .unwrap();
        println!("== token cache benchmark ==");
        println!("chain: {chain}");
        println!("tokens: {n_tokens}");
        println!("cache load: {load_elapsed:?}, RSS {rss_before:.0} MiB -> {rss_after:.0} MiB");

        // Sample addresses spread across the first 100k tokens for the address filter.
        let sample_addresses: Vec<Address> = {
            let first_page = cache
                .query_tokens(&TokenQuery {
                    chain,
                    addresses: None,
                    quality_range: QualityRange::None(),
                    last_traded_ts_threshold: None,
                    pagination: Some(PaginationParams::new(0, 100_000)),
                })
                .unwrap()
                .entity;
            first_page
                .iter()
                .step_by((first_page.len() / 100).max(1))
                .map(|token| token.address.clone())
                .take(100)
                .collect()
        };

        let page_size = 3_000i64;
        let deep_page = (n_tokens / 2) / page_size;
        let scenarios = vec![
            Scenario {
                name: "all tokens, page 0",
                quality: QualityRange::None(),
                traded_days: None,
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "all tokens, deep page",
                quality: QualityRange::None(),
                traded_days: None,
                page: Some((deep_page, page_size)),
                addresses: None,
            },
            Scenario {
                name: "min_quality=51, page 0",
                quality: QualityRange::min_only(51),
                traded_days: None,
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "min_quality=51 traded_30d, page 0",
                quality: QualityRange::min_only(51),
                traded_days: Some(30),
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "min_quality=0 traded_30d, page 0",
                quality: QualityRange::min_only(0),
                traded_days: Some(30),
                page: Some((0, page_size)),
                addresses: None,
            },
            Scenario {
                name: "100 addresses",
                quality: QualityRange::None(),
                traded_days: None,
                page: Some((0, page_size)),
                addresses: Some(sample_addresses),
            },
        ];

        for scenario in &scenarios {
            let threshold = scenario
                .traded_days
                .map(|days| chrono::Utc::now().naive_utc() - chrono::Duration::days(days));
            let pagination = scenario
                .page
                .map(|(page, size)| PaginationParams::new(page, size));

            let address_refs: Option<Vec<&Address>> = scenario
                .addresses
                .as_ref()
                .map(|addresses| addresses.iter().collect());

            // Warm the DB page cache with one run, then measure the second.
            let mut sql_result = None;
            let mut sql_elapsed = Duration::default();
            for _ in 0..2 {
                let started = Instant::now();
                sql_result = Some(
                    gateway
                        .get_tokens(
                            chain,
                            address_refs.as_deref(),
                            scenario.quality.clone(),
                            threshold,
                            pagination.as_ref(),
                            &mut conn,
                        )
                        .await
                        .expect("sql query failed"),
                );
                sql_elapsed = started.elapsed();
            }
            let sql_result = sql_result.unwrap();

            let query = TokenQuery {
                chain,
                addresses: scenario.addresses.clone(),
                quality_range: scenario.quality.clone(),
                last_traded_ts_threshold: threshold,
                pagination,
            };
            let started = Instant::now();
            let cache_result = cache
                .query_tokens(&query)
                .expect("cache query failed");
            let cache_elapsed = started.elapsed();

            let equal_totals = sql_result.total == cache_result.total;
            let sql_addresses: Vec<&Address> = sql_result
                .entity
                .iter()
                .map(|token| &token.address)
                .collect();
            let cache_addresses: Vec<&Address> = cache_result
                .entity
                .iter()
                .map(|token| &token.address)
                .collect();
            let equal_pages = sql_addresses == cache_addresses;

            println!(
                "{:40} sql {:>12?}  cache {:>10?}  speedup {:>8.0}x  total {:>9?}  match totals={} pages={}",
                scenario.name,
                sql_elapsed,
                cache_elapsed,
                sql_elapsed.as_secs_f64() / cache_elapsed.as_secs_f64().max(1e-9),
                sql_result.total.unwrap(),
                equal_totals,
                equal_pages,
            );
        }

        // Full paging sweep at client defaults: every page through the filtered list.
        let threshold = chrono::Utc::now().naive_utc() - chrono::Duration::days(30);
        let total = cache
            .query_tokens(&TokenQuery {
                chain,
                addresses: None,
                quality_range: QualityRange::min_only(51),
                last_traded_ts_threshold: Some(threshold),
                pagination: Some(PaginationParams::new(0, 1)),
            })
            .unwrap()
            .total
            .unwrap();
        let n_pages = (total + page_size - 1) / page_size;

        let started = Instant::now();
        for page in 0..n_pages {
            cache
                .query_tokens(&TokenQuery {
                    chain,
                    addresses: None,
                    quality_range: QualityRange::min_only(51),
                    last_traded_ts_threshold: Some(threshold),
                    pagination: Some(PaginationParams::new(page, page_size)),
                })
                .unwrap();
        }
        let cache_sweep = started.elapsed();

        let sql_pages = n_pages.min(10);
        let started = Instant::now();
        for page in 0..sql_pages {
            gateway
                .get_tokens(
                    chain,
                    None,
                    QualityRange::min_only(51),
                    Some(threshold),
                    Some(&PaginationParams::new(page, page_size)),
                    &mut conn,
                )
                .await
                .expect("sql query failed");
        }
        let sql_sweep = started.elapsed();

        println!(
            "full sweep (q>=51, 30d, {n_pages} pages): cache {cache_sweep:?}; sql {:?} for first {sql_pages} pages (~{:.1?} extrapolated)",
            sql_sweep,
            sql_sweep * (n_pages.max(1) as u32) / (sql_pages.max(1) as u32),
        );
    }
}
