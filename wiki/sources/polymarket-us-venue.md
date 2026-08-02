# Polymarket US (`polymarket.us`) — a DIFFERENT venue from the one we trade

Created 2026-08-02, after an operator pointed at `docs.polymarket.us` asking what we were missing.
The short answer: those docs describe **a different exchange**, and nothing we have would port.
This page exists so that stays visible without anyone building against the wrong API.

## The distinction that matters

| | what we use | what `docs.polymarket.us` documents |
|---|---|---|
| Market data | `gamma-api.polymarket.com` | `gateway.polymarket.us` (public) |
| Trading | `clob.polymarket.com` | `api.polymarket.us` (authenticated) |
| Streaming | `wss://ws-subscriptions-clob.polymarket.com/ws/market` | `wss://api.polymarket.us/v1/ws/private` + `/v1/ws/markets`, **plus gRPC** |
| Also offered | — | FIX |
| Identifiers | Gamma numeric `id`, CLOB `token_id` | `symbol` / instrument refdata |
| Fee model | per-market `feeSchedule` (see below) | flat taker Θ = 0.06, **maker Θ = −0.0125** |

Market IDs, the data model and the auth scheme are all different. A migration is a rewrite of the
ingest and execution layers, not a base-URL change.

## Are we using gRPC? No — definitively

No `tonic`, `prost`, or `protobuf` anywhere in `Cargo.toml`, no `build.rs`, no codegen. A repo-wide
search returns exactly one hit and it is aspirational prose, not code: `docs/project-plan.md:93`
("perhaps gRPC or internal for Hermes"). Everything is HTTP/JSON via `reqwest` plus one WebSocket
via `tokio-tungstenite`. gRPC is a **`.us`-only** capability.

## Fees: the two venues agree on shape, differ on coefficients

Both use the same symmetric formula, which is why our `polymarket_fee` is structurally right:

```text
fee = Θ × contracts × p × (1 − p)
```

Maximal at p = 0.50, vanishing toward 0.01/0.99. `.us` documents banker's rounding.

**`.us`**: taker Θ = 0.06 flat; maker Θ = **−0.0125** (a rebate paid at the point of trade); taker
volume rebates tiered 10/25/50% above $250K/$1M/$10M monthly.

**`.com` (ours)**, sampled across 400 live top-volume markets on 2026-08-02:

- `exponent` is **always 1**, `takerOnly` **always true**
- `rate` ∈ {0.04, 0.05, 0.07}; `rebateRate` ∈ {0.15, 0.20, 0.25}, varying *independently* of `rate`
- **92 of 400 (23%) return `feesEnabled: false` with no schedule at all** — genuinely fee-free

`rebateRate` is the fraction of the taker fee paid to the **maker**, not a discount for the taker:
`0.06 × 0.208 = 0.0125` reproduces the `.us` maker theta exactly. Since we always cross the spread,
we pay full `rate`. See the 2026-08-02 roadmap entry for the full falsification.

## What `.us` would buy, if we ever wanted it

- A **documented, first-class maker rebate** rather than one inferred from a `rebateRate` field.
- **gRPC streaming** for BBO / L2 / market statistics, and OHLC + volume + open interest via REST
  (`/v1/orderbook/{symbol}/bbo`, `/v1/orderbook/{symbol}`, `/v1/refdata/{instruments,symbols,metadata}`).
- A **liquidity incentive program** ("rewards for placing resting orders, whether they fill or
  not"). Note: `.com` exposes the same idea *today* through Gamma's `clobRewards`, which is the
  cheaper path — see the roadmap open item.
- A changelog with an RSS feed (`https://docs.polymarket.us/changelog/rss.xml`). Its entries are
  `.us`-specific (market_sport_type, partial contracts, tick decimalization, rate-limit
  reductions), so it is **not** a useful monitor for our venue — a changelog widget was considered
  for the dashboard and deliberately declined for that reason.

## Standing decision (2026-08-02)

**Stay on `.com`; mine it harder.** The maker opportunity that motivated looking at `.us` is
already reachable through `clobRewards` on the venue we are on, with no integration cost. Revisit
only if a real-money decision requires the US-regulated entity — in which case this is a new
ingest + execution layer, budgeted as such.
