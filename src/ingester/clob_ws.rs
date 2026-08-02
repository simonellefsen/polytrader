//! P5 — live CLOB orderbook feed over WebSocket.
//!
//! This is the first real increment of the P5 item that the pre-registered go/no-go criterion
//! unblocked (Path B GO, 2026-07-25). It replaces the no-op skeleton that used to live at the
//! bottom of `clob_public.rs` and keeps the same two gates that skeleton was written behind:
//!
//! - **Compile gate**: Cargo feature `clob-ws` (off by default for a plain `cargo build`; the
//!   release image turns it on so the code is *present* and can be exercised live).
//! - **Runtime gate**: `POLYTRADER_ENABLE_CLOB_WS`, unset by default. Nothing here runs, connects,
//!   or allocates unless that variable is set to a recognized mode.
//!
//! ## What this does, and deliberately does not do
//!
//! It maintains live orderbooks in memory, audits them against REST ground truth, and — since
//! increment 2 — **prices the negRisk arb scanner's legs**. It still does not *execute*: orders
//! are placed by the existing paper engine on the 5-minute cycle, not reactively off a frame.
//!
//! That ordering was the point. Increment 1 shipped shadow-only precisely so the books could be
//! proven correct on live data before anything read them (92/92 REST agreement over the first
//! deploy), because a book that is silently wrong turns a "risk-free" basket into a directional
//! position. Increment 2 then let the scanner read them, with the fallback to polled snapshots
//! kept for any book the feed cannot vouch for.
//!
//! [`WsMode::Shadow`] remains the only implemented mode; the name now means "no reactive
//! execution" rather than "no readers". `POLYTRADER_ENABLE_CLOB_WS=highconviction` (the
//! reactive-execution mode the skeleton named) is still unimplemented and refuses to start.
//!
//! ## Protocol (verified live 2026-08-01, not from docs)
//!
//! Endpoint `wss://ws-subscriptions-clob.polymarket.com/ws/market`, public, no auth.
//! Subscribe by sending `{"assets_ids":["<token>",...],"type":"market"}`.
//!
//! - The server immediately replies with a **JSON array** of `event_type:"book"` objects, one per
//!   subscribed asset: `{market, asset_id, timestamp, hash, bids, asks, tick_size,
//!   last_trade_price, event_type}`.
//! - Thereafter it sends **single objects** with `event_type:"price_change"` carrying a
//!   `price_changes` array; each entry is `{asset_id, price, size, side, hash, best_bid,
//!   best_ask}`. `size:"0"` means *remove that level*, not "a level with zero size".
//! - Updates arrive for **both** tokens of a market when you subscribe to either one, so the
//!   complement's deltas show up unbidden; we ignore deltas for assets we hold no snapshot of,
//!   because a book assembled from deltas alone has never seen the levels that existed before we
//!   connected and is therefore not a book.
//! - Observed ordering: bids ascending by price, asks *descending*. We never rely on that. Levels
//!   go into a `BTreeMap` keyed by price, so best-of-book is a min/max on the map. This is the
//!   same class of bug as the one that made a `orderbook_snapshots.asks->0` read report a 0.066
//!   implied sum against the scanner's 1.021 — assuming feed ordering is the trap, so the data
//!   structure removes the assumption instead of documenting it.
//!
//! ## Gap detection without sequence numbers
//!
//! The feed gives no sequence number, so a dropped frame would silently corrupt the maintained
//! book forever. But every `price_change` entry carries the exchange's own `best_bid`/`best_ask`.
//! Comparing those against the best-of-book we derive from our map is a free, continuous
//! consistency check: they agree iff we have not missed a message that moved the top. Because a
//! frame can legitimately arrive behind the state it describes, a single mismatch is treated as a
//! race and only [`DESYNC_STRIKES`] in a row condemn the book — a real gap never heals, a race
//! resolves on the next frame. Condemned books are excluded from reads until a reconnect
//! re-snapshots them. Silent wrongness is the one failure mode a live feed must not have.

//! ## Why only half of this file is feature-gated
//!
//! The book model, the store and the frame parser compile unconditionally; only the socket itself
//! (`connect_and_pump`, `run_shard`, `spawn_feed`) sits behind `clob-ws`. That split exists so
//! readers like the arb scanner can take an `Option<&LiveBookStore>` without a `cfg` in the
//! strategy layer: with the feature off the store simply stays empty and every read falls back to
//! the polled snapshot, which is the same code path a read takes when a book is desynced. One
//! behaviour, type-checked in both builds, instead of two shapes of the scanner.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

#[cfg(feature = "clob-ws")]
use futures_util::{SinkExt, StreamExt};
#[cfg(feature = "clob-ws")]
use tokio_tungstenite::tungstenite::Message;

#[cfg(feature = "clob-ws")]
const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

#[cfg(feature = "clob-ws")]
/// Assets per connection. The server does not document a subscription cap, so rather than probe
/// for it we shard: several modest connections are also more robust than one fat one, since a
/// disconnect then costs a fraction of the universe instead of all of it.
const CHUNK_ASSETS: usize = 250;

#[cfg(feature = "clob-ws")]
/// Keepalive cadence. The server tolerated 75s of near-idle in testing, but Polymarket's own
/// clients ping, and an unanswered connection that looks alive to the OS is worse than a reconnect.
const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a *connection* may go silent before the books it carries stop being readable.
///
/// Deliberately measured against the shard, not the book — see [`LiveBookStore::shard_alive`].
/// Comfortably above the 10s keepalive, so an ordinary lull never trips it.
const STALE_AFTER_SECS: i64 = 120;

/// Consecutive best-of-book disagreements before a book is declared desynced.
///
/// Not 1. The `best_bid`/`best_ask` the exchange stamps on a frame is computed on its book at emit
/// time, so a frame in flight behind another can legitimately quote a top we have not been told
/// about yet — a transient race, not a lost message. A genuine gap never heals on its own, so it
/// keeps striking; a race resolves on the very next frame. Three is enough to tell them apart
/// without letting a real gap live long.
const DESYNC_STRIKES: u32 = 3;

