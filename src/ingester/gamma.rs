//! Gamma API client (markets, events, prices, resolutions).
//! Base URL: https://gamma-api.polymarket.com/
//!
//! Paper-only: only public read endpoints. Used for ingester to discover live markets.
//!
//! get_market is provided for future but unused in Phase 0 tick (list suffices).
#![allow(dead_code)]

use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Market {
    pub id: String,
    pub slug: String,
    pub question: String,
    pub outcomes: Vec<String>,
    pub clob_token_ids: Vec<String>,
    pub active: bool,
    pub closed: bool,
    /// Gamma `outcomePrices`, aligned with `outcomes`. After resolution one is "1" and the rest "0".
    pub outcome_prices: Vec<String>,
    /// Gamma `endDate` (ISO 8601, e.g. "2026-06-30T12:00:00Z"), if present. Resolution time — feeds the
    /// theta/convergence signal's days-to-resolution. Serialized into `raw_json` as `end_date`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    /// Polymarket taker fee RATE from Gamma's `feeSchedule.rate` (0 when `feesEnabled` is false, e.g.
    /// geopolitics). The effective fee is `shares × rate × price × (1 − price)`. None when the fields
    /// are absent — callers fall back to the category default. Stored per-market so historical P&L
    /// reflects the fee schedule in force.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taker_fee_rate: Option<Decimal>,
    /// Gamma `enableOrderBook` — whether the market has a live CLOB order book. Used to filter the
    /// volume-ranked arb-discovery universe down to markets that actually have books to arb (skipping
    /// order-book-less markets avoids a wasted CLOB fetch + error every tick). Defaults false.
    #[serde(default)]
    pub enable_order_book: bool,
    /// Gamma `volume24hr` (USD). Feeds the directional-rotation promotion filter (liquidity floor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_24hr: Option<Decimal>,
    /// Gamma `liquidityNum` (USD resting in the book). The `market_data.markets.liquidity` column
    /// has existed since the init migration and was **never written** — this fills it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidity: Option<Decimal>,
    /// Gamma `orderPriceMinTickSize` — the market's price increment (0.01 or 0.001 observed).
    ///
    /// We were fetching this from the CLOB `/tick-size` endpoint in a dry-run-only path and
    /// *discarding* the `tick_size` the WS feed puts on every book frame, while it was sitting in
    /// the Gamma payload we already parse. It bounds how finely a maker can undercut, so it is a
    /// prerequisite for quoting rather than a nicety.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tick_size: Option<Decimal>,
    /// Liquidity-rewards daily budget for this market, summed over `clobRewards[]` (USD/day).
    ///
    /// **This is money paid for resting orders whether or not they fill** — the concrete, per-market
    /// form of the P5 maker thesis, and the single most valuable field we were throwing away (one
    /// market observed at $1,000/day). Zero/absent means the market pays no rewards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewards_daily_rate: Option<Decimal>,
    /// Gamma `rewardsMinSize` — minimum resting order size (shares) to qualify for rewards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewards_min_size: Option<Decimal>,
    /// Gamma `rewardsMaxSpread` — max distance from the midpoint (in cents) a resting order may sit
    /// and still qualify. With `rewards_min_size` this is the full qualification rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewards_max_spread: Option<Decimal>,
    /// Gamma parent event id (`events[0].id`). Groups the mutually-exclusive member markets of a
    /// negRisk event for the event-level arbitrage scanner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Gamma `negRisk` (market-level, falling back to the parent event's flag): at most ONE member
    /// market of this event resolves Yes. The invariant the buy-all-No event arb rests on.
    #[serde(default)]
    pub neg_risk: bool,
}

