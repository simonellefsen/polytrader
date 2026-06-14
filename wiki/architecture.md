# Architecture

System architecture of polytrader: the live components, the autonomous decision→execution loop, the
multi-signal fusion brain, the Hermes learning loop, and the fail-closed real-order gate. All diagrams
are Mermaid (render on GitHub). Paper-only by default; real-order dispatch is structurally impossible
in this build (see [Real-order gate](#real-order-fail-closed-gate)).

Cross-references: [index.md](index.md) · [schema.md](schema.md) ·
[strategies/multi-signal-fusion.md](strategies/multi-signal-fusion.md) ·
[concepts/hermes-self-improvement.md](concepts/hermes-self-improvement.md) ·
[decisions/real-order-approval-flow.md](decisions/real-order-approval-flow.md).

## 1. System / deployment topology

Three long-lived workloads in the k8s namespace `polytrader`, plus public read-only data sources.

```mermaid
flowchart TB
  subgraph ext["External (public, read-only)"]
    gamma["Gamma API\nmarkets + outcome prices"]
    clob["CLOB API\norderbooks (unauth)"]
    data["Data API\nproxy positions / PUSD value"]
    news["newsdata.io\n(12h-delayed, free)"]
    yahoo["Yahoo Finance\nadvisory context"]
    orouter["OpenRouter\nopenai/gpt-5.4-mini"]
  end

  subgraph k8s["k8s namespace: polytrader"]
    pt["polytrader (main)\naxum + Dioxus SSR\ningester · fusion · paper engine · web UI"]
    hermes["hermes\nreflection + weight tuning"]
    pg[("CloudNativePG\nPostgres (2 replicas)\nmarket_data · paper_trading · journal")]
  end

  user(["operator / browser"]) -->|"/board /trades /console"| pt
  gamma --> pt
  clob --> pt
  data --> pt
  news --> pt
  yahoo --> pt
  pt <--> pg
  hermes <--> pg
  hermes --> orouter
  pt -. "read-only balance" .-> data

  classDef ext fill:#1b2a3a,stroke:#2f5d8a,color:#cfe;
  class gamma,clob,data,news,yahoo,orouter ext;
```

- **polytrader** ingests public market data, runs the fusion + risk pipeline every 5 min, simulates
  fills, serves the dashboards, and reads (never writes) the real PUSD proxy balance.
- **hermes** runs an independent reflection loop: attributes realized P&L to signals, tunes fusion
  weights (clamped, gradual), and optionally synthesizes notes via OpenRouter.
- **postgres** is the single source of truth — three schemas, see [schema.md](schema.md).

## 2. Autonomous decision → execution loop (every 5 min)

```mermaid
flowchart LR
  A["ingest tick\nGamma + CLOB"] --> B["per market:\nbuild snapshot\n(mids, recent move, external)"]
  B --> C["FusionEngine.fuse()\nΣ score·conf·weight"]
  C --> D{"net edge\n≥ 4%?"}
  D -- no --> R1["journal decision_report\n(no trade)"]
  D -- yes --> E{"arb-only market\nor already held?"}
  E -- yes --> R1
  E -- no --> F["RiskManager\nKelly size + gates"]
  F -- reject --> R2["journal rejection\n(deduped 1/market/hr)"]
  F -- approve --> G["paper LIMIT order\nshares = usdc / price"]
  G --> H["PaperEngine fill\n(best-first orderbook)"]
  H --> I["write paper_position\n+ portfolio snapshot"]
  I --> J["shadow_real_order\n(fail-closed, see §5)"]
  J --> K["mark-to-market snapshot\n→ /trades P&L chart"]

  classDef gate fill:#3a2f1b,stroke:#8a6a2f,color:#fe7;
  class D,E,F gate;
```

Risk defaults (see `src/risk/mod.rs`): quarter-Kelly, `max_position_usdc 20`, `max_market_exposure
0.20`, `max_total_exposure 0.80`, `min_net_edge 0.04`, `pnl_floor -0.20`. Widening the market set only
widens the funnel into this loop — every candidate still clears the same per-trade gates.

## 3. Multi-signal fusion brain

```mermaid
flowchart TB
  subgraph signals["Processors → Signal{score, confidence, edge}"]
    s1["orderbook_momentum"]
    s2["spike_divergence"]
    s3["overreaction_fade\n(volatility-guarded)"]
    s4["yahoo_finance\n(advisory ≤0.30)"]
    s5["news_sentiment\n(advisory ≤0.30)"]
  end
  s1 & s2 & s3 & s4 & s5 --> F["fuse():\nΣ(score·conf·w) / Σ(conf·w)"]
  W["learned weights\n(strategy_weights journal)"] --> F
  F --> NE["fused score → net edge\nafter fees"]
  NE --> DR["decision_report\n(attribution per signal)"]

  classDef adv fill:#2a2333,stroke:#6a4f8a,color:#dcf;
  class s4,s5 adv;
```

Each processor emits a `Signal`; the engine fuses them with per-processor **learned weights** (clamped
to `[0.25, 2.0]`) loaded from the latest `strategy_weights` journal event. `overreaction_fade` only
fires on extreme prices (>0.72 / <0.28) **and** a recent move ≥ 0.07 — otherwise the extreme is likely
correct and it stands down. Details: [strategies/multi-signal-fusion.md](strategies/multi-signal-fusion.md).

## 4. Hermes self-improvement loop

```mermaid
sequenceDiagram
  participant H as hermes
  participant J as journal.events
  participant L as OpenRouter (optional)
  loop each reflection
    H->>J: read decision_reports, settlements
    H->>H: attribute realized P&L to signals\n(split by |score·effective_weight|)
    H->>H: compute weight adjustments\ntarget = clamp(1.0 + pnl/40), STEP 0.34
    H->>J: write strategy_weights (clamped, gradual)
    H->>L: synthesize reflection (if configured)
    L-->>H: text (or error)
    H->>J: write llm_health (ok | out_of_credits | auth_error | ...)
  end
  Note over H,J: polytrader loads strategy_weights\non the next fusion cycle (closed loop)
```

Profitable signals get up-weighted, losing ones down-weighted, gradually and within clamps. The
`llm_health` event surfaces on the /trades AI badge so a broken key or exhausted credits is visible at a
glance; Hermes falls back to local synthesis so reflections never stop. See
[concepts/hermes-self-improvement.md](concepts/hermes-self-improvement.md).

## 5. Real-order fail-closed gate

```mermaid
flowchart LR
  sig["approved paper trade"] --> shadow["shadow_real_order()"]
  shadow --> gate{"go-live gate"}
  gate --> p["proven:\nrealized P&L > 0\nover ≥10 settled"]
  gate --> f["funded:\nreal PUSD > 0"]
  gate --> a["approved:\noperator env token"]
  p & f & a --> ready{"all true?"}
  ready -- no --> rej["FailClosedLiveOrderSender\nrejects · nothing sent"]
  ready -- "yes (still fail-closed in this build)" --> rej
  rej --> jr["journal clob_shadow_order\n(would_send, gate, request_sent=false)"]

  classDef stop fill:#3a1b1b,stroke:#8a2f2f,color:#fbb;
  class rej stop;
```

Only the `FailClosedLiveOrderSender` is wired — even if proven + funded + approved were all true, no
order is dispatched. The gate exists to **measure** distance to live readiness, surfaced on the /trades
readiness panel. Full rationale: [decisions/real-order-approval-flow.md](decisions/real-order-approval-flow.md).

## 6. Data model (high level)

```mermaid
erDiagram
  markets ||--o{ paper_positions : "gamma_id"
  markets ||--o{ decision_reports : "market_id"
  paper_positions ||--o{ paper_fills : "market_id"
  portfolio_snapshots }o--|| paper_positions : "aggregates"
  events ||--o{ events : "journal (append-only)"

  markets {
    text gamma_id PK
    text slug
    decimal last_mid_yes
    decimal last_mid_no
    jsonb outcome_prices
    text resolved_outcome
  }
  paper_positions {
    text market_id
    text outcome
    decimal shares
    decimal avg_entry_price
    decimal collateral_locked
  }
  portfolio_snapshots {
    decimal virtual_usdc
    decimal total_locked
    decimal unrealized_pnl
    decimal realized_pnl
    text snapshot_reason
  }
```

Schemas: `market_data` (markets/orderbooks), `paper_trading` (positions/fills/snapshots), `journal`
(append-only events — decision_reports, settlements, strategy_weights, llm_health, clob_shadow_order,
real_account_balance). Money is always `rust_decimal::Decimal`, never floats. Full DDL + invariants:
[schema.md](schema.md).

## Surfaces (web UI)

- **`/board`** (default landing) — one card per market: probability bar, fused signal + fired chips,
  news polarity, resolution status, and **held positions** (sorted first, accent border, live
  unrealized P&L badge).
- **`/trades`** — portfolio cards, **live P&L chart** (green above water / red below), open positions
  with live marks, settlements, executions (filterable), AI health badge, real-trading readiness.
- **`/console`** — the original Dioxus SSR dashboard.

All three share the `Markets · Trades · Console` nav. Everything is read-only and paper-only.