/// Monotonic shard-id source. See the comment in [`spawn_feed`] for why ids must never be reused.
#[cfg(feature = "clob-ws")]
static NEXT_SHARD_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// ===========================================================================================
// Modes
// ===========================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsMode {
    /// Maintain live books and audit them against REST. No execution path reads them.
    Shadow,
}

impl WsMode {
    /// Parse the runtime gate. Unset/empty/unrecognized ⇒ `None` ⇒ nothing starts.
    ///
    /// `highconviction` is recognized-but-refused on purpose: it is the value the original
    /// skeleton documented for reactive execution, and silently treating it as shadow mode would
    /// be the worst outcome — an operator who set it believing execution was live, getting a
    /// feed that trades nothing and says nothing about it.
    pub fn from_env_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "shadow" => Some(WsMode::Shadow),
            "" => None,
            "highconviction" => {
                tracing::error!(
                    "POLYTRADER_ENABLE_CLOB_WS=highconviction requests reactive WS execution, \
                     which is NOT implemented — the live feed is shadow-only until its book \
                     accuracy is proven. Refusing to start rather than silently downgrading. \
                     Set POLYTRADER_ENABLE_CLOB_WS=shadow for the read-only feed."
                );
                None
            }
            other => {
                tracing::warn!(value = %other, "unrecognized POLYTRADER_ENABLE_CLOB_WS; WS feed stays off");
                None
            }
        }
    }
}

// ===========================================================================================
// Wire types + parsing (pure; this is where the tests live)
// ===========================================================================================

#[derive(Debug, Deserialize)]
struct RawLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct RawBook {
    asset_id: String,
    #[serde(default)]
    market: String,
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
}

