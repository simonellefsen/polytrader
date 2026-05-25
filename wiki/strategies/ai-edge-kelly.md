# AI Edge Detection + Kelly / Position Sizing Strategies

**Last updated**: 2026-05-25 (wiki-first per approved plan + AGENTS.md)  
**Credits**: Inspired by **Poly-Trader/** (polymarket_ai_search.py, polymarket_ai_simple.py, polymarket_combined.py, polymarket_openai.py, polymarket_profits.py, place_polymarket_bet.py + fetch_*.py, app.py, check_*.py, bet_history.csv analysis, generate_html.py). Strong AI-assisted market search, edge identification (LLM/web emulator patterns), profit tracking, and programmatic betting flows. Also informed by AI/LLM scorer patterns in openclaw (src/models/llmScorer.ts) and agents/ (prompts.py + application/executor.py).

**Overview**
Poly-Trader shows practical LLM + data fusion for "what markets have edge right now" (AI search over Polymarket + simple heuristics + combined), plus P&L/profit analysis and bet placement scaffolding. Kelly-like sizing (implicit in profits/risk management) is key for Phase 3.4 (risk/position). Transfer the edge-detection loop + attribution to Hermes closed-loop (3.3).

**Key Transferable Elements**
- AI search / simple / combined scanners (polymarket_ai_*.py, polymarket_search.py) using LLM for narrative + data signals.
- Real-time market fetch + orderbook (fetch_order_book.py, fetch_real_markets.py).
- Profits/P&L tracking + history (polymarket_profits.py, bet_history.csv).
- Programmatic + web-emulator flows for robustness.
- Balance/approval checks (check_usdc.py etc) — gate patterns for later real mode.

**Mapping to polytrader**
- Hermes (src/bin/hermes.rs) already does reflection + optional LLM synthesis (reqwest to /chat). Extend for edge proposal generation.
- Journal + paper portfolio for profits attribution.
- UI (src/ui/) can surface AI-suggested edges (post-Phase 2 SSR).

**Rust Port Notes**
- LLM client already present (hermes + env LLM_API_*); wrap for edge scoring with Decimal outputs.
- "Kelly" port: fractional Kelly using Decimal (edge, win prob from fusion, bankroll from virtual_portfolio). Formula: f = (bp - q) / b  (edge-adjusted); conservative fractions in paper.
- Heavy risk comments: "Risk: LLM edge can hallucinate or overfit narratives; always require fusion signal confirmation + paper forward-test before any sizing. Strict max-fraction + daily loss limits non-negotiable (AGENTS safety rules). Never use full Kelly in early phases."
- Store proposed edges + sizing rationale in journal for Hermes review.

**Paper-Only**
- All AI edges feed paper orders only. Virtual USDC bankroll from paper_trading.virtual_portfolio.
- Profits analysis via existing P&L in hermes reflections.

**Phase 3.3 / 3.4 Ties**
- Closed-loop: Hermes attributes P&L to specific AI edges + fusion signals.
- Dashboard: "AI Edge Scanner" panel + Kelly sizing simulator.
- Complements multi-signal-fusion (AI as one high-level processor or meta-signal).

See integrations/ for data, decisions/ for adoption, hermes-self-improvement.md for learning loop.

*Wiki-first. Explicit credits. Smallest doc enabling 3.4 position/risk code.*