#[derive(Clone)]
pub struct GammaClient {
    http: Client,
    base: String,
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .user_agent("polytrader/0.1 (paper-only)")
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
            base: "https://gamma-api.polymarket.com".to_string(),
        }
    }

    /// Parse a single Gamma market JSON object into our Market struct.
    /// Gamma returns `outcomes` and `clobTokenIds` as JSON-encoded *strings*, hence the inner parse.
    /// Gamma is inconsistent about number vs string for the same field across endpoints (and even
    /// between `liquidity` and `liquidityNum` on one market), so every numeric read goes through
    /// this rather than committing to one JSON type per field.
    /// Note it goes via `Number::to_string()`, NOT `as_f64()`. `as_f64` + `from_f64_retain` drags
    /// the value through a binary float and back: Gamma's `1018110.2348` comes out as
    /// `1018110.2347999999765306711195`. `to_string()` hands back the original JSON literal, which
    /// parses to the exact decimal — the same "never route money through f64" rule the rest of the
    /// crate follows.
    fn num(v: Option<&Value>) -> Option<Decimal> {
        match v? {
            Value::Number(x) => x.to_string().parse::<Decimal>().ok(),
            Value::String(s) => s.trim().parse::<Decimal>().ok(),
            _ => None,
        }
    }

    fn parse_market(v: &Value) -> Market {
        let outcomes_str = v.get("outcomes").and_then(|o| o.as_str()).unwrap_or("[]");
        let outcomes: Vec<String> = serde_json::from_str(outcomes_str).unwrap_or_default();

        let clob_str = v
            .get("clobTokenIds")
            .and_then(|c| c.as_str())
            .unwrap_or("[]");
        let clob_token_ids: Vec<String> = serde_json::from_str(clob_str).unwrap_or_default();

        let prices_str = v
            .get("outcomePrices")
            .and_then(|c| c.as_str())
            .unwrap_or("[]");
        let outcome_prices: Vec<String> = serde_json::from_str(prices_str).unwrap_or_default();

        Market {
            id: v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string(),
            slug: v
                .get("slug")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            question: v
                .get("question")
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .to_string(),
            outcomes,
            clob_token_ids,
            active: v.get("active").and_then(|a| a.as_bool()).unwrap_or(false),
            closed: v.get("closed").and_then(|c| c.as_bool()).unwrap_or(true),
            outcome_prices,
            end_date: v
                .get("endDate")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            taker_fee_rate: {
                // feesEnabled=false ⇒ fee-free (0). Otherwise take feeSchedule.rate. Absent ⇒ None.
                let enabled = v.get("feesEnabled").and_then(|b| b.as_bool());
                // Via `num` (literal → Decimal), not as_f64: this rate multiplies into the negRisk
                // arb line, so float noise here is noise in the tradeable/not-tradeable decision.
                let rate = Self::num(v.get("feeSchedule").and_then(|f| f.get("rate")));
                match (enabled, rate) {
                    (Some(false), _) => Some(Decimal::ZERO),
                    (_, Some(r)) => Some(r),
                    _ => None,
                }
            },
            enable_order_book: v
                .get("enableOrderBook")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
            volume_24hr: Self::num(v.get("volume24hr")),
            // Gamma sends `liquidity` as a string and `liquidityNum` as a number for the same
            // value; take either.
            liquidity: Self::num(v.get("liquidityNum")).or_else(|| Self::num(v.get("liquidity"))),
            tick_size: Self::num(v.get("orderPriceMinTickSize")),
            // clobRewards is an ARRAY: a market can carry several concurrent reward programs, and
            // what a maker earns from resting there is their sum, not the first one. `null` (no
            // programme) and `[]` both mean zero — mapped to None so "not rewarded" and "we failed
            // to parse" stay distinguishable in the column.
            rewards_daily_rate: v
                .get("clobRewards")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| Self::num(r.get("rewardsDailyRate")))
                        .sum::<Decimal>()
                })
                .filter(|total| !total.is_zero()),
            rewards_min_size: Self::num(v.get("rewardsMinSize")),
            rewards_max_spread: Self::num(v.get("rewardsMaxSpread")),
            event_id: v
                .get("events")
                .and_then(|e| e.as_array())
                .and_then(|a| a.first())
                .and_then(|e| e.get("id"))
                .and_then(|i| i.as_str())
                .map(|s| s.to_string()),
            neg_risk: v
                .get("negRisk")
                .and_then(|b| b.as_bool())
                .or_else(|| {
                    v.get("events")
                        .and_then(|e| e.as_array())
                        .and_then(|a| a.first())
                        .and_then(|e| e.get("negRisk"))
                        .and_then(|b| b.as_bool())
                })
                .unwrap_or(false),
        }
    }

    /// Discover the top active binary markets by 24h volume that have a live order book — the
    /// volume-ranked arb-scan universe. Filtered client-side to 2-outcome (Yes/No) order-book
    /// markets since the YES+NO<$1 arb needs exactly two complementary books. Breadth is the point:
    /// real arbs are rare per-market but scale with the number of books watched. Failure is
    /// caller-non-fatal.
    ///
    /// PAGINATED (2026-08-01). This was a single call passing `limit` straight through, but Gamma
    /// caps a /markets page at 100 **regardless of the limit parameter** — the same cap already
    /// documented on `discover_directional_markets`. So `POLYTRADER_ARB_DISCOVERY_LIMIT=150` had
    /// been silently delivering 100 markets (confirmed against the live API and against the
    /// `universe=158` ingest log against a 150 limit + 23 bootstrap + rotation/held slugs). The knob
    /// now means what it says.
    pub async fn discover_arb_markets(&self, limit: usize) -> Result<Vec<Market>> {
        const PAGE: usize = 100; // Gamma caps a /markets page at 100 regardless of limit
        let mut out: Vec<Market> = Vec::new();
        let mut returned_total = 0usize;
        let mut offset = 0usize;
        while offset < limit {
            let want = PAGE.min(limit - offset);
            let url = format!(
                "{}/markets?active=true&closed=false&order=volume24hr&ascending=false&limit={}&offset={}",
                self.base, want, offset
            );
            let resp: Vec<Value> = self.http.get(&url).send().await?.json().await?;
            let n = resp.len();
            returned_total += n;
            out.extend(resp.iter().map(Self::parse_market).filter(|m| {
                !m.closed
                    && m.enable_order_book
                    && m.outcomes.len() == 2
                    && m.clob_token_ids.len() == 2
            }));
            if n < want {
                break; // ranking exhausted
            }
            offset += n;
        }
        tracing::info!(
            returned = returned_total,
            usable = out.len(),
            limit,
            "gamma arb-discovery markets"
        );
        Ok(out)
    }

    /// Fetch every member market of the given negRisk events (`/events?id=…`, whose `markets[]`
    /// carries the full member list). Returns live, binary, order-book members only.
    ///
    /// This is what makes event-level arb reachable. The volume-ranked discovery pool ranks
    /// INDIVIDUAL markets, so a big ladder enters it as a handful of its most-traded legs and the
    /// rest are invisible. Measured 2026-08-01: across the 31 negRisk events we already touched,
    /// Gamma exposed **530** live order-book members while we held fresh books for **76** — the
    /// scanner was reading 14% of the very events it was scanning. Since the arb line is
    /// `Σ(1 − ask_no) > 1`, a fragment of a 50-leg ladder sums nowhere near 1 no matter how
    /// dislocated the event is; coverage has to be near-complete before the event is even
    /// judgeable.
    ///
    /// Event payloads are large (full description text per member), so ids are requested in small
    /// chunks and a failed chunk is skipped rather than failing the pass.
    pub async fn fetch_event_members(&self, event_ids: &[String]) -> Result<Vec<Market>> {
        /// Event JSON carries every member's full description; keep requests small.
        const CHUNK: usize = 5;
        let mut out: Vec<Market> = Vec::new();
        for chunk in event_ids.chunks(CHUNK) {
            let q = chunk
                .iter()
                .map(|id| format!("id={id}"))
                .collect::<Vec<_>>()
                .join("&");
            let url = format!("{}/events?limit=100&{}", self.base, q);
            let resp: Vec<Value> = match self.http.get(&url).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "gamma event-members parse failed; skipping chunk");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "gamma event-members request failed; skipping chunk");
                    continue;
                }
            };
            for ev in resp {
                let Some(event_id) = ev.get("id").and_then(|i| i.as_str()) else {
                    continue;
                };
                // The parent's negRisk is authoritative: member objects nested under an event carry
                // no `events` array, so `parse_market`'s usual fallback path can't see it.
                let parent_neg_risk = ev.get("negRisk").and_then(|b| b.as_bool()).unwrap_or(false);
                let Some(members) = ev.get("markets").and_then(|m| m.as_array()) else {
                    continue;
                };
                for mv in members {
                    let mut m = Self::parse_market(mv);
                    if m.closed
                        || !m.active
                        || !m.enable_order_book
                        || m.outcomes.len() != 2
                        || m.clob_token_ids.len() != 2
                        || m.id.is_empty()
                    {
                        continue;
                    }
                    m.event_id = Some(event_id.to_string());
                    m.neg_risk = m.neg_risk || parent_neg_risk;
                    out.push(m);
                }
            }
        }
        tracing::info!(
            events = event_ids.len(),
            members = out.len(),
            "gamma negRisk event members"
        );
        Ok(out)
    }

    /// Discover directional-rotation candidates: top active binary order-book markets by 24h volume
    /// whose end date falls within the next `max_days`. Short-dated is the point — the profile that
    /// made the June 2026 curated set tradeable (uncertain, moving, resolves in days-weeks) versus
    /// the multi-year horizons that never clear the edge gate. The caller applies the non-sports
    /// classifier and the volume floor; this returns the raw usable pool. Failure is caller-non-fatal.
    ///
    /// PAGINATED past Gamma's 100-per-page cap (measured 2026-07-05): page 0 of the short-dated
    /// volume ranking is a wall of sports match markets with only ~5 non-sports candidates, while
    /// pages 1–4 held ~150 (Fed brackets, BTC weeklies, Iran/Hormuz deadlines, box office …).
    /// `pages` × 100 candidates, stopping early on a short page. Runs on the 6h rotation cadence,
    /// so a handful of extra requests is negligible.
    pub async fn discover_directional_markets(
        &self,
        pages: usize,
        max_days: i64,
    ) -> Result<Vec<Market>> {
        const PAGE: usize = 100; // Gamma caps a /markets page at 100 regardless of limit
        let now = chrono::Utc::now();
        let emin = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let emax = (now + chrono::Duration::days(max_days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let mut out: Vec<Market> = Vec::new();
        let mut returned_total = 0usize;
        for page in 0..pages.max(1) {
            let url = format!(
                "{}/markets?active=true&closed=false&order=volume24hr&ascending=false&limit={}&offset={}&end_date_min={}&end_date_max={}",
                self.base,
                PAGE,
                page * PAGE,
                emin,
                emax,
            );
            let resp: Vec<Value> = self.http.get(&url).send().await?.json().await?;
            let n = resp.len();
            returned_total += n;
            out.extend(resp.iter().map(Self::parse_market).filter(|m| {
                !m.closed
                    && m.enable_order_book
                    && m.outcomes.len() == 2
                    && m.clob_token_ids.len() == 2
            }));
            if n < PAGE {
                break; // ranking exhausted
            }
        }
        tracing::info!(
            returned = returned_total,
            usable = out.len(),
            pages,
            max_days,
            "gamma directional-rotation candidates"
        );
        Ok(out)
    }

    /// Tag labels of a Gamma EVENT (`/events/{id}` → `tags[].label`). The market/slug endpoints
    /// return no tags, but the event endpoint carries Polymarket's own taxonomy (e.g.
    /// ["Sports", "Esports", "league of legends", "lol"]) — DATA-driven category truth, unlike the
    /// slug-keyword classifier that new slug formats keep dodging (wta-/cs2- 2026-07-04, then
    /// will-t1-win-msi/cricmlc- 2026-07-05). Used by the rotation job to hard-reject sports/esports
    /// from directional eligibility. Errors propagate — the caller fails CLOSED (no promotion).
    pub async fn event_tags(&self, event_id: &str) -> Result<Vec<String>> {
        let url = format!("{}/events/{}", self.base, event_id);
        let ev: Value = self.http.get(&url).send().await?.json().await?;
        Ok(ev
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.get("label").and_then(|l| l.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Fetch specific markets by slug (public Gamma /markets?slug=...).
    ///
    /// This is the bootstrap path: querying each configured slug directly *guarantees* the market
    /// is fetched, instead of hoping it appears in a generic top-N list (which previously caused
    /// `processed: 0` — the bootstrap slug was never in the first 20 active markets returned).
    /// Robust: a failed/empty slug query is logged and skipped, never aborts the whole tick.
    pub async fn fetch_markets_by_slugs(&self, slugs: &[String]) -> Result<Vec<Market>> {
        let mut out = Vec::new();
        for slug in slugs {
            // Default Gamma /markets?slug=X returns the market ONLY while it is open — a RESOLVED
            // market is dropped from this query. If we stop here, the moment a held market resolves
            // we go blind to it: `closed`/resolved prices are never ingested, `resolved_outcome`
            // stays NULL, and settle_resolved_positions never fires (collateral locked forever).
            let url = format!("{}/markets?slug={}", self.base, slug);
            let mut items = self.get_markets_json(&url, slug).await;

            // Fallback: a slug missing from the open query is almost always resolved/closed. Re-query
            // including closed markets so we capture the resolution (closed=true + final outcomePrices),
            // which is exactly what the settlement path needs. This is the fix for "0 settlements
            // despite markets resolving".
            if items.is_empty() {
                let closed_url = format!("{}/markets?slug={}&closed=true", self.base, slug);
                items = self.get_markets_json(&closed_url, slug).await;
                if !items.is_empty() {
                    tracing::info!(slug = %slug, "gamma slug resolved/closed; ingested via closed=true fallback (will settle)");
                }
            }

            if items.is_empty() {
                tracing::warn!(slug = %slug, "gamma returned no market for bootstrap slug even with closed=true (renamed/delisted?)");
            }
            out.extend(items.iter().map(Self::parse_market));
        }
        tracing::debug!(count = out.len(), "gamma fetched markets by slug");
        Ok(out)
    }

    /// GET a Gamma /markets URL and parse the JSON array, logging+swallowing errors (returns empty).
    /// Keeps `fetch_markets_by_slugs` robust: one bad slug/response never aborts the tick.
    async fn get_markets_json(&self, url: &str, slug: &str) -> Vec<Value> {
        match self.http.get(url).send().await {
            Ok(resp) => match resp.json::<Vec<Value>>().await {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!(slug = %slug, error = %e, "gamma slug response parse failed; skipping");
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(slug = %slug, error = %e, "gamma slug request failed; skipping");
                Vec::new()
            }
        }
    }

    /// List active markets (public Gamma /markets, up to limit).
    /// Used when no bootstrap allowlist is configured (discovery mode).
    pub async fn list_active_markets(&self) -> Result<Vec<Market>> {
        let url = format!("{}/markets?limit=20&active=true", self.base);
        let resp: Vec<Value> = self.http.get(&url).send().await?.json().await?;
        let out: Vec<Market> = resp.iter().map(Self::parse_market).collect();
        tracing::debug!(count = out.len(), "gamma listed active markets");
        Ok(out)
    }

    pub async fn get_market(&self, market_id: &str) -> Result<Option<Market>> {
        // For bootstrap, reuse list and find; or call /markets/{id} if exists.
        let markets = self.list_active_markets().await?;
        Ok(markets.into_iter().find(|m| m.id == market_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// A real Gamma `/markets` row, trimmed to the fields under test. Captured live 2026-08-02 from
    /// `will-the-us-invade-iran-before-2027` — the market that made the rewards data worth parsing
    /// at all ($1,000/day paid to resting orders, and we were discarding it).
    const REAL_ROW: &str = r#"{
        "id": "512345", "slug": "will-the-us-invade-iran-before-2027",
        "question": "Will the US invade Iran before 2027?",
        "outcomes": "[\"Yes\",\"No\"]", "clobTokenIds": "[\"111\",\"222\"]",
        "outcomePrices": "[\"0.18\",\"0.82\"]",
        "active": true, "closed": false, "enableOrderBook": true,
        "feesEnabled": false, "feeSchedule": null,
        "liquidity": "1018110.2348", "liquidityNum": 1018110.2348,
        "volume24hr": 2126126.372011996,
        "orderPriceMinTickSize": 0.01,
        "rewardsMinSize": 200, "rewardsMaxSpread": 3.5,
        "clobRewards": [{"id": "288102", "rewardsAmount": 0, "rewardsDailyRate": 1000}]
    }"#;

    #[test]
    fn a_real_gamma_row_yields_the_rewards_and_tick_fields_we_used_to_discard() {
        let m = GammaClient::parse_market(&serde_json::from_str(REAL_ROW).unwrap());
        assert_eq!(m.rewards_daily_rate, Some(dec!(1000)));
        assert_eq!(m.rewards_min_size, Some(dec!(200)));
        assert_eq!(m.rewards_max_spread, Some(dec!(3.5)));
        assert_eq!(m.tick_size, Some(dec!(0.01)));
        assert_eq!(m.liquidity, Some(dec!(1018110.2348)));
        // feesEnabled:false really does mean fee-free — 23% of the top-volume universe is.
        assert_eq!(m.taker_fee_rate, Some(Decimal::ZERO));
    }

    #[test]
    fn rewards_sum_across_concurrent_programmes() {
        // clobRewards is an ARRAY. A market can run several programmes at once, and what a maker
        // earns for resting there is their sum — taking the first would under-report the budget.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"clobRewards":[{"rewardsDailyRate":600},{"rewardsDailyRate":150}]}"#,
        )
        .unwrap();
        assert_eq!(
            GammaClient::parse_market(&v).rewards_daily_rate,
            Some(dec!(750))
        );
    }

    #[test]
    fn no_reward_programme_is_none_not_zero() {
        // "pays nothing" and "we failed to parse" must stay distinguishable in the column, so an
        // absent or empty programme list yields None rather than 0.
        for raw in [r#"{}"#, r#"{"clobRewards":null}"#, r#"{"clobRewards":[]}"#] {
            let m = GammaClient::parse_market(&serde_json::from_str(raw).unwrap());
            assert_eq!(m.rewards_daily_rate, None, "raw was {raw}");
        }
    }

    #[test]
    fn numeric_fields_parse_from_either_json_type() {
        // Gamma is inconsistent: `liquidity` is a string and `liquidityNum` a number for the SAME
        // value on the same market, and other endpoints flip which form a field takes.
        assert_eq!(
            GammaClient::num(Some(&serde_json::json!("1018110.2348"))),
            Some(dec!(1018110.2348))
        );
        assert_eq!(
            GammaClient::num(Some(&serde_json::json!(1018110.2348))),
            Some(dec!(1018110.2348))
        );
        assert_eq!(GammaClient::num(Some(&serde_json::json!(null))), None);
        assert_eq!(GammaClient::num(None), None);
    }

    #[test]
    fn json_numbers_do_not_go_through_a_binary_float() {
        // Caught by the test above failing on the real payload. `as_f64` + `from_f64_retain` turns
        // Gamma's 1018110.2348 into 1018110.2347999999765306711195; going via the JSON literal
        // keeps it exact. The rate below matters most — it multiplies into the negRisk arb line, so
        // noise here is noise in a tradeable/not-tradeable decision.
        let exact = GammaClient::num(Some(&serde_json::json!(1018110.2348))).unwrap();
        assert_eq!(exact.to_string(), "1018110.2348");

        let m = GammaClient::parse_market(
            &serde_json::from_str(r#"{"feesEnabled":true,"feeSchedule":{"rate":0.07}}"#).unwrap(),
        );
        assert_eq!(m.taker_fee_rate.unwrap().to_string(), "0.07");
    }
}