#[derive(Debug, Deserialize)]
struct RawChange {
    asset_id: String,
    price: String,
    size: String,
    side: String,
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPriceChangeMsg {
    #[serde(default)]
    price_changes: Vec<RawChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub(crate) struct LevelChange {
    pub token_id: String,
    pub price: Decimal,
    pub size: Decimal,
    pub side: Side,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub(crate) enum WsEvent {
    Book {
        token_id: String,
        market_id: String,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
    },
    PriceChange(Vec<LevelChange>),
}

fn dec(s: &str) -> Option<Decimal> {
    Decimal::from_str(s.trim()).ok()
}

fn levels(raw: &[RawLevel]) -> Vec<(Decimal, Decimal)> {
    raw.iter()
        .filter_map(|l| Some((dec(&l.price)?, dec(&l.size)?)))
        .collect()
}

/// Turn one WS text frame into zero or more events.
///
/// Frames are either a JSON array (the initial burst of book snapshots) or a single object
/// (everything after), so both shapes normalize to a slice before dispatch. Event types we do not
/// consume (`tick_size_change`, `last_trade_price`) are skipped without ceremony; a frame that is
/// not JSON at all is an error worth surfacing, because it means the protocol moved.
pub(crate) fn parse_frame(raw: &str) -> Result<Vec<WsEvent>> {
    let v: serde_json::Value =
        serde_json::from_str(raw).context("CLOB WS frame was not valid JSON")?;
    let items: Vec<serde_json::Value> = match v {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };

    let mut out = Vec::new();
    for item in items {
        let kind = item
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "book" => {
                if let Ok(b) = serde_json::from_value::<RawBook>(item) {
                    out.push(WsEvent::Book {
                        token_id: b.asset_id,
                        market_id: b.market,
                        bids: levels(&b.bids),
                        asks: levels(&b.asks),
                    });
                }
            }
            "price_change" => {
                if let Ok(m) = serde_json::from_value::<RawPriceChangeMsg>(item) {
                    let changes: Vec<LevelChange> = m
                        .price_changes
                        .iter()
                        .filter_map(|c| {
                            let side = match c.side.to_ascii_uppercase().as_str() {
                                "BUY" => Side::Buy,
                                "SELL" => Side::Sell,
                                _ => return None,
                            };
                            Some(LevelChange {
                                token_id: c.asset_id.clone(),
                                price: dec(&c.price)?,
                                size: dec(&c.size)?,
                                side,
                                best_bid: c.best_bid.as_deref().and_then(dec),
                                best_ask: c.best_ask.as_deref().and_then(dec),
                            })
                        })
                        .collect();
                    if !changes.is_empty() {
                        out.push(WsEvent::PriceChange(changes));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

// ===========================================================================================
// The maintained book
// ===========================================================================================

#[derive(Debug, Clone)]
// The identity fields and the richer accessors below are the read surface the execution tier will
// use (leg → market grouping, mid marks, depth-aware sizing). Shadow mode only needs best-of-book,
// so they read as dead until that tier lands; they are tested, not speculative.
#[allow(dead_code)]
pub struct LiveBook {
    pub token_id: String,
    pub market_id: String,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    pub updated_at: DateTime<Utc>,
    /// Set when the feed's own best-of-book disagreed with ours for [`DESYNC_STRIKES`] frames
    /// running: we missed a message and this book can no longer be trusted until a reconnect
    /// re-snapshots it.
    pub desynced: bool,
    /// Consecutive frames whose stated best-of-book disagreed with ours. Reset by any agreement.
    strikes: u32,
    /// Which connection carries this book. Freshness is a property of the connection, not of the
    /// book — see [`LiveBookStore::shard_alive`].
    shard: usize,
}

#[allow(dead_code)] // see the note on the struct
impl LiveBook {
    /// Highest resting bid. Read off the map, never off feed ordering.
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.keys().next_back().copied()
    }

    /// Lowest resting ask — the price a buy-all-No basket actually pays.
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.keys().next().copied()
    }

    /// Best ask together with the size resting at it, which is what bounds basket units.
    ///
    /// Depth has to come from the same read as the price. Taking the price live and the size from
    /// a polled snapshot would size a basket against liquidity that no longer exists — the exact
    /// mis-fill that turns a "risk-free" basket into a directional position.
    pub fn best_ask_with_size(&self) -> Option<(Decimal, Decimal)> {
        self.asks.iter().next().map(|(p, s)| (*p, *s))
    }

    pub fn mid(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) if a > b => Some((b + a) / Decimal::from(2)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            _ => None,
        }
    }

    pub fn depth(&self) -> (usize, usize) {
        (self.bids.len(), self.asks.len())
    }

    /// Every resting bid level, `(price, size)`.
    pub fn bid_levels(&self) -> Vec<(Decimal, Decimal)> {
        self.bids.iter().map(|(p, s)| (*p, *s)).collect()
    }

    /// Every resting ask level, `(price, size)`. The paper matcher sorts what it is handed, so the
    /// order here is not load-bearing — but it comes out ascending, i.e. genuinely best-first,
    /// unlike the REST payload.
    pub fn ask_levels(&self) -> Vec<(Decimal, Decimal)> {
        self.asks.iter().map(|(p, s)| (*p, *s)).collect()
    }

    pub fn age_secs(&self) -> i64 {
        (Utc::now() - self.updated_at).num_seconds()
    }

    fn apply_level(&mut self, side: Side, price: Decimal, size: Decimal) {
        let book = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        // size 0 is a deletion, not a zero-size level. Treating it as a level would leave a phantom
        // at the top of the book and quote a price nobody is offering.
        if size.is_zero() {
            book.remove(&price);
        } else {
            book.insert(price, size);
        }
    }
}

// ===========================================================================================
// Store
// ===========================================================================================

#[derive(Debug, Default)]
struct Counters {
    frames: AtomicU64,
    books: AtomicU64,
    changes: AtomicU64,
    desyncs: AtomicU64,
    reconnects: AtomicU64,
    orphan_changes: AtomicU64,
}

/// Shared, cheaply cloneable handle to every book the feed is maintaining.
#[derive(Clone, Default)]
pub struct LiveBookStore {
    books: Arc<RwLock<HashMap<String, LiveBook>>>,
    /// Last time each shard delivered anything. This — not per-book age — is the liveness signal.
    shard_seen: Arc<RwLock<HashMap<usize, DateTime<Utc>>>>,
    counters: Arc<Counters>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreHealth {
    pub tracked: usize,
    pub fresh: usize,
    pub desynced: usize,
    pub stale: usize,
    pub frames: u64,
    pub book_events: u64,
    pub change_events: u64,
    pub desync_events: u64,
    pub reconnects: u64,
    pub orphan_changes: u64,
}

impl LiveBookStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a shard has delivered anything recently.
    ///
    /// Freshness belongs here rather than on the book, and the distinction is not academic: a
    /// quiet market emits no deltas for minutes at a time, so per-book age marks a perfectly
    /// accurate book as stale purely for being calm. Measured live on the first deploy, that
    /// mistake condemned 18 of 488 books — and quiet, wide books are disproportionately the
    /// negRisk ladder legs the arb scanner exists to read. What actually invalidates a book is the
    /// connection carrying it going silent, because then we cannot know whether it moved.
    pub fn shard_alive(&self, shard: usize) -> bool {
        self.shard_seen
            .read()
            .ok()
            .and_then(|g| g.get(&shard).copied())
            .is_some_and(|t| (Utc::now() - t).num_seconds() <= STALE_AFTER_SECS)
    }

    fn is_readable(&self, b: &LiveBook) -> bool {
        !b.desynced && self.shard_alive(b.shard)
    }

    /// A book is only returned when it is live and in sync — a caller can never accidentally read
    /// a book we know to be wrong, or one whose connection has gone quiet.
    pub fn get_fresh(&self, token_id: &str) -> Option<LiveBook> {
        let b = self.books.read().ok()?.get(token_id).cloned()?;
        self.is_readable(&b).then_some(b)
    }

    /// Drop every book outside `keep`, and forget shards that no longer carry one.
    ///
    /// Called on resubscribe. The store was append-only before this existed: measured over one
    /// 7-hour run the tracked set grew 495 → 653 against a 500-token cap, i.e. ~150 books for
    /// tokens nobody was subscribed to any more, silently answering reads with hours-old data.
    /// The REST audit caught it as a rising disagreement rate — 0.4% in the first hour to 5.0% by
    /// the seventh — which is precisely the job that audit was built for.
    pub fn retain_tokens(&self, keep: &std::collections::HashSet<String>) {
        let live_shards: std::collections::HashSet<usize> = match self.books.write() {
            Ok(mut g) => {
                g.retain(|token, _| keep.contains(token));
                g.values().map(|b| b.shard).collect()
            }
            Err(_) => return,
        };
        if let Ok(mut s) = self.shard_seen.write() {
            s.retain(|id, _| live_shards.contains(id));
        }
    }

    pub fn tracked_tokens(&self) -> Vec<String> {
        self.books
            .read()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn apply(&self, shard: usize, ev: WsEvent) {
        // Applying an event IS the evidence that this shard is alive.
        if let Ok(mut s) = self.shard_seen.write() {
            s.insert(shard, Utc::now());
        }
        let Ok(mut g) = self.books.write() else {
            return;
        };
        match ev {
            WsEvent::Book {
                token_id,
                market_id,
                bids,
                asks,
            } => {
                self.counters.books.fetch_add(1, Ordering::Relaxed);
                // A snapshot is the authority: it replaces whatever we had and clears any desync,
                // which is exactly why reconnecting is the resync mechanism.
                g.insert(
                    token_id.clone(),
                    LiveBook {
                        token_id,
                        market_id,
                        bids: bids.into_iter().filter(|(_, s)| !s.is_zero()).collect(),
                        asks: asks.into_iter().filter(|(_, s)| !s.is_zero()).collect(),
                        updated_at: Utc::now(),
                        desynced: false,
                        strikes: 0,
                        shard,
                    },
                );
            }
            WsEvent::PriceChange(changes) => {
                self.counters.changes.fetch_add(1, Ordering::Relaxed);

                // Two phases, because a frame's stated best-of-book describes the book AFTER every
                // change in that frame. Checking mid-batch would flag a book as desynced for the
                // entirely normal case of a frame that moves the top and then adds a level behind
                // it — the check has to see the same state the exchange was describing.
                let mut stated: HashMap<String, (Option<Decimal>, Option<Decimal>)> =
                    HashMap::new();
                for c in changes {
                    let Some(book) = g.get_mut(&c.token_id) else {
                        // The complement token of a subscribed market, or an asset whose snapshot
                        // we never received. Deltas cannot bootstrap a book, so this is dropped
                        // rather than accumulated into a plausible-looking fiction.
                        self.counters.orphan_changes.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    book.apply_level(c.side, c.price, c.size);
                    book.updated_at = Utc::now();
                    let e = stated.entry(c.token_id).or_insert((None, None));
                    e.0 = c.best_bid.or(e.0);
                    e.1 = c.best_ask.or(e.1);
                }

                for (token, (bid, ask)) in stated {
                    let Some(book) = g.get_mut(&token) else {
                        continue;
                    };
                    if book.desynced {
                        continue;
                    }
                    if !feed_best_disagrees(book, bid, ask) {
                        book.strikes = 0;
                        continue;
                    }
                    book.strikes += 1;
                    if book.strikes < DESYNC_STRIKES {
                        continue;
                    }
                    book.desynced = true;
                    self.counters.desyncs.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        token = %token,
                        our_bid = ?book.best_bid(), feed_bid = ?bid,
                        our_ask = ?book.best_ask(), feed_ask = ?ask,
                        strikes = book.strikes,
                        "CLOB WS book desynced (feed best-of-book disagreed {DESYNC_STRIKES}x \
                         running) — excluded from reads until a reconnect re-snapshots it"
                    );
                }
            }
        }
    }

    fn note_frame(&self) {
        self.counters.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// Backdate a shard's last-seen and every book's last-update, so the freshness rules can be
    /// tested without a two-minute sleep.
    #[cfg(test)]
    fn backdate(&self, shard_secs: i64, book_secs: i64) {
        if let Ok(mut s) = self.shard_seen.write() {
            for t in s.values_mut() {
                *t = Utc::now() - chrono::Duration::seconds(shard_secs);
            }
        }
        if let Ok(mut g) = self.books.write() {
            for b in g.values_mut() {
                b.updated_at = Utc::now() - chrono::Duration::seconds(book_secs);
            }
        }
    }

    fn note_reconnect(&self) {
        self.counters.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn health(&self) -> StoreHealth {
        let books: Vec<LiveBook> = self
            .books
            .read()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default();
        let tracked = books.len();
        let mut fresh = 0;
        let mut desynced = 0;
        let mut stale = 0;
        for b in &books {
            if b.desynced {
                desynced += 1;
            } else if !self.shard_alive(b.shard) {
                stale += 1;
            } else {
                fresh += 1;
            }
        }
        StoreHealth {
            tracked,
            fresh,
            desynced,
            stale,
            frames: self.counters.frames.load(Ordering::Relaxed),
            book_events: self.counters.books.load(Ordering::Relaxed),
            change_events: self.counters.changes.load(Ordering::Relaxed),
            desync_events: self.counters.desyncs.load(Ordering::Relaxed),
            reconnects: self.counters.reconnects.load(Ordering::Relaxed),
            orphan_changes: self.counters.orphan_changes.load(Ordering::Relaxed),
        }
    }
}

/// The gap detector. Only fires when the feed actually states a best price *and* it differs from
/// ours; a missing `best_bid`/`best_ask` in the frame is not evidence of anything.
///
/// One asymmetry is deliberate: the exchange reports `best_bid:"0"` / `best_ask:"1"` for a side
/// with nothing resting, which is a sentinel rather than a price, and our map correctly holds
/// `None` there. Treating that pair as a disagreement would desync every one-sided book on its
/// first delta.
pub(crate) fn feed_best_disagrees(
    book: &LiveBook,
    feed_bid: Option<Decimal>,
    feed_ask: Option<Decimal>,
) -> bool {
    let empty_bid = Decimal::ZERO;
    let empty_ask = Decimal::ONE;

    if let Some(fb) = feed_bid {
        let ours = book.best_bid();
        if !(fb == empty_bid && ours.is_none()) && ours != Some(fb) {
            return true;
        }
    }
    if let Some(fa) = feed_ask {
        let ours = book.best_ask();
        if !(fa == empty_ask && ours.is_none()) && ours != Some(fa) {
            return true;
        }
    }
    false
}

// ===========================================================================================
// Connection
// ===========================================================================================

#[cfg(feature = "clob-ws")]
/// Backoff for a failed or dropped connection: exponential, capped, with jitter so that N sharded
/// connections dropped by one server-side event do not all come back in the same instant.
fn backoff_delay(attempt: u32) -> std::time::Duration {
    let base = 500u64.saturating_mul(1 << attempt.min(6)); // 0.5s → 32s
    let capped = base.min(30_000);
    // Cheap jitter without pulling in `rand`: nanosecond clock noise, ±25%.
    let noise = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let spread = capped / 4;
    let jitter = if spread == 0 { 0 } else { noise % (spread * 2) };
    std::time::Duration::from_millis(capped.saturating_sub(spread).saturating_add(jitter))
}

#[cfg(feature = "clob-ws")]
/// One long-lived sharded connection: connect, subscribe, pump messages into the store, ping,
/// and reconnect forever. Returns only if the task is dropped.
async fn run_shard(store: LiveBookStore, assets: Vec<String>, shard: usize) {
    let mut attempt: u32 = 0;
    loop {
        match connect_and_pump(&store, &assets, shard).await {
            Ok(()) => {
                tracing::warn!(
                    shard,
                    assets = assets.len(),
                    "CLOB WS stream ended; reconnecting"
                );
                attempt = 0;
            }
            Err(e) => {
                tracing::warn!(shard, error = %e, attempt, "CLOB WS connection failed; backing off");
                attempt = attempt.saturating_add(1);
            }
        }
        store.note_reconnect();
        tokio::time::sleep(backoff_delay(attempt)).await;
    }
}

#[cfg(feature = "clob-ws")]
async fn connect_and_pump(store: &LiveBookStore, assets: &[String], shard: usize) -> Result<()> {
    let (ws, _resp) = tokio_tungstenite::connect_async(WS_URL)
        .await
        .context("CLOB WS connect failed")?;
    let (mut tx, mut rx) = ws.split();

    let sub = serde_json::json!({ "assets_ids": assets, "type": "market" });
    tx.send(Message::Text(sub.to_string()))
        .await
        .context("CLOB WS subscribe send failed")?;
    tracing::info!(
        shard,
        assets = assets.len(),
        "CLOB WS connected and subscribed"
    );

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await; // the first tick is immediate; we just subscribed

    loop {
        tokio::select! {
            _ = ping.tick() => {
                // Polymarket's keepalive is a literal text PING, not a WS control frame.
                if let Err(e) = tx.send(Message::Text("PING".to_string())).await {
                    return Err(anyhow::anyhow!("CLOB WS ping failed: {e}"));
                }
            }
            msg = rx.next() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg.context("CLOB WS read failed")? {
                    Message::Text(t) => {
                        store.note_frame();
                        // The PONG reply is not JSON; skipping it here keeps parse errors
                        // meaningful instead of one per keepalive.
                        if t.trim().eq_ignore_ascii_case("PONG") { continue; }
                        match parse_frame(&t) {
                            Ok(events) => for ev in events { store.apply(shard, ev); },
                            Err(e) => tracing::warn!(shard, error = %e, preview = %t.chars().take(120).collect::<String>(), "CLOB WS frame parse failed"),
                        }
                    }
                    Message::Ping(p) => { let _ = tx.send(Message::Pong(p)).await; }
                    Message::Close(c) => {
                        tracing::info!(shard, ?c, "CLOB WS closed by server");
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(feature = "clob-ws")]
/// Split a universe into shards and spawn one resilient connection per shard.
///
/// The returned handles are the only way to stop a shard: each runs an unbounded reconnect loop by
/// design (a feed that gives up is worse than one that keeps retrying), so the caller aborts them
/// when the subscribed universe rotates and a fresh set is needed.
pub fn spawn_feed(store: LiveBookStore, assets: Vec<String>) -> Vec<tokio::task::JoinHandle<()>> {
    // Drop books for tokens this subscription no longer covers. Without this the store is
    // append-only across resubscribes and an unsubscribed token keeps its last-known book forever.
    store.retain_tokens(&assets.iter().cloned().collect());

    assets
        .chunks(CHUNK_ASSETS)
        .map(|chunk| {
            // Shard ids are globally monotonic, never 0..n per subscription. Reusing 0/1 each time
            // was the teeth of the same bug: an orphaned book still pointing at "shard 0" looked
            // alive because the NEXT subscription's shard 0 was delivering, so a book that had
            // stopped updating hours ago still passed `get_fresh`. A fresh id per connection means
            // any book left behind by a resubscribe points at a shard that is dead by definition.
            let shard = NEXT_SHARD_ID.fetch_add(1, Ordering::Relaxed);
            let store = store.clone();
            let chunk = chunk.to_vec();
            tokio::spawn(async move { run_shard(store, chunk, shard).await })
        })
        .collect()
}

// ===========================================================================================
// Shadow audit — the proof the maintained book is actually right
// ===========================================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditResult {
    pub sampled: usize,
    pub agreed: usize,
    pub disagreed: usize,
    /// Sampled token had no live REST book to compare against (404). Not a failure of ours.
    pub no_rest_book: usize,
    pub worst_ask_delta_bps: Option<i64>,
}

/// Compare a sample of maintained books against a fresh REST `/book` fetch.
///
/// This is the honest test, and it is deliberately *not* "compare WS against the 5-minute
/// snapshot table" — that comparison always shows a difference (the snapshot is up to five
/// minutes old) and so proves nothing about correctness either way. Fetching ground truth at the
/// moment of comparison is the only version of this check that can fail for the right reason.
pub async fn audit_against_rest(
    store: &LiveBookStore,
    clob: &super::ClobPublicClient,
    sample: usize,
) -> AuditResult {
    let mut out = AuditResult::default();
    let tokens = store.tracked_tokens();
    if tokens.is_empty() {
        return out;
    }
    // Rotate the sample window by wall-clock so successive audits cover different tokens without
    // holding any cursor state.
    let start = (Utc::now().timestamp() as usize).wrapping_mul(7) % tokens.len();

    for i in 0..sample.min(tokens.len()) {
        let token = &tokens[(start + i) % tokens.len()];
        let Some(ours) = store.get_fresh(token) else {
            continue;
        };
        let rest = match clob.get_orderbook(token).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                out.no_rest_book += 1;
                continue;
            }
            Err(e) => {
                tracing::debug!(token = %token, error = %e, "audit REST fetch failed; skipping");
                continue;
            }
        };
        out.sampled += 1;

        let rest_ask = rest
            .asks
            .iter()
            .filter_map(|l| Decimal::from_str(&l.price).ok())
            .min();
        let rest_bid = rest
            .bids
            .iter()
            .filter_map(|l| Decimal::from_str(&l.price).ok())
            .max();

        if rest_ask == ours.best_ask() && rest_bid == ours.best_bid() {
            out.agreed += 1;
        } else {
            out.disagreed += 1;
            if let (Some(r), Some(o)) = (rest_ask, ours.best_ask()) {
                let bps = ((o - r) * Decimal::from(10_000))
                    .round()
                    .to_i64()
                    .unwrap_or(0)
                    .abs();
                if out.worst_ask_delta_bps.is_none_or(|w| bps > w) {
                    out.worst_ask_delta_bps = Some(bps);
                }
            }
            tracing::debug!(
                token = %token, ours_bid = ?ours.best_bid(), rest_bid = ?rest_bid,
                ours_ask = ?ours.best_ask(), rest_ask = ?rest_ask,
                "CLOB WS audit disagreement"
            );
        }
    }
    out
}

// ===========================================================================================
// Tests
// ===========================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Captured verbatim from the live endpoint 2026-08-01 (trimmed to 3 levels a side). Using a
    /// real frame rather than a hand-written one is the point: the shape being tested is the shape
    /// the exchange actually sends, including asks arriving in descending order.
    const REAL_BOOK_FRAME: &str = r#"[{"market":"0xbd28e79d","asset_id":"17026652800646939086597854837636530894046674466009065464250490419206140378789","timestamp":"1785615724645","hash":"fc903f","bids":[{"price":"0.001","size":"1000"},{"price":"0.94","size":"500"},{"price":"0.951","size":"1016.87"}],"asks":[{"price":"0.999","size":"12341"},{"price":"0.97","size":"3802.3"},{"price":"0.952","size":"5"}],"tick_size":"0.001","event_type":"book","last_trade_price":"0.95"}]"#;

    const REAL_CHANGE_FRAME: &str = r#"{"market":"0xbd28e79d", "price_changes":[{"asset_id":"17026652800646939086597854837636530894046674466009065464250490419206140378789", "price":"0.921", "size":"40", "side":"BUY", "hash":"940e83", "best_bid":"0.951", "best_ask":"0.952"}, {"asset_id":"28032304910613282558446337092297637171175507983730585546859039383651650095917", "price":"0.079", "size":"40", "side":"SELL", "hash":"f8f485", "best_bid":"0.048", "best_ask":"0.049"}], "timestamp":"1785615736984", "event_type":"price_change"}"#;

    const TOKEN: &str =
        "17026652800646939086597854837636530894046674466009065464250490419206140378789";

    fn store_with_real_book() -> LiveBookStore {
        let s = LiveBookStore::new();
        for ev in parse_frame(REAL_BOOK_FRAME).unwrap() {
            s.apply(0, ev);
        }
        s
    }

    #[test]
    fn best_of_book_ignores_the_order_the_feed_sent_levels_in() {
        let s = store_with_real_book();
        let b = s.get_fresh(TOKEN).expect("book");
        // The frame lists asks 0.999, 0.97, 0.952 — best LAST. A `asks[0]` read would quote 0.999.
        assert_eq!(b.best_ask(), Some(dec!(0.952)));
        assert_eq!(b.best_bid(), Some(dec!(0.951)));
        assert_eq!(b.depth(), (3, 3));
    }

    #[test]
    fn a_zero_size_change_deletes_the_level_rather_than_resting_at_zero() {
        let s = store_with_real_book();
        s.apply(
            0,
            WsEvent::PriceChange(vec![LevelChange {
                token_id: TOKEN.into(),
                price: dec!(0.952),
                size: Decimal::ZERO,
                side: Side::Sell,
                best_bid: None,
                best_ask: None,
            }]),
        );
        let b = s.get_fresh(TOKEN).expect("book");
        // The old best is gone entirely; the next real ask takes over.
        assert_eq!(b.best_ask(), Some(dec!(0.97)));
        assert_eq!(b.depth(), (3, 2));
    }

    #[test]
    fn a_new_inside_ask_becomes_the_best_ask() {
        let s = store_with_real_book();
        s.apply(
            0,
            WsEvent::PriceChange(vec![LevelChange {
                token_id: TOKEN.into(),
                price: dec!(0.953),
                size: dec!(10),
                side: Side::Sell,
                best_bid: None,
                best_ask: None,
            }]),
        );
        // 0.953 is worse than 0.952, so it must NOT take the top.
        assert_eq!(s.get_fresh(TOKEN).unwrap().best_ask(), Some(dec!(0.952)));

        s.apply(
            0,
            WsEvent::PriceChange(vec![LevelChange {
                token_id: TOKEN.into(),
                price: dec!(0.9),
                size: dec!(10),
                side: Side::Sell,
                best_bid: None,
                best_ask: None,
            }]),
        );
        assert_eq!(s.get_fresh(TOKEN).unwrap().best_ask(), Some(dec!(0.9)));
    }

    #[test]
    fn deltas_for_an_unknown_asset_never_fabricate_a_book() {
        let s = store_with_real_book();
        for ev in parse_frame(REAL_CHANGE_FRAME).unwrap() {
            s.apply(0, ev);
        }
        // The real frame carries the complement token too. We never snapshotted it, so it must not
        // appear — a book made of deltas alone is missing every pre-connection level.
        let h = s.health();
        assert_eq!(h.tracked, 1, "complement token must not materialize a book");
        assert_eq!(h.orphan_changes, 1);
        // The subscribed side did apply.
        assert_eq!(s.get_fresh(TOKEN).unwrap().depth(), (4, 3));
    }

    #[test]
    fn a_frame_whose_best_matches_ours_does_not_desync() {
        let s = store_with_real_book();
        // The real frame states best_bid 0.951 / best_ask 0.952, which is exactly our book, and the
        // 0.921 bid it adds is deep — the top does not move.
        for ev in parse_frame(REAL_CHANGE_FRAME).unwrap() {
            s.apply(0, ev);
        }
        assert!(s.get_fresh(TOKEN).is_some());
        assert_eq!(s.health().desync_events, 0);
    }

    /// A frame that claims a best ask we were never told how to reach. The level it actually
    /// carries is a deep BID on purpose: it must not move our top, or the test would be measuring
    /// its own side effect instead of the missed message.
    fn frame_claiming_ask(claimed: Decimal) -> WsEvent {
        WsEvent::PriceChange(vec![LevelChange {
            token_id: TOKEN.into(),
            price: dec!(0.30),
            size: dec!(5),
            side: Side::Buy,
            best_bid: Some(dec!(0.951)),
            best_ask: Some(claimed),
        }])
    }

    fn stale_top_frame() -> WsEvent {
        frame_claiming_ask(dec!(0.94))
    }

    #[test]
    fn a_missed_frame_is_caught_by_the_feeds_own_best_of_book() {
        let s = store_with_real_book();
        // Simulate a dropped message: the exchange says the best ask is now 0.94, but we never saw
        // the frame that put it there, so our map still tops out at 0.952. A real gap never heals,
        // so it keeps striking until the book is condemned.
        for _ in 0..DESYNC_STRIKES {
            s.apply(0, stale_top_frame());
        }
        assert_eq!(s.health().desync_events, 1);
        // And the desynced book is withheld from readers rather than quoted wrongly.
        assert!(
            s.get_fresh(TOKEN).is_none(),
            "a book known to be wrong must not be readable"
        );
    }

    #[test]
    fn a_transient_disagreement_that_resolves_does_not_condemn_the_book() {
        let s = store_with_real_book();
        // Frames can arrive behind the state they describe, so one disagreement is a race, not a
        // gap. Strike twice — one short of the threshold...
        s.apply(0, stale_top_frame());
        s.apply(0, stale_top_frame());
        assert_eq!(s.health().desync_events, 0);
        // ...then the delayed frame lands and the top agrees again.
        s.apply(
            0,
            WsEvent::PriceChange(vec![LevelChange {
                token_id: TOKEN.into(),
                price: dec!(0.94),
                size: dec!(100),
                side: Side::Sell,
                best_bid: Some(dec!(0.951)),
                best_ask: Some(dec!(0.94)),
            }]),
        );
        assert_eq!(s.get_fresh(TOKEN).unwrap().best_ask(), Some(dec!(0.94)));
        // The strike count must have reset, or a later isolated race would tip an in-sync book
        // over on its first strike. Two fresh disagreements must still be survivable.
        s.apply(0, frame_claiming_ask(dec!(0.90)));
        s.apply(0, frame_claiming_ask(dec!(0.90)));
        assert_eq!(s.health().desync_events, 0);
        assert!(s.get_fresh(TOKEN).is_some());
    }

    #[test]
    fn a_frames_stated_best_is_checked_against_the_whole_batch_not_each_change() {
        let s = store_with_real_book();
        // One frame, two changes for the same token, in the order the exchange may well send them:
        // an unrelated deep bid first, the ask that actually moves the top second. Both entries
        // carry best_ask 0.94 — the state AFTER the whole batch. Checked change-by-change, the
        // first entry would compare a stated 0.94 against our still-unmoved 0.952 and strike.
        let frame = || {
            WsEvent::PriceChange(vec![
                LevelChange {
                    token_id: TOKEN.into(),
                    price: dec!(0.30),
                    size: dec!(5),
                    side: Side::Buy,
                    best_bid: Some(dec!(0.951)),
                    best_ask: Some(dec!(0.94)),
                },
                LevelChange {
                    token_id: TOKEN.into(),
                    price: dec!(0.94),
                    size: dec!(100),
                    side: Side::Sell,
                    best_bid: Some(dec!(0.951)),
                    best_ask: Some(dec!(0.94)),
                },
            ])
        };
        for _ in 0..DESYNC_STRIKES + 2 {
            s.apply(0, frame());
        }
        assert_eq!(
            s.health().desync_events,
            0,
            "a batched frame must be judged on its end state, not its intermediate one"
        );
        assert_eq!(s.get_fresh(TOKEN).unwrap().best_ask(), Some(dec!(0.94)));
    }

    #[test]
    fn an_empty_side_sentinel_is_not_a_disagreement() {
        // The exchange reports best_bid "0" / best_ask "1" when a side is empty. Our map holds
        // None. Naive equality would desync every one-sided book on its first delta — and thin
        // one-sided books are exactly the arb-discovery pool.
        let s = LiveBookStore::new();
        s.apply(
            0,
            WsEvent::Book {
                token_id: TOKEN.into(),
                market_id: "0x".into(),
                bids: vec![],
                asks: vec![(dec!(0.6), dec!(10))],
            },
        );
        s.apply(
            0,
            WsEvent::PriceChange(vec![LevelChange {
                token_id: TOKEN.into(),
                price: dec!(0.6),
                size: dec!(12),
                side: Side::Sell,
                best_bid: Some(Decimal::ZERO),
                best_ask: Some(dec!(0.6)),
            }]),
        );
        assert_eq!(s.health().desync_events, 0);
        assert!(s.get_fresh(TOKEN).is_some());
    }

    #[test]
    fn a_reconnect_snapshot_clears_a_desync() {
        let s = store_with_real_book();
        for _ in 0..DESYNC_STRIKES {
            s.apply(0, stale_top_frame());
        }
        assert!(s.get_fresh(TOKEN).is_none());
        // Reconnecting re-sends full books; that is the resync mechanism.
        for ev in parse_frame(REAL_BOOK_FRAME).unwrap() {
            s.apply(0, ev);
        }
        assert_eq!(s.get_fresh(TOKEN).unwrap().best_ask(), Some(dec!(0.952)));
        assert_eq!(s.health().desynced, 0);
    }

    #[test]
    fn a_pong_or_unknown_event_type_yields_no_events_but_is_not_an_error() {
        assert!(
            parse_frame(r#"{"event_type":"tick_size_change","market":"0x"}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_frame(r#"{"event_type":"last_trade_price"}"#)
            .unwrap()
            .is_empty());
        // Genuine garbage is still an error — a silently-swallowed protocol change is how a feed
        // goes quietly wrong.
        assert!(parse_frame("PONG").is_err());
    }

    #[test]
    fn zero_size_levels_in_a_snapshot_are_dropped_on_ingest() {
        let s = LiveBookStore::new();
        s.apply(
            0,
            WsEvent::Book {
                token_id: TOKEN.into(),
                market_id: "0x".into(),
                bids: vec![(dec!(0.4), dec!(0)), (dec!(0.3), dec!(5))],
                asks: vec![(dec!(0.6), dec!(0))],
            },
        );
        let b = s.get_fresh(TOKEN).unwrap();
        assert_eq!(
            b.best_bid(),
            Some(dec!(0.3)),
            "phantom 0.4 bid must not top the book"
        );
        assert_eq!(b.best_ask(), None);
    }

    #[test]
    fn a_quiet_book_on_a_live_connection_stays_readable() {
        // Measured live: per-book age condemned 18 of 488 books that were entirely correct — they
        // were simply calm. A market with no orders arriving emits no deltas, and silence from a
        // market is not the same as silence from the feed. Wide, quiet books are also
        // disproportionately the negRisk ladder legs the arb scanner exists to read, so getting
        // this wrong would blind it to exactly its own universe.
        let s = store_with_real_book();
        s.backdate(1, 10 * STALE_AFTER_SECS);
        assert!(
            s.get_fresh(TOKEN).is_some(),
            "a long-quiet book on a live connection is accurate, not stale"
        );
        assert_eq!(s.health().fresh, 1);
        assert_eq!(s.health().stale, 0);
    }

    #[test]
    fn every_book_goes_unreadable_when_its_connection_goes_silent() {
        // The converse, and the reason freshness is tracked at all: if the connection stopped
        // delivering, we cannot know whether the book moved, however recently we updated it.
        let s = store_with_real_book();
        s.backdate(STALE_AFTER_SECS + 1, 0);
        assert!(s.get_fresh(TOKEN).is_none());
        let h = s.health();
        assert_eq!((h.fresh, h.stale), (0, 1));
    }

    #[test]
    fn a_resubscribe_evicts_books_it_no_longer_covers() {
        // The store was append-only across resubscribes: measured live, tracked grew 495 → 653
        // against a 500-token cap. Retaining only the new set is the cleanup half of the fix.
        let s = store_with_real_book();
        s.apply(
            0,
            WsEvent::Book {
                token_id: "dropped".into(),
                market_id: "0x".into(),
                bids: vec![(dec!(0.4), dec!(5))],
                asks: vec![(dec!(0.6), dec!(5))],
            },
        );
        assert_eq!(s.health().tracked, 2);

        s.retain_tokens(&[TOKEN.to_string()].into_iter().collect());
        assert_eq!(s.health().tracked, 1);
        assert!(s.get_fresh("dropped").is_none());
        assert!(s.get_fresh(TOKEN).is_some(), "the covered book survives");
    }

    #[test]
    fn a_book_left_on_a_retired_shard_is_never_readable() {
        // The teeth of the same bug, and the half that made it silent. Shard ids used to restart
        // at 0 on every resubscribe, so an orphaned book pointing at "shard 0" looked alive the
        // moment the NEXT subscription's shard 0 delivered a frame — a book frozen hours ago
        // passing `get_fresh` as current. Ids are now monotonic, so a retired shard stays dead
        // even if this eviction is somehow missed.
        let s = LiveBookStore::new();
        s.apply(
            7,
            WsEvent::Book {
                token_id: TOKEN.into(),
                market_id: "0x".into(),
                bids: vec![(dec!(0.4), dec!(5))],
                asks: vec![(dec!(0.6), dec!(5))],
            },
        );
        assert!(s.get_fresh(TOKEN).is_some());

        // A different shard now carries the feed. The orphan must NOT inherit its liveness.
        s.backdate(STALE_AFTER_SECS + 1, 0);
        s.apply(
            8,
            WsEvent::Book {
                token_id: "other".into(),
                market_id: "0x".into(),
                bids: vec![(dec!(0.4), dec!(5))],
                asks: vec![(dec!(0.6), dec!(5))],
            },
        );
        assert!(s.get_fresh("other").is_some(), "the live shard is readable");
        assert!(
            s.get_fresh(TOKEN).is_none(),
            "a book on a retired shard must not read as fresh"
        );
    }

    #[test]
    fn the_runtime_gate_refuses_the_unimplemented_execution_mode() {
        assert_eq!(WsMode::from_env_value("shadow"), Some(WsMode::Shadow));
        assert_eq!(WsMode::from_env_value(" SHADOW "), Some(WsMode::Shadow));
        assert_eq!(WsMode::from_env_value(""), None);
        // Recognized-but-refused, not silently downgraded to shadow.
        assert_eq!(WsMode::from_env_value("highconviction"), None);
        assert_eq!(WsMode::from_env_value("1"), None);
        assert_eq!(WsMode::from_env_value("true"), None);
    }

    #[cfg(feature = "clob-ws")]
    #[test]
    fn backoff_grows_then_caps_and_always_carries_jitter() {
        let d0 = backoff_delay(0).as_millis();
        let d3 = backoff_delay(3).as_millis();
        assert!(d0 < d3, "backoff must grow: {d0} vs {d3}");
        for attempt in [6u32, 10, 50, u32::MAX] {
            let ms = backoff_delay(attempt).as_millis();
            // Cap 30s ±25% jitter, and the shift must not overflow at large attempt counts.
            assert!(
                (22_500..=37_500).contains(&ms),
                "attempt {attempt} gave {ms}ms"
            );
        }
    }

    #[test]
    fn mid_falls_back_to_whichever_side_exists() {
        let s = LiveBookStore::new();
        s.apply(
            0,
            WsEvent::Book {
                token_id: TOKEN.into(),
                market_id: "0x".into(),
                bids: vec![(dec!(0.4), dec!(5))],
                asks: vec![(dec!(0.6), dec!(5))],
            },
        );
        assert_eq!(s.get_fresh(TOKEN).unwrap().mid(), Some(dec!(0.5)));

        let s2 = LiveBookStore::new();
        s2.apply(
            0,
            WsEvent::Book {
                token_id: TOKEN.into(),
                market_id: "0x".into(),
                bids: vec![(dec!(0.4), dec!(5))],
                asks: vec![],
            },
        );
        assert_eq!(s2.get_fresh(TOKEN).unwrap().mid(), Some(dec!(0.4)));
    }
}
