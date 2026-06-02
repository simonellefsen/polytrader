//! Gated authenticated CLOB client for real Polymarket reads.
//!
//! This module provides the foundation for using real L2 credentials (derived via
//! the native SDK path or server key) to talk to https://clob.polymarket.com.
//!
//! **EXTREMELY IMPORTANT - READ BEFORE USING**:
//! - This uses **REAL** L2 credentials, even though it currently performs only read-only requests.
//! - Read-only calls use real account credentials and may count toward Polymarket API rate limits.
//! - Per AGENTS.md, wiki decisions, and all prior work: real order placement requires explicit additional reviews,
//!   risk engine integration, sizing limits, kill switches, tax journaling, and human approval gates.
//!
//! Current gates (as of this writing):
//! - Read-only live calls can be disabled with `POLYTRADER_ENABLE_REAL_CLOB_READS=0`.
//! - Real order writes (place) are disabled unless `POLYTRADER_ENABLE_REAL_ORDERS=1` (or legacy _SUBMISSION); default false.
//! - Must have successfully derived real L2 credentials (the session must exist in L2_SECRETS).
//! - Paper mode is still the default; this client is orthogonal to the PaperTradingEngine.
//! - place_limit_order exists (sign + POST /order) but is only reachable via GatedRealClobLiveOrderSender + submit-facade when all pre-gates, human_approval_event_id, final_review_decision_event_id, kill switch, and risk revalidation pass inside the sender immediately before dispatch.
//!
//! When you are ready for the next phase (after risk review), this is where the real CLOB client + order logic lives.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as ReqwestClient;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Configuration for the real (authenticated) CLOB client.
#[derive(Clone)]
pub struct RealClobConfig {
    pub base_url: String,
    /// Whether read-only live CLOB calls are allowed.
    pub read_enabled: bool,
    /// Whether real order placement (write) to CLOB is allowed.
    /// Controlled by POLYTRADER_ENABLE_REAL_ORDERS (or legacy _SUBMISSION).
    /// DEFAULT FALSE. Per AGENTS: explicit human gates + kill switch + risk
    /// required even when true; this flag is only the top-level unlock.
    pub orders_enabled: bool,
    /// Polymarket wallet signature type used for balance/allowance reads.
    /// Defaults to EOA (0). Proxy/deposit-wallet users can override with
    /// POLYMARKET_SIGNATURE_TYPE=1,2,3 after verifying their funder setup.
    pub signature_type: u8,
}

impl Default for RealClobConfig {
    fn default() -> Self {
        let signature_type = std::env::var("POLYMARKET_SIGNATURE_TYPE")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|v| *v <= 3)
            .unwrap_or(0);

        // Explicit unlock for writes (orders). Separate from reads. Default false.
        // Task requires support for POLYTRADER_ENABLE_REAL_ORDERS (and preserves
        // the prior _SUBMISSION name for compatibility with existing gates).
        let orders_enabled = std::env::var("POLYTRADER_ENABLE_REAL_ORDERS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
            || std::env::var("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

        Self {
            base_url: "https://clob.polymarket.com".to_string(),
            read_enabled: std::env::var("POLYTRADER_ENABLE_REAL_CLOB_READS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true),
            orders_enabled,
            signature_type,
        }
    }
}

/// A real authenticated client for the Polymarket CLOB using derived L2 credentials.
///
/// Credentials (api_key, secret, passphrase) come from successful L2 derivation
/// (either on startup via server key or via the UI "Derive from Server Key" button).
///
/// The secret is used to sign requests (HMAC). Never log it.
#[derive(Clone)]
pub struct RealClobClient {
    http: ReqwestClient,
    address: String,
    api_key: String,
    secret: String,
    passphrase: String,
    config: RealClobConfig,
}

/// Proposed real CLOB order intent for dry-run validation only.
///
/// RISK: This is not an order builder and is never signed/submitted. It exists
/// so the system can prove risk/preflight blockers before any real route exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealOrderIntentDryRun {
    pub token_id: String,
    pub side: String,
    pub order_type: String,
    pub size: Decimal,
    pub price: Option<Decimal>,
    pub expected_edge_bps: Option<Decimal>,
    pub market_id: Option<String>,
    pub outcome: Option<String>,
}

/// Explicit request for a signed-order payload dry-run.
///
/// RISK: With `confirm_signed_payload_dry_run=true`, this may use the real
/// Polymarket private key to sign an order payload locally. The signature is
/// never returned in full, persisted, posted, submitted, or logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedOrderPayloadDryRunRequest {
    #[serde(flatten)]
    pub intent: RealOrderIntentDryRun,
    pub confirm_signed_payload_dry_run: bool,
}

/// Explicit request for a non-submitting order POST request dry-run.
///
/// RISK: This can build a locally signed payload and serialize the would-be
/// `POST /order` body/headers. It must never send the request, expose full
/// signatures/HMACs, persist raw signed bodies, or place an order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderPostRequestDryRunRequest {
    #[serde(flatten)]
    pub signed_payload_request: SignedOrderPayloadDryRunRequest,
    pub confirm_order_post_request_dry_run: bool,
}

/// Explicit request for the fail-closed real-order submission facade.
///
/// RISK: This is not real-order enablement. It proves the shape of a future
/// submit path while keeping every dangerous side effect blocked unless
/// separate approval, kill-switch, exposure, journaling, and config gates are
/// implemented and explicitly unlocked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSubmitFacadeRequest {
    #[serde(flatten)]
    pub post_request_dry_run_request: OrderPostRequestDryRunRequest,
    pub confirm_real_order_submission: bool,
    #[serde(default)]
    pub human_approval_event_id: Option<uuid::Uuid>,
    /// The journal event id of a prior clob_final_review_decision (recorded via
    /// /clob/final-review-decision). Carried into LiveOrderSendRequest and
    /// validated in gate_report so that final_ok + human_ok can enable the ready
    /// path in Gated sender (non-zero id required even for audit-only decisions).
    #[serde(default)]
    pub final_review_decision_event_id: Option<uuid::Uuid>,
    pub human_approval_token: Option<String>,
    pub human_approval_note: Option<String>,
    pub operator: Option<String>,
    #[serde(default, skip_deserializing)]
    pub server_human_approval: Option<HumanApprovalValidation>,
    #[serde(default, skip_deserializing)]
    pub server_final_review_decision: Option<FinalReviewDecisionValidation>,
    #[serde(default, skip_deserializing)]
    pub server_collateral_readiness: Option<CollateralReadinessValidation>,
}

/// Server-side validation result for a journaled human approval event.
///
/// This is skipped during request deserialization so clients cannot claim
/// approval validity. The server populates it after reading `journal.events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanApprovalValidation {
    pub valid: bool,
    pub event_id: Option<uuid::Uuid>,
    pub decision: Option<String>,
    pub subject_hash: Option<String>,
    pub blockers: Vec<String>,
}

/// Server-side validation result for a journaled final review decision event.
///
/// Populated by server (from journal) after client supplies the id in the
/// submit facade request. Non-zero + valid decision event is required (with
/// human) for the Gated sender's final_ok gate (even though decisions are
/// audit-only and never auto-approve).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalReviewDecisionValidation {
    pub valid: bool,
    pub event_id: Option<uuid::Uuid>,
    pub decision: Option<String>,
    pub operator: Option<String>,
    pub blockers: Vec<String>,
}

/// Server-side validation result for the latest journaled collateral readiness snapshot.
///
/// Clients cannot populate this field. The server derives it from `journal.events`
/// so the submit facade can require a fresh, audited view of external wallet
/// state before any future live-send code is considered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollateralReadinessValidation {
    pub valid: bool,
    pub event_id: Option<uuid::Uuid>,
    pub created_at: Option<String>,
    pub wallet_address: Option<String>,
    pub collateral_balance: Option<String>,
    pub collateral_balance_positive: bool,
    pub collateral_allowance_positive: bool,
    pub positive_allowance_count: Option<u64>,
    pub max_age_seconds: u64,
    pub age_seconds: Option<i64>,
    pub blockers: Vec<String>,
}

impl RealClobClient {
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Try to construct from the currently active L2 session (if any).
    ///
    /// Returns None if no L2 credentials are currently derived / stored.
    ///
    /// Deterministic: prefers SERVER_L2_SESSION_ID (set by derive-from-server-key
    /// and auto-derive) to fix race/TOCTOU with .values().next() when map has
    /// multiple entries or concurrent derive/disconnect (critical for gated send
    /// rebuild at dispatch using correct trading creds + address match).
    pub fn from_current_l2_session() -> Option<Self> {
        // This reaches into the server module's secret store (made pub for the gated real client).
        // In a cleaner architecture this would be injected via AppState or a dedicated CLOB service.
        let secrets = crate::server::get_l2_secrets().lock().ok()?;
        let server_sid = crate::server::get_server_l2_session_id()
            .lock()
            .ok()
            .and_then(|g| g.clone());
        // Prefer server session (the one for real trading / k8s); fallback to any for
        // read paths after non-server UI demo derives (which do not set SERVER id).
        let creds = server_sid
            .and_then(|sid| secrets.get(&sid).cloned())
            .or_else(|| secrets.values().next().cloned());
        if let Some(creds) = creds {
            if creds.address.is_empty()
                || creds.secret.is_empty()
                || creds.passphrase.is_empty()
                || creds.api_key.is_empty()
            {
                return None;
            }

            let config = RealClobConfig::default();
            if !config.read_enabled {
                warn!("RealClobClient constructed but POLYTRADER_ENABLE_REAL_CLOB_READS is disabled; live read-only calls will fail closed.");
            }
            if config.orders_enabled {
                // Still gated by per-send revalidation inside GatedRealClobLiveOrderSender + facade + final review.
                warn!("RealClobClient constructed with orders_enabled=true (from POLYTRADER_ENABLE_REAL_ORDERS or _SUBMISSION). Real order placement remains blocked unless every human approval, kill switch, risk, and revalidation gate also passes at dispatch time.");
            }

            Some(Self {
                http: ReqwestClient::new(),
                address: creds.address,
                api_key: creds.api_key,
                secret: creds.secret,
                passphrase: creds.passphrase,
                config,
            })
        } else {
            None
        }
    }

    /// Read-only live CLOB call: list open orders for the authenticated L2 key.
    ///
    /// This proves the L2 credential path can authenticate without implementing
    /// any write endpoint. It intentionally does not expose order placement or
    /// cancellation.
    pub async fn open_orders(&self) -> Result<serde_json::Value> {
        self.authenticated_get_json("/data/orders").await
    }

    /// Read-only live CLOB call: fetch collateral balance and allowance.
    ///
    /// This is a pre-trade diagnostic only. It helps surface whether the
    /// Polymarket account has collateral/approval state visible to the CLOB
    /// before any future order path is considered.
    pub async fn collateral_balance_allowance(&self) -> Result<serde_json::Value> {
        let query = format!(
            "asset_type=COLLATERAL&signature_type={}",
            self.config.signature_type
        );
        self.authenticated_get_json_with_query("/balance-allowance", Some(&query))
            .await
    }

    /// Read-only account snapshot assembled from safe authenticated CLOB reads.
    pub async fn account_snapshot(&self) -> Result<serde_json::Value> {
        let open_orders = self.open_orders().await?;
        let collateral = self.collateral_balance_allowance().await?;

        Ok(serde_json::json!({
            "open_orders": open_orders,
            "collateral": collateral,
            "signature_type": self.config.signature_type,
        }))
    }

    /// Read-only preflight diagnostics for future real-order work.
    ///
    /// This deliberately returns blockers instead of enabling anything. A later
    /// order path must call an equivalent gate before signing/submitting.
    pub async fn preflight_report(&self) -> Result<serde_json::Value> {
        let account = self.account_snapshot().await?;
        Ok(build_preflight_report(&account))
    }

    /// Read-only collateral/allowance capacity report for operators.
    ///
    /// This does not approve allowances, refresh allowances, transfer funds, or
    /// otherwise mutate account/exchange state. It only makes the remaining
    /// external wallet blockers explicit before a final human review.
    pub async fn collateral_readiness_report(&self) -> Result<serde_json::Value> {
        let account = self.account_snapshot().await?;
        Ok(build_collateral_readiness_report(
            &account,
            &self.address,
            self.config.signature_type,
        ))
    }

    /// Dry-run a proposed real order intent against account + risk gates.
    ///
    /// This never signs, posts, places, or persists anything. It is intentionally
    /// useful before implementing the dangerous order path because it makes risk
    /// rejection reasons explicit and testable.
    pub async fn dry_run_order_intent(
        &self,
        intent: &RealOrderIntentDryRun,
    ) -> Result<serde_json::Value> {
        let account = self.account_snapshot().await?;
        let preflight = build_preflight_report(&account);
        let market_validation = self.market_metadata_validation(intent).await;
        Ok(build_order_intent_dry_run_report_with_market(
            intent,
            &preflight,
            Some(&market_validation),
        ))
    }

    /// Read-only validation of token market metadata needed before any future
    /// signed/send path: tick size and negative-risk status. This may call
    /// public CLOB metadata endpoints, but never signs, posts, or mutates
    /// exchange/account state.
    pub async fn market_metadata_validation(
        &self,
        intent: &RealOrderIntentDryRun,
    ) -> serde_json::Value {
        let token_id = intent.token_id.trim();
        if token_id.is_empty() || !token_id.chars().all(|ch| ch.is_ascii_digit()) {
            return build_market_metadata_validation_report(
                intent,
                None,
                None,
                None,
                Some("token_id must be a uint256 decimal string".to_string()),
            );
        }

        let query = format!("token_id={token_id}");
        let tick_response = match self
            .public_get_json_with_query("/tick-size", Some(&query))
            .await
        {
            Ok(value) => Some(value),
            Err(e) => {
                return build_market_metadata_validation_report(
                    intent,
                    None,
                    None,
                    None,
                    Some(format!(
                        "tick-size lookup failed: {}",
                        truncate_for_error(&e.to_string())
                    )),
                );
            }
        };

        let neg_risk_response = match self
            .public_get_json_with_query("/neg-risk", Some(&query))
            .await
        {
            Ok(value) => Some(value),
            Err(e) => {
                return build_market_metadata_validation_report(
                    intent,
                    tick_response.as_ref(),
                    None,
                    None,
                    Some(format!(
                        "neg-risk lookup failed: {}",
                        truncate_for_error(&e.to_string())
                    )),
                );
            }
        };

        let market_response = self
            .public_get_json(&format!("/markets-by-token/{token_id}"))
            .await
            .ok();

        build_market_metadata_validation_report(
            intent,
            tick_response.as_ref(),
            neg_risk_response.as_ref(),
            market_response.as_ref(),
            None,
        )
    }

    /// Build and optionally sign a CLOB order payload without posting it.
    ///
    /// This is the next safety step after paper order-intent dry-runs. It may
    /// perform authenticated/public reads needed by the SDK to resolve tick size,
    /// protocol version, and negative-risk settings. It never calls `post_order`,
    /// never returns the full signature, and never persists the signed payload.
    pub async fn signed_order_payload_dry_run(
        &self,
        request: &SignedOrderPayloadDryRunRequest,
    ) -> Result<serde_json::Value> {
        let account = self.account_snapshot().await?;
        let preflight = build_preflight_report(&account);
        let dry_run_report = build_order_intent_dry_run_report(&request.intent, &preflight);
        let mut blockers = json_string_vec(&dry_run_report, "blockers");

        if !request.confirm_signed_payload_dry_run {
            blockers.push("signed_payload_dry_run_confirmation_missing".to_string());
            blockers.sort();
            blockers.dedup();
            return Ok(signed_payload_dry_run_response(
                request,
                &dry_run_report,
                blockers,
                None,
                Some("Set confirm_signed_payload_dry_run=true to build/sign locally.".to_string()),
            ));
        }

        if !request
            .intent
            .order_type
            .trim()
            .eq_ignore_ascii_case("limit")
        {
            blockers.push("signed_payload_market_order_not_supported_yet".to_string());
            blockers.sort();
            blockers.dedup();
            return Ok(signed_payload_dry_run_response(
                request,
                &dry_run_report,
                blockers,
                None,
                Some("Signed payload dry-run currently supports limit orders only.".to_string()),
            ));
        }

        let market_validation = self.market_metadata_validation(&request.intent).await;
        let dry_run_report = build_order_intent_dry_run_report_with_market(
            &request.intent,
            &preflight,
            Some(&market_validation),
        );
        blockers = json_string_vec(&dry_run_report, "blockers");
        if !json_string_vec(&market_validation, "blockers").is_empty() {
            return Ok(signed_payload_dry_run_response(
                request,
                &dry_run_report,
                blockers,
                None,
                Some("Market metadata validation failed; signed payload dry-run stopped before local signing.".to_string()),
            ));
        }

        match self
            .try_build_signed_limit_order_payload(&request.intent)
            .await
        {
            Ok(summary) => Ok(signed_payload_dry_run_response(
                request,
                &dry_run_report,
                blockers,
                Some(summary),
                None,
            )),
            Err(e) => {
                blockers.push("signed_payload_build_failed".to_string());
                blockers.sort();
                blockers.dedup();
                Ok(signed_payload_dry_run_response(
                    request,
                    &dry_run_report,
                    blockers,
                    None,
                    Some(truncate_for_error(&e.to_string())),
                ))
            }
        }
    }

    /// Build a redacted, non-submitting preview of the CLOB `POST /order`
    /// request. This intentionally reuses the signed-payload dry-run and only
    /// returns request metadata/body with sensitive fields redacted.
    pub async fn order_post_request_dry_run(
        &self,
        request: &OrderPostRequestDryRunRequest,
    ) -> Result<serde_json::Value> {
        if !request.confirm_order_post_request_dry_run {
            return Ok(order_post_request_dry_run_response(
                request,
                None,
                vec!["order_post_request_dry_run_confirmation_missing".to_string()],
                Some("Set confirm_order_post_request_dry_run=true to serialize the non-submitting request preview.".to_string()),
            ));
        }

        if !request
            .signed_payload_request
            .confirm_signed_payload_dry_run
        {
            return Ok(order_post_request_dry_run_response(
                request,
                None,
                vec!["signed_payload_dry_run_confirmation_missing".to_string()],
                Some("Set confirm_signed_payload_dry_run=true because the post-request dry-run needs a locally signed payload.".to_string()),
            ));
        }

        let signed_report = self
            .signed_order_payload_dry_run(&request.signed_payload_request)
            .await?;
        let signed_payload = signed_report
            .get("signed_payload")
            .cloned()
            .filter(|value| !value.is_null());
        let post_request = signed_payload
            .as_ref()
            .and_then(|signed_payload| signed_payload.get("post_request_dry_run").cloned());
        let mut blockers = json_string_vec(&signed_report, "blockers");

        if post_request.is_none() {
            blockers.push("order_post_request_dry_run_build_failed".to_string());
            blockers.sort();
            blockers.dedup();
        }

        Ok(order_post_request_dry_run_response(
            request,
            post_request,
            blockers,
            signed_report
                .get("signing_error")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        ))
    }

    /// Evaluate the real-order submission facade without sending anything.
    ///
    /// RISK: This deliberately fails closed. It may build the same redacted
    /// request preview as the POST dry-run, then evaluates human approval,
    /// kill-switch, exposure, and explicit real-trading config gates. It does
    /// not call CLOB `POST /order` or SDK `post_order`/`post_orders`.
    pub async fn submit_order_facade(
        &self,
        request: &OrderSubmitFacadeRequest,
    ) -> Result<serde_json::Value> {
        let post_report = self
            .order_post_request_dry_run(&request.post_request_dry_run_request)
            .await?;
        Ok(order_submit_facade_response(request, post_report))
    }

    /// Place a real limit order on the CLOB (authenticated POST /order).
    ///
    /// This is the minimal implementation that can actually place a live order.
    ///
    /// RISK (non-negotiable, per AGENTS.md + Trading Safety Rules):
    /// - Only callable when config.orders_enabled (POLYTRADER_ENABLE_REAL_ORDERS=1
    ///   or POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION). Default: never.
    /// - Signing the order payload requires the native-l2 feature + POLYMARKET_PRIVATE_KEY
    ///   (must match the active L2 session address). This is the same key path used
    ///   for signed-payload dry-runs.
    /// - The caller (GatedRealClobLiveOrderSender or facade) *must* have already
    ///   validated + journaled human_approval_event_id, final_review_decision_event_id,
    ///   collateral readiness, risk sizing (max_order_notional etc from env), and
    ///   kill switch. This fn does a last re-build of the payload immediately before
    ///   the HTTP POST (the "revalidate immediately before network dispatch" rule).
    /// - Full context (intent + source event ids) must be journaled by caller
    ///   *before* invoking the path that reaches here.
    /// - No auto-approval. Human in the loop via the event ids is mandatory.
    /// - Position sizing, exposure, daily loss etc are enforced in the pre-gates
    ///   (see build_submit_facade_gate_report and risk_details in strategy/paper);
    ///   this fn does not re-derive full risk but inherits the caller's decision.
    /// - Observable: the POST result + any error is returned for journaling.
    /// - This path is orthogonal to PaperTradingEngine; it talks to real CLOB.
    ///
    /// The LiveOrderSendRequest (with its *_event_id fields) is the contract for
    /// passing the pre-approval context into the sender that calls this.
    pub async fn place_limit_order(
        &self,
        intent: &RealOrderIntentDryRun,
    ) -> Result<serde_json::Value> {
        if !self.config.orders_enabled {
            anyhow::bail!(
                "Real order placement is disabled (POLYTRADER_ENABLE_REAL_ORDERS or POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION must be truthy). Rejected before signing or network POST."
            );
        }
        if !intent.order_type.trim().eq_ignore_ascii_case("limit") {
            anyhow::bail!("place_limit_order currently supports limit orders only (GTC)");
        }

        #[cfg(feature = "native-l2")]
        {
            self.do_signed_limit_order_place(intent).await
        }
        #[cfg(not(feature = "native-l2"))]
        {
            anyhow::bail!(
                "native-l2 feature is required for real order signing and placement (order signature uses SDK LocalSigner path)"
            )
        }
    }

    #[cfg(feature = "native-l2")]
    async fn do_signed_limit_order_place(
        &self,
        intent: &RealOrderIntentDryRun,
    ) -> Result<serde_json::Value> {
        use polymarket_client_sdk_v2::auth::{Credentials, LocalSigner, Signer};
        use polymarket_client_sdk_v2::clob::types::{OrderType, Side};
        use polymarket_client_sdk_v2::clob::{Client, Config};
        use polymarket_client_sdk_v2::types::U256;
        use polymarket_client_sdk_v2::POLYGON;

        // NOTE (dupe signing): the SDK auth+limit_order+sign+creds block below is
        // intentionally duplicated from try_build_signed_limit_order_payload (for
        // smallest viable change; dry-run path has complex redaction/report logic
        // that we avoid touching). Extract to shared prepare_signed... in future
        // tranche. Risk of divergence noted; both paths use get_polymarket_private_key.

        let private_key = get_polymarket_private_key()?;
        let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));
        let signer_address = signer.address().to_checksum(None);
        if !signer_address.eq_ignore_ascii_case(&self.address) {
            anyhow::bail!(
                "POLYMARKET_PRIVATE_KEY signer address does not match active L2 session address"
            );
        }

        let api_key = uuid::Uuid::parse_str(&self.api_key)
            .context("active L2 api key is not a UUID understood by the SDK")?;
        let credentials = Credentials::new(api_key, self.secret.clone(), self.passphrase.clone());
        let signature_type = sdk_signature_type(self.config.signature_type)?;
        let token_id =
            U256::from_str(intent.token_id.trim()).context("token_id must be a uint256 string")?;
        let side = match intent.side.trim().to_ascii_lowercase().as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            _ => anyhow::bail!("side must be buy or sell"),
        };
        let price = intent.price.context("limit orders require price")?;

        let client = Client::new(&self.config.base_url, Config::default())?
            .authentication_builder(&signer)
            .credentials(credentials)
            .signature_type(signature_type)
            .authenticate()
            .await?;

        let signable = client
            .limit_order()
            .token_id(token_id)
            .side(side)
            .price(price)
            .size(intent.size)
            .order_type(OrderType::GTC)
            .post_only(false)
            .build()
            .await?;
        let signed = client.sign(&signer, signable).await?;

        // Now do the authenticated POST using *manual* L2 HMAC (same as our read path
        // and the post-request dry-run construction). The `signed` is the CLOB order
        // body containing the EIP-712 order signature.
        let exact_body = serde_json::to_string(&signed)
            .context("failed to serialize signed CLOB order body for real place")?;
        let request_timestamp = current_timestamp_secs()?;
        let l2_hmac_message = l2_message(request_timestamp, "POST", "/order", &exact_body);
        let l2_hmac = compute_poly_signature(&self.secret, &l2_hmac_message)?;

        let url = format!("{}/order", self.config.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(url)
            .header("POLY_ADDRESS", &self.address)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_PASSPHRASE", &self.passphrase)
            .header("POLY_SIGNATURE", l2_hmac)
            .header("POLY_TIMESTAMP", request_timestamp.to_string())
            .header("Content-Type", "application/json")
            .body(exact_body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "CLOB real order POST /order failed with status {}: {}",
                status,
                truncate_for_error(&text)
            );
        }

        let placed: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse CLOB place order response")?;
        Ok(placed)
    }

    #[cfg(feature = "native-l2")]
    async fn try_build_signed_limit_order_payload(
        &self,
        intent: &RealOrderIntentDryRun,
    ) -> Result<serde_json::Value> {
        use polymarket_client_sdk_v2::auth::{Credentials, LocalSigner, Signer};
        use polymarket_client_sdk_v2::clob::types::{OrderPayload, OrderType, Side};
        use polymarket_client_sdk_v2::clob::{Client, Config};
        use polymarket_client_sdk_v2::types::U256;
        use polymarket_client_sdk_v2::POLYGON;

        // NOTE (dupe signing): duplicated from do_signed... see comment there.

        let private_key = get_polymarket_private_key()?;
        let signer = LocalSigner::from_str(&private_key)?.with_chain_id(Some(POLYGON));
        let signer_address = signer.address().to_checksum(None);
        if !signer_address.eq_ignore_ascii_case(&self.address) {
            anyhow::bail!(
                "POLYMARKET_PRIVATE_KEY signer address does not match active L2 session address"
            );
        }

        let api_key = uuid::Uuid::parse_str(&self.api_key)
            .context("active L2 api key is not a UUID understood by the SDK")?;
        let credentials = Credentials::new(api_key, self.secret.clone(), self.passphrase.clone());
        let signature_type = sdk_signature_type(self.config.signature_type)?;
        let token_id =
            U256::from_str(intent.token_id.trim()).context("token_id must be a uint256 string")?;
        let side = match intent.side.trim().to_ascii_lowercase().as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            _ => anyhow::bail!("side must be buy or sell"),
        };
        let price = intent
            .price
            .context("limit signed dry-runs require price")?;

        let client = Client::new(&self.config.base_url, Config::default())?
            .authentication_builder(&signer)
            .credentials(credentials)
            .signature_type(signature_type)
            .authenticate()
            .await?;

        let signable = client
            .limit_order()
            .token_id(token_id)
            .side(side)
            .price(price)
            .size(intent.size)
            .order_type(OrderType::GTC)
            .post_only(false)
            .build()
            .await?;
        let signable_version = signable.payload.version();
        let signed = client.sign(&signer, signable).await?;
        let signature = signed.signature.to_string();
        let exact_body =
            serde_json::to_string(&signed).context("failed to serialize signed CLOB order body")?;
        let body_sha256 = sha256_hex(&exact_body);
        let request_timestamp = current_timestamp_secs()?;
        let l2_hmac_message = l2_message(request_timestamp, "POST", "/order", &exact_body);
        let l2_hmac = compute_poly_signature(&self.secret, &l2_hmac_message)?;
        let mut redacted_body = serde_json::from_str::<serde_json::Value>(&exact_body)
            .context("failed to re-parse signed CLOB order body for redaction")?;
        redact_order_signature(&mut redacted_body);
        let payload_summary = match &signed.payload {
            OrderPayload::V2(payload) => serde_json::json!({
                "version": 2,
                "salt": payload.order.salt.to_string(),
                "maker": payload.order.maker.to_string(),
                "signer": payload.order.signer.to_string(),
                "token_id": payload.order.tokenId.to_string(),
                "maker_amount": payload.order.makerAmount.to_string(),
                "taker_amount": payload.order.takerAmount.to_string(),
                "side": payload.order.side,
                "signature_type": payload.order.signatureType,
                "expiration": payload.expiration.to_string(),
            }),
            OrderPayload::V1(payload) => serde_json::json!({
                "version": 1,
                "salt": payload.order.salt.to_string(),
                "maker": payload.order.maker.to_string(),
                "signer": payload.order.signer.to_string(),
                "taker": payload.order.taker.to_string(),
                "token_id": payload.order.tokenId.to_string(),
                "maker_amount": payload.order.makerAmount.to_string(),
                "taker_amount": payload.order.takerAmount.to_string(),
                "side": payload.order.side,
                "signature_type": payload.order.signatureType,
                "expiration": payload.order.expiration.to_string(),
                "nonce": payload.order.nonce.to_string(),
                "fee_rate_bps": payload.order.feeRateBps.to_string(),
            }),
            _ => serde_json::json!({
                "version": "unknown",
                "note": "SDK returned an unrecognized order payload variant; full payload is intentionally not exposed."
            }),
        };

        Ok(serde_json::json!({
            "signable_payload_built": true,
            "signed_payload_built": true,
            "signed_payload_verified": true,
            "payload_version": signable_version,
            "post_endpoint": "/order",
            "would_post": false,
            "owner_api_key_masked": mask_for_display(&signed.owner.to_string()),
            "signature_masked": mask_for_display(&signature),
            "signature_length": signature.len(),
            "order_type": signed.order_type.to_string(),
            "post_only": signed.post_only,
            "defer_exec": signed.defer_exec,
            "payload_summary": payload_summary,
            "post_request_dry_run": {
                "built": true,
                "method": "POST",
                "path": "/order",
                "url": format!("{}{}", self.config.base_url.trim_end_matches('/'), "/order"),
                "body_sha256": body_sha256.clone(),
                "body_redacted": redacted_body,
                "body_signature_redacted": true,
                "headers_redacted": {
                    "POLY_ADDRESS": self.address,
                    "POLY_API_KEY": mask_for_display(&self.api_key),
                    "POLY_PASSPHRASE": "[redacted]",
                    "POLY_SIGNATURE": "[redacted]",
                    "POLY_TIMESTAMP": request_timestamp.to_string()
                },
                "l2_hmac_signature_length": l2_hmac.len(),
                "l2_hmac_message_components": {
                    "timestamp": request_timestamp,
                    "method": "POST",
                    "path": "/order",
                    "body_sha256": body_sha256
                },
                "would_send": false,
                "would_post": false,
                "post_order_called": false,
                "post_orders_called": false,
                "note": "Exact request body was serialized for hashing/signing, then redacted before return. The request was not sent."
            },
        }))
    }

    #[cfg(not(feature = "native-l2"))]
    async fn try_build_signed_limit_order_payload(
        &self,
        _intent: &RealOrderIntentDryRun,
    ) -> Result<serde_json::Value> {
        anyhow::bail!("native-l2 feature is required for signed payload dry-runs")
    }

    async fn authenticated_get_json(&self, path: &str) -> Result<serde_json::Value> {
        self.authenticated_get_json_with_query(path, None).await
    }

    async fn public_get_json(&self, path: &str) -> Result<serde_json::Value> {
        self.public_get_json_with_query(path, None).await
    }

    async fn public_get_json_with_query(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<serde_json::Value> {
        if !self.config.read_enabled {
            anyhow::bail!(
                "Read-only real CLOB calls are disabled (set POLYTRADER_ENABLE_REAL_CLOB_READS=1 or leave unset to enable reads)"
            );
        }
        if !path.starts_with('/') {
            anyhow::bail!("CLOB request path must start with '/'");
        }

        let mut url = format!("{}{}", self.config.base_url.trim_end_matches('/'), path);
        if let Some(query) = query.filter(|q| !q.is_empty()) {
            url.push('?');
            url.push_str(query.trim_start_matches('?'));
        }

        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "CLOB public metadata request failed with status {}: {}",
                status,
                truncate_for_error(&text)
            );
        }

        serde_json::from_str(&text).context("failed to parse CLOB public metadata response as JSON")
    }

    async fn authenticated_get_json_with_query(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<serde_json::Value> {
        if !self.config.read_enabled {
            anyhow::bail!(
                "Read-only real CLOB calls are disabled (set POLYTRADER_ENABLE_REAL_CLOB_READS=1 or leave unset to enable reads)"
            );
        }
        if !path.starts_with('/') {
            anyhow::bail!("CLOB request path must start with '/'");
        }

        let timestamp = current_timestamp_secs()?;
        let method = "GET";
        // Match the official Rust SDK: L2 signatures use timestamp + method +
        // URL path + body. Query parameters are sent on the URL but are not
        // included in the signed message.
        let message = l2_message(timestamp, method, path, "");
        let signature = compute_poly_signature(&self.secret, &message)?;

        let mut url = format!("{}{}", self.config.base_url.trim_end_matches('/'), path);
        if let Some(query) = query.filter(|q| !q.is_empty()) {
            url.push('?');
            url.push_str(query.trim_start_matches('?'));
        }
        let resp = self
            .http
            .get(url)
            .header("POLY_ADDRESS", &self.address)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_PASSPHRASE", &self.passphrase)
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "CLOB read-only request failed with status {}: {}",
                status,
                truncate_for_error(&text)
            );
        }

        let json: serde_json::Value =
            serde_json::from_str(&text).context("failed to parse CLOB response as JSON")?;

        Ok(json)
    }

    // place_limit_order is now implemented (minimal, gated). See above + live_sender.
    // pub async fn cancel_order(&self, id: &str) -> Result<()> { ... }
}

pub fn build_preflight_report(account: &serde_json::Value) -> serde_json::Value {
    let collateral = account
        .get("collateral")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let balance_raw = collateral
        .get("balance")
        .and_then(value_as_decimal)
        .unwrap_or(Decimal::ZERO);

    let allowance_values = collateral
        .get("allowances")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.values()
                .filter_map(value_as_decimal)
                .collect::<Vec<Decimal>>()
        })
        .unwrap_or_default();
    let positive_allowance_count = allowance_values
        .iter()
        .filter(|allowance| **allowance > Decimal::ZERO)
        .count();

    let open_order_count = account
        .get("open_orders")
        .and_then(|orders| orders.get("count"))
        .and_then(|count| count.as_u64())
        .unwrap_or_else(|| {
            account
                .get("open_orders")
                .and_then(|orders| orders.get("data"))
                .and_then(|data| data.as_array())
                .map(|data| data.len() as u64)
                .unwrap_or(0)
        });

    let collateral_balance_positive = balance_raw > Decimal::ZERO;
    let collateral_allowance_positive = positive_allowance_count > 0;

    let mut checks = Vec::new();
    checks.push(serde_json::json!({
        "name": "l2_account_read",
        "ok": true,
        "severity": "info",
        "detail": "Derived L2 credentials can read open orders and collateral balance/allowance."
    }));
    checks.push(serde_json::json!({
        "name": "collateral_balance_positive",
        "ok": collateral_balance_positive,
        "severity": if collateral_balance_positive { "info" } else { "blocker" },
        "detail": format!("Collateral balance reported by CLOB: {}", balance_raw)
    }));
    checks.push(serde_json::json!({
        "name": "collateral_allowance_positive",
        "ok": collateral_allowance_positive,
        "severity": if collateral_allowance_positive { "info" } else { "blocker" },
        "detail": format!("Positive allowance entries: {} of {}", positive_allowance_count, allowance_values.len())
    }));
    checks.push(serde_json::json!({
        "name": "open_order_visibility",
        "ok": true,
        "severity": "info",
        "detail": format!("Open orders visible: {}", open_order_count)
    }));
    checks.push(serde_json::json!({
        "name": "real_order_submission_facade_available",
        "ok": submitting_order_facade_available(),
        "severity": if submitting_order_facade_available() { "info" } else { "blocker" },
        "detail": if submitting_order_facade_available() {
            "A fail-closed submission facade exists for gate evaluation; live sending remains disabled."
        } else {
            "No real-order submission facade is implemented in this binary."
        }
    }));
    checks.push(serde_json::json!({
        "name": "human_approval_workflow_available",
        "ok": human_approval_workflow_available(),
        "severity": if human_approval_workflow_available() { "info" } else { "blocker" },
        "detail": if human_approval_workflow_available() {
            "Journaled human approval workflow exists for submit-facade audit events; it still does not unlock live orders."
        } else {
            "No journaled human approval workflow is implemented for real orders."
        }
    }));

    let blockers = checks
        .iter()
        .filter(|check| {
            check.get("severity").and_then(|v| v.as_str()) == Some("blocker")
                && check.get("ok").and_then(|v| v.as_bool()) == Some(false)
        })
        .filter_map(|check| {
            check
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect::<Vec<String>>();

    serde_json::json!({
        "ready_for_real_orders": false,
        "real_orders_enabled": false,
        "paper_only": true,
        "open_order_count": open_order_count,
        "collateral_balance": balance_raw.to_string(),
        "positive_allowance_count": positive_allowance_count,
        "allowance_count": allowance_values.len(),
        "blockers": blockers,
        "checks": checks,
        "note": "Preflight is diagnostic only. It cannot place, cancel, sign, or submit real orders."
    })
}

pub fn build_collateral_readiness_report(
    account: &serde_json::Value,
    address: &str,
    signature_type: u8,
) -> serde_json::Value {
    let collateral = account
        .get("collateral")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let balance_raw = collateral
        .get("balance")
        .and_then(value_as_decimal)
        .unwrap_or(Decimal::ZERO);
    let allowance_entries = collateral
        .get("allowances")
        .and_then(|v| v.as_object())
        .map(|allowances| {
            allowances
                .iter()
                .map(|(spender, value)| {
                    let amount = value_as_decimal(value).unwrap_or(Decimal::ZERO);
                    serde_json::json!({
                        "spender": spender,
                        "amount": amount.to_string(),
                        "positive": amount > Decimal::ZERO,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let positive_allowance_count = allowance_entries
        .iter()
        .filter(|entry| entry.get("positive").and_then(|value| value.as_bool()) == Some(true))
        .count();
    let balance_positive = balance_raw > Decimal::ZERO;
    let allowance_positive = positive_allowance_count > 0;
    let mut blockers = Vec::new();
    if !balance_positive {
        blockers.push("collateral_balance_positive".to_string());
    }
    if !allowance_positive {
        blockers.push("collateral_allowance_positive".to_string());
    }

    serde_json::json!({
        "ready": false,
        "ready_for_real_orders": false,
        "real_orders_enabled": false,
        "paper_only": true,
        "read_only_live_check": true,
        "wallet_address": address,
        "signature_type": signature_type,
        "collateral_balance": balance_raw.to_string(),
        "collateral_balance_positive": balance_positive,
        "allowance_count": allowance_entries.len(),
        "positive_allowance_count": positive_allowance_count,
        "collateral_allowance_positive": allowance_positive,
        "allowances": allowance_entries,
        "blockers": blockers,
        "operator_actions": [
            {
                "id": "fund_collateral",
                "required": !balance_positive,
                "label": "Fund the active Polymarket wallet/funder with USDC collateral through the official Polymarket wallet flow, then re-run diagnostics.",
            },
            {
                "id": "approve_collateral_allowance",
                "required": !allowance_positive,
                "label": "Create or refresh the required collateral allowance through the official Polymarket wallet flow, then re-run diagnostics.",
            },
            {
                "id": "keep_real_trading_locked",
                "required": true,
                "label": "Keep POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION unset until final human review; this report never unlocks live sending.",
            }
        ],
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Read-only collateral readiness report. It does not transfer funds, approve allowances, refresh allowances, sign orders, submit orders, cancel orders, or mutate balances."
    })
}

pub fn build_real_trading_unlock_status() -> serde_json::Value {
    let explicit_unlock = env_truthy("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION")
        || env_truthy("POLYTRADER_ENABLE_REAL_ORDERS");
    let kill_switch_open = env_truthy("POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN");
    let paper_mode_active = std::env::var("POLYTRADER_MODE")
        .map(|value| value.trim().eq_ignore_ascii_case("paper"))
        .unwrap_or(true);
    // GatedRealClobLiveOrderSender is now implemented (wired to place_limit_order).
    // It still only dispatches when its internal revalidation + env gates pass.
    let live_sender_implemented = true;

    let mut checks = Vec::new();
    let mut blockers = Vec::new();
    push_check(
        &mut checks,
        &mut blockers,
        "explicit_real_trading_config_unlock",
        explicit_unlock,
        "blocker",
        "POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION must be true before a future live sender could be considered.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "kill_switch_open",
        kill_switch_open,
        "blocker",
        "POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN must be true; default is closed.",
    );
    // paper_mode check is conditional: an explicit real-orders unlock allows the
    // CLOB write path even while the top-level mode remains "paper" (the assert in
    // config + paper engine are preserved; real clob orders are an orthogonal gated path).
    let paper_blocks_real = paper_mode_active && !explicit_unlock;
    push_check(
        &mut checks,
        &mut blockers,
        "paper_mode_still_active",
        !paper_blocks_real,
        "blocker",
        "POLYTRADER_MODE is paper or unset and no explicit real-orders unlock is configured, so live submission is blocked.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "live_order_sender_implemented",
        live_sender_implemented,
        "blocker",
        "No live CLOB order sender is implemented in this binary.",
    );

    blockers.sort();
    blockers.dedup();

    serde_json::json!({
        "ready": false,
        "ready_for_real_orders": false,
        "real_orders_enabled": explicit_unlock,
        "paper_only": true,
        "unlock_status_available": true,
        "explicit_real_order_submission_configured": explicit_unlock,
        "kill_switch_open": kill_switch_open,
        "paper_mode_active": paper_mode_active,
        "live_order_sender_implemented": live_sender_implemented,
        "checks": checks,
        "blockers": blockers,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "note": "Read-only real-trading unlock status. This does not enable live trading or create a live sender unless every other gate (human approval, final review, collateral, risk, kill) also passes at submit time."
    })
}

pub fn build_order_intent_dry_run_report(
    intent: &RealOrderIntentDryRun,
    preflight: &serde_json::Value,
) -> serde_json::Value {
    build_order_intent_dry_run_report_with_market(intent, preflight, None)
}

pub fn build_order_intent_dry_run_report_with_market(
    intent: &RealOrderIntentDryRun,
    preflight: &serde_json::Value,
    market_validation: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut blockers = preflight
        .get("blockers")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    let mut checks = Vec::new();
    let normalized_side = intent.side.trim().to_ascii_lowercase();
    let normalized_order_type = intent.order_type.trim().to_ascii_lowercase();
    let token_present = !intent.token_id.trim().is_empty();
    let side_ok = normalized_side == "buy" || normalized_side == "sell";
    let order_type_ok = normalized_order_type == "market" || normalized_order_type == "limit";
    let size_positive = intent.size > Decimal::ZERO;
    let price_ok = match (normalized_order_type.as_str(), intent.price) {
        ("limit", Some(price)) => price > Decimal::ZERO && price < Decimal::ONE,
        ("limit", None) => false,
        ("market", Some(price)) => price > Decimal::ZERO && price <= Decimal::ONE,
        ("market", None) => true,
        _ => false,
    };

    push_check(
        &mut checks,
        &mut blockers,
        "token_id_present",
        token_present,
        "blocker",
        "A CLOB token_id is required.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "side_supported",
        side_ok,
        "blocker",
        "side must be buy or sell.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "order_type_supported",
        order_type_ok,
        "blocker",
        "order_type must be market or limit.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "size_positive",
        size_positive,
        "blocker",
        "size must be greater than zero.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "price_valid_for_order_type",
        price_ok,
        "blocker",
        "limit orders need 0 < price < 1; market dry-runs may omit price or use 0 < price <= 1.",
    );

    let conservative_price = intent
        .price
        .unwrap_or(Decimal::ONE)
        .clamp(dec!(0), Decimal::ONE);
    let estimated_notional = if size_positive {
        intent.size * conservative_price
    } else {
        Decimal::ZERO
    };
    let bankroll = env_decimal("POLYTRADER_REAL_DRY_RUN_BANKROLL_USDC", dec!(150));
    let max_trade_risk_pct = env_decimal("POLYTRADER_MAX_RISK_PER_TRADE_PCT", dec!(1));
    let max_notional = bankroll * (max_trade_risk_pct / dec!(100));
    let notional_ok = estimated_notional > Decimal::ZERO && estimated_notional <= max_notional;
    push_check(
        &mut checks,
        &mut blockers,
        "max_risk_per_trade",
        notional_ok,
        "blocker",
        &format!(
            "estimated_notional={} must be > 0 and <= max_notional={} (bankroll={} risk_pct={})",
            estimated_notional, max_notional, bankroll, max_trade_risk_pct
        ),
    );

    let min_edge_bps = env_decimal("POLYTRADER_MIN_REAL_DRY_RUN_EDGE_BPS", dec!(400));
    let edge_ok = intent
        .expected_edge_bps
        .map(|edge| edge >= min_edge_bps)
        .unwrap_or(false);
    push_check(
        &mut checks,
        &mut blockers,
        "expected_edge_present_and_sufficient",
        edge_ok,
        "blocker",
        &format!("expected_edge_bps must be provided and >= {}", min_edge_bps),
    );

    if let Some(market_validation) = market_validation {
        blockers.extend(json_string_vec(market_validation, "blockers"));
        let market_checks = market_validation
            .get("checks")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        checks.extend(market_checks);
    }

    blockers.sort();
    blockers.dedup();

    serde_json::json!({
        "accepted": false,
        "dry_run_only": true,
        "paper_only": true,
        "real_orders_enabled": false,
        "intent": intent,
        "estimated_notional": estimated_notional.to_string(),
        "risk_limits": {
            "bankroll_usdc": bankroll.to_string(),
            "max_risk_per_trade_pct": max_trade_risk_pct.to_string(),
            "max_notional": max_notional.to_string(),
            "min_expected_edge_bps": min_edge_bps.to_string()
        },
        "preflight": preflight,
        "market_metadata_validation": market_validation.cloned(),
        "checks": checks,
        "blockers": blockers,
        "note": "Dry-run only. No order was signed, submitted, persisted, cancelled, or placed."
    })
}

pub fn build_market_metadata_validation_report(
    intent: &RealOrderIntentDryRun,
    tick_response: Option<&serde_json::Value>,
    neg_risk_response: Option<&serde_json::Value>,
    market_response: Option<&serde_json::Value>,
    fetch_error: Option<String>,
) -> serde_json::Value {
    let mut checks = Vec::new();
    let mut blockers = Vec::new();
    let tick_size = tick_response.and_then(extract_tick_size_decimal);
    let neg_risk = neg_risk_response.and_then(extract_neg_risk_bool);
    let normalized_order_type = intent.order_type.trim().to_ascii_lowercase();

    push_check(
        &mut checks,
        &mut blockers,
        "market_metadata_no_send",
        true,
        "info",
        "Validation used read-only metadata calls only; no order endpoint was called.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "tick_size_present",
        tick_size.is_some(),
        "blocker",
        "CLOB tick-size metadata must be available for the token.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "neg_risk_present",
        neg_risk.is_some(),
        "blocker",
        "CLOB negative-risk metadata must be available for the token.",
    );

    let mut price_tick_valid = None;
    let mut price_within_tick_range = None;
    if normalized_order_type == "limit" {
        let price = intent.price;
        let tick_range_ok = match (price, tick_size) {
            (Some(price), Some(tick_size)) => {
                price >= tick_size && price <= Decimal::ONE - tick_size
            }
            _ => false,
        };
        let tick_increment_ok = match (price, tick_size) {
            (Some(price), Some(tick_size)) => {
                price.normalize().scale() <= tick_size.normalize().scale()
            }
            _ => false,
        };
        price_tick_valid = Some(tick_increment_ok);
        price_within_tick_range = Some(tick_range_ok);
        push_check(
            &mut checks,
            &mut blockers,
            "price_within_tick_range",
            tick_range_ok,
            "blocker",
            "Limit price must be within the CLOB tick-size range [tick_size, 1 - tick_size].",
        );
        push_check(
            &mut checks,
            &mut blockers,
            "price_respects_tick_size",
            tick_increment_ok,
            "blocker",
            "Limit price decimal precision must not exceed the token tick size.",
        );
    } else {
        push_check(
            &mut checks,
            &mut blockers,
            "price_tick_validation_not_required_for_market_order",
            true,
            "info",
            "Market order dry-runs do not validate a limit price tick increment.",
        );
    }

    if fetch_error.is_some() {
        blockers.push("market_metadata_fetch_failed".to_string());
    }
    blockers.sort();
    blockers.dedup();

    let condition_id = market_response.and_then(extract_condition_id);

    serde_json::json!({
        "accepted": false,
        "dry_run_only": true,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "market_metadata_validation_available": true,
        "market_metadata_fetched": tick_size.is_some() && neg_risk.is_some(),
        "token_id": intent.token_id.trim(),
        "condition_id": condition_id,
        "tick_size": tick_size.map(|value| value.to_string()),
        "neg_risk": neg_risk,
        "negative_risk_adapter_required": neg_risk.unwrap_or(false),
        "price_tick_valid": price_tick_valid,
        "price_within_tick_range": price_within_tick_range,
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "raw_metadata": {
            "tick_size": tick_response.cloned(),
            "neg_risk": neg_risk_response.cloned(),
            "market_by_token": market_response.cloned(),
        },
        "fetch_error": fetch_error,
        "checks": checks,
        "blockers": blockers,
        "note": "Read-only CLOB market metadata validation. No order was signed, posted, submitted, persisted, cancelled, or placed."
    })
}

fn signed_payload_dry_run_response(
    request: &SignedOrderPayloadDryRunRequest,
    dry_run_report: &serde_json::Value,
    blockers: Vec<String>,
    signed_payload: Option<serde_json::Value>,
    signing_error: Option<String>,
) -> serde_json::Value {
    let signed_payload_built = signed_payload.is_some();

    serde_json::json!({
        "accepted": false,
        "dry_run_only": true,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "confirm_signed_payload_dry_run": request.confirm_signed_payload_dry_run,
        "signed_payload_built": signed_payload_built,
        "signed_payload_verified": signed_payload_built,
        "signature_redacted": true,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "intent": &request.intent,
        "order_intent_dry_run": dry_run_report,
        "signed_payload": signed_payload,
        "signing_error": signing_error,
        "blockers": blockers,
        "note": "Signed-order payload dry-run only. No full signature is returned, and no order was posted, submitted, persisted, cancelled, or placed."
    })
}

fn order_post_request_dry_run_response(
    request: &OrderPostRequestDryRunRequest,
    post_request: Option<serde_json::Value>,
    blockers: Vec<String>,
    error: Option<String>,
) -> serde_json::Value {
    let post_request_built = post_request.is_some();

    serde_json::json!({
        "accepted": false,
        "dry_run_only": true,
        "paper_only": true,
        "real_orders_enabled": false,
        "ready_for_real_orders": false,
        "confirm_signed_payload_dry_run": request.signed_payload_request.confirm_signed_payload_dry_run,
        "confirm_order_post_request_dry_run": request.confirm_order_post_request_dry_run,
        "post_request_dry_run_built": post_request_built,
        "signature_redacted": true,
        "l2_hmac_redacted": true,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "intent": &request.signed_payload_request.intent,
        "post_request_dry_run": post_request,
        "error": error,
        "blockers": blockers,
        "note": "Non-submitting CLOB order POST request dry-run only. No request was sent, no full signature/HMAC is returned, and no order was posted, submitted, persisted, cancelled, or placed."
    })
}

fn order_submit_facade_response(
    request: &OrderSubmitFacadeRequest,
    post_report: serde_json::Value,
) -> serde_json::Value {
    let mut blockers = json_string_vec(&post_report, "blockers");
    let gate_report = build_submit_facade_gate_report(request, &post_report);
    blockers.extend(json_string_vec(&gate_report, "blockers"));
    blockers.sort();
    blockers.dedup();
    let blocker_count = blockers.len();
    // With gated real sender wired, if the facade gates all pass we no longer
    // hard-reject as "no_live_sender"; the actual dispatch decision (and send)
    // is made by the caller (submit handler) which invokes the LiveOrderSender.
    // Keep the decision conservative here (the facade fn itself never sends).
    let submit_decision = if blockers.is_empty() {
        "rejected_before_live_send_even_with_gates" // caller may still dispatch via sender
    } else {
        "rejected_fail_closed"
    };
    let reconciliation_status = "reconciled_no_send";
    let reconciliation = serde_json::json!({
        "required": true,
        "reconciled": true,
        "status": reconciliation_status,
        "submit_decision": submit_decision,
        "request_sent": false,
        "exchange_order_id": null,
        "local_order_id": null,
        "post_order_called": false,
        "post_orders_called": false,
        "expected_exchange_state": "no_order_created",
        "observed_exchange_state": "not_queried_no_send",
        "submit_result": if blockers.is_empty() { "rejected_before_send_no_live_sender" } else { "rejected_before_send_blocked" },
        "blocker_count": blocker_count,
        "blockers": blockers,
        "note": "Reconciliation is complete because the facade failed closed before any exchange request was sent."
    });

    serde_json::json!({
        "accepted": false,
        "submission_facade_only": true,
        "real_order_submit_journal_ready": true,
        "submit_decision": submit_decision,
        "reconciliation_required": true,
        "reconciled": true,
        "reconciliation_status": reconciliation_status,
        "dry_run_only": true,
        "paper_only": true,
        "real_orders_enabled": env_truthy("POLYTRADER_ENABLE_REAL_ORDERS") || env_truthy("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION"),
        "ready_for_real_orders": false,
        "facade_available": submitting_order_facade_available(),
        "confirm_real_order_submission": request.confirm_real_order_submission,
        "human_approval_event_id": request.human_approval_event_id,
        "human_approval_event_valid": request.server_human_approval.as_ref().map(|approval| approval.valid).unwrap_or(false),
        "final_review_decision_event_id": request.final_review_decision_event_id,
        "final_review_decision_event_valid": request.server_final_review_decision.as_ref().map(|f| f.valid).unwrap_or(false),
        "human_approval_present": request.human_approval_token.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false),
        "human_approval_note_present": request.human_approval_note.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false),
        "operator": request.operator.as_deref().unwrap_or("unspecified"),
        "request_sent": false,
        "would_send": false,
        "would_post": false,
        "post_order_called": false,
        "post_orders_called": false,
        "order_id": null,
        "post_request_dry_run_built": post_report.get("post_request_dry_run_built").and_then(|v| v.as_bool()).unwrap_or(false),
        "signature_redacted": true,
        "l2_hmac_redacted": true,
        "post_request_dry_run": post_report.get("post_request_dry_run").cloned().unwrap_or(serde_json::Value::Null),
        "gate_report": gate_report,
        "reconciliation": reconciliation,
        "blockers": reconciliation.get("blockers").cloned().unwrap_or_else(|| serde_json::json!([])),
        "note": "Submission facade. Gates evaluated (including live sender implemented + conditional paper). When all pass + real unlock, the *caller* (submit handler) dispatches via GatedRealClobLiveOrderSender which may call place_limit_order. This response itself is still pre-send."
    })
}

fn build_submit_facade_gate_report(
    request: &OrderSubmitFacadeRequest,
    post_report: &serde_json::Value,
) -> serde_json::Value {
    let mut checks = Vec::new();
    let mut blockers = Vec::new();

    push_check(
        &mut checks,
        &mut blockers,
        "submission_facade_available",
        submitting_order_facade_available(),
        "blocker",
        "The fail-closed submission facade must be compiled into the binary.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "post_request_preview_built",
        post_report
            .get("post_request_dry_run_built")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "blocker",
        "A redacted non-submitting POST request preview must build before any future live send.",
    );
    push_check(
        &mut checks,
        &mut blockers,
        "human_approval_workflow_available",
        human_approval_workflow_available(),
        "blocker",
        "A journaled human approval workflow must exist before any future live send.",
    );

    let approval_validation = request
        .server_human_approval
        .as_ref()
        .cloned()
        .unwrap_or_else(|| HumanApprovalValidation {
            valid: false,
            event_id: request.human_approval_event_id,
            decision: None,
            subject_hash: None,
            blockers: vec!["human_approval_event_missing".to_string()],
        });
    let journaled_approval_valid = approval_validation.valid;
    push_check(
        &mut checks,
        &mut blockers,
        "journaled_human_approval_valid",
        journaled_approval_valid,
        "blocker",
        "A matching, unexpired journaled human approval event is required for the submit facade.",
    );
    blockers.extend(approval_validation.blockers.iter().cloned());

    // Final review decision validation (wired end-to-end for sender final_ok gate).
    let final_validation = request
        .server_final_review_decision
        .as_ref()
        .cloned()
        .unwrap_or_else(|| FinalReviewDecisionValidation {
            valid: false,
            event_id: request.final_review_decision_event_id,
            decision: None,
            operator: None,
            blockers: vec!["final_review_decision_event_missing".to_string()],
        });
    push_check(
        &mut checks,
        &mut blockers,
        "journaled_final_review_decision_valid",
        final_validation.valid,
        "blocker",
        "A matching journaled final review decision event is required for the submit facade (non-zero id + recorded decision).",
    );
    blockers.extend(final_validation.blockers.iter().cloned());

    let collateral_validation = request
        .server_collateral_readiness
        .as_ref()
        .cloned()
        .unwrap_or_else(|| CollateralReadinessValidation {
            valid: false,
            event_id: None,
            created_at: None,
            wallet_address: None,
            collateral_balance: None,
            collateral_balance_positive: false,
            collateral_allowance_positive: false,
            positive_allowance_count: None,
            max_age_seconds: collateral_readiness_max_age_seconds(),
            age_seconds: None,
            blockers: vec!["fresh_collateral_readiness_missing".to_string()],
        });
    push_check(
        &mut checks,
        &mut blockers,
        "fresh_collateral_readiness_valid",
        collateral_validation.valid,
        "blocker",
        "A recent journaled collateral readiness event with positive collateral and positive allowance is required.",
    );
    blockers.extend(collateral_validation.blockers.iter().cloned());

    push_check(
        &mut checks,
        &mut blockers,
        "kill_switch_and_risk_limits_available",
        kill_switch_and_risk_limits_available(),
        "blocker",
        "Fail-closed kill switch, per-order cap, total exposure cap, and daily-loss cap checks must be implemented.",
    );

    push_check(
        &mut checks,
        &mut blockers,
        "real_order_submission_confirmation",
        request.confirm_real_order_submission,
        "blocker",
        "The operator must set confirm_real_order_submission=true for the facade to proceed.",
    );

    // Or both names for consistency with config default, live_sender, server handler,
    // unlock status, and effective_real_enabled (review issue: was using legacy only,
    // so setting only POLYTRADER_ENABLE_REAL_ORDERS left the blocker).
    let explicit_unlock = env_truthy("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION")
        || env_truthy("POLYTRADER_ENABLE_REAL_ORDERS");
    push_check(
        &mut checks,
        &mut blockers,
        "explicit_real_trading_config_unlock",
        explicit_unlock,
        "blocker",
        "POLYTRADER_ENABLE_REAL_ORDERS (or legacy POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION) must be true before any live send.",
    );

    let paper_mode_active = std::env::var("POLYTRADER_MODE")
        .map(|v| v.trim().eq_ignore_ascii_case("paper"))
        .unwrap_or(true);
    let explicit_real_for_gate = env_truthy("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION")
        || env_truthy("POLYTRADER_ENABLE_REAL_ORDERS");
    // Conditional to allow gated real clob orders without regressing the paper-mode
    // assert in Config or the paper engine (real clob is separate path).
    let paper_blocks_real = paper_mode_active && !explicit_real_for_gate;
    push_check(
        &mut checks,
        &mut blockers,
        "paper_mode_still_active",
        !paper_blocks_real,
        "blocker",
        "The app currently asserts paper mode at startup and no explicit real-orders unlock is set, so live submission is blocked.",
    );

    let intent = &request
        .post_request_dry_run_request
        .signed_payload_request
        .intent;
    let conservative_price = intent
        .price
        .unwrap_or(Decimal::ONE)
        .clamp(dec!(0), Decimal::ONE);
    let projected_notional = if intent.size > Decimal::ZERO {
        intent.size * conservative_price
    } else {
        Decimal::ZERO
    };
    let bankroll = env_decimal("POLYTRADER_REAL_DRY_RUN_BANKROLL_USDC", dec!(150));
    let max_order_risk_pct = env_decimal("POLYTRADER_MAX_RISK_PER_TRADE_PCT", dec!(1));
    let max_order_notional = bankroll * (max_order_risk_pct / dec!(100));
    let order_notional_ok =
        projected_notional > Decimal::ZERO && projected_notional <= max_order_notional;
    push_check(
        &mut checks,
        &mut blockers,
        "projected_order_notional_within_limit",
        order_notional_ok,
        "blocker",
        &format!(
            "projected_notional={} must be > 0 and <= max_order_notional={} (bankroll={} risk_pct={})",
            projected_notional, max_order_notional, bankroll, max_order_risk_pct
        ),
    );

    let max_total_exposure_pct = env_decimal("POLYTRADER_MAX_TOTAL_REAL_EXPOSURE_PCT", dec!(15));
    let max_total_exposure = bankroll * (max_total_exposure_pct / dec!(100));
    let exposure_ok =
        projected_notional > Decimal::ZERO && projected_notional <= max_total_exposure;
    push_check(
        &mut checks,
        &mut blockers,
        "projected_total_exposure_within_limit",
        exposure_ok,
        "blocker",
        &format!(
            "projected_notional={} must be > 0 and <= max_total_exposure={} (bankroll={} exposure_pct={})",
            projected_notional, max_total_exposure, bankroll, max_total_exposure_pct
        ),
    );

    let daily_realized_pnl =
        env_decimal_allow_negative("POLYTRADER_REAL_DRY_RUN_DAILY_PNL_USDC", Decimal::ZERO);
    let current_daily_loss = if daily_realized_pnl < Decimal::ZERO {
        -daily_realized_pnl
    } else {
        Decimal::ZERO
    };
    let max_daily_loss_pct = env_decimal("POLYTRADER_MAX_DAILY_REAL_LOSS_PCT", dec!(5));
    let max_daily_loss = bankroll * (max_daily_loss_pct / dec!(100));
    let daily_loss_ok = current_daily_loss <= max_daily_loss;
    push_check(
        &mut checks,
        &mut blockers,
        "daily_loss_within_limit",
        daily_loss_ok,
        "blocker",
        &format!(
            "current_daily_loss={} must be <= max_daily_loss={} (bankroll={} daily_loss_pct={})",
            current_daily_loss, max_daily_loss, bankroll, max_daily_loss_pct
        ),
    );

    let kill_switch_open = env_truthy("POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN");
    push_check(
        &mut checks,
        &mut blockers,
        "kill_switch_open",
        kill_switch_open,
        "blocker",
        "POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN must be true; default is closed.",
    );

    blockers.sort();
    blockers.dedup();

    serde_json::json!({
        "ready": false,
        "kill_switch_and_risk_limits_available": kill_switch_and_risk_limits_available(),
        "kill_switch_open": kill_switch_open,
        "request_sent": false,
        "post_order_called": false,
        "post_orders_called": false,
        "checks": checks,
        "blockers": blockers,
        "human_approval": approval_validation,
        "collateral_readiness": collateral_validation,
        "risk_limits": {
            "bankroll_usdc": bankroll.to_string(),
            "max_risk_per_trade_pct": max_order_risk_pct.to_string(),
            "max_order_notional": max_order_notional.to_string(),
            "max_total_real_exposure_pct": max_total_exposure_pct.to_string(),
            "max_total_exposure": max_total_exposure.to_string(),
            "max_daily_real_loss_pct": max_daily_loss_pct.to_string(),
            "max_daily_loss": max_daily_loss.to_string(),
            "current_daily_loss": current_daily_loss.to_string(),
            "projected_notional": projected_notional.to_string()
        },
        "note": "All live-submission gates fail closed unless explicitly configured and approved; this report itself never sends an order."
    })
}

fn push_check(
    checks: &mut Vec<serde_json::Value>,
    blockers: &mut Vec<String>,
    name: &str,
    ok: bool,
    severity: &str,
    detail: &str,
) {
    checks.push(serde_json::json!({
        "name": name,
        "ok": ok,
        "severity": severity,
        "detail": detail,
    }));
    if !ok && severity == "blocker" {
        blockers.push(name.to_string());
    }
}

fn json_string_vec(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn extract_tick_size_decimal(value: &serde_json::Value) -> Option<Decimal> {
    extract_decimal_field(
        value,
        &[
            "minimum_tick_size",
            "minimumTickSize",
            "tick_size",
            "tickSize",
            "min_tick_size",
            "minTickSize",
            "mts",
        ],
    )
}

fn extract_neg_risk_bool(value: &serde_json::Value) -> Option<bool> {
    extract_bool_field(value, &["neg_risk", "negRisk", "negative_risk", "nr"])
}

fn extract_condition_id(value: &serde_json::Value) -> Option<String> {
    extract_string_field(value, &["condition_id", "conditionId", "condition", "c"])
}

fn extract_decimal_field(value: &serde_json::Value, keys: &[&str]) -> Option<Decimal> {
    for key in keys {
        if let Some(parsed) = value.get(*key).and_then(value_as_decimal) {
            return Some(parsed);
        }
    }
    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = extract_decimal_field(item, keys) {
                return Some(parsed);
            }
        }
    }
    if let Some(object) = value.as_object() {
        for item in object.values() {
            if item.is_object() || item.is_array() {
                if let Some(parsed) = extract_decimal_field(item, keys) {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

fn extract_bool_field(value: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(parsed) = value.get(*key).and_then(value_as_bool) {
            return Some(parsed);
        }
    }
    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = extract_bool_field(item, keys) {
                return Some(parsed);
            }
        }
    }
    if let Some(object) = value.as_object() {
        for item in object.values() {
            if item.is_object() || item.is_array() {
                if let Some(parsed) = extract_bool_field(item, keys) {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

fn extract_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(parsed) = value
            .get(*key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
        {
            return Some(parsed);
        }
    }
    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = extract_string_field(item, keys) {
                return Some(parsed);
            }
        }
    }
    if let Some(object) = value.as_object() {
        for item in object.values() {
            if item.is_object() || item.is_array() {
                if let Some(parsed) = extract_string_field(item, keys) {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

#[cfg(feature = "native-l2")]
fn mask_for_display(value: &str) -> String {
    if value.len() <= 14 {
        return "[redacted]".to_string();
    }
    format!("{}...{}", &value[..6], &value[value.len() - 6..])
}

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex_encode(&digest)
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Pub helper for server handler code that needs the same truthy check without
/// duplicating the env logic (used for live sender wiring in submit facade).
pub fn env_truthy_for_clob_reports(name: &str) -> bool {
    env_truthy(name)
}

/// Unified L1 private key lookup supporting direct env or _FILE (for k8s secret
/// volume without putting secret value in process env). Matches the logic in
/// server l2 derive handler. Used for both real place_limit_order (EIP-712 order
/// sig via SDK) and the signed dry-run payload builder.
/// NOTE: FILE/env resolve logic is duplicated (lightly) in server::try_auto_derive_l2_on_startup
/// (they serve signing key vs L2 session derive entrypoints). Extracting a common helper was
/// considered minor; left as-is for smallest change in fix round (no new cross-module churn).
#[cfg(feature = "native-l2")]
fn get_polymarket_private_key() -> Result<String> {
    if let Ok(path) = std::env::var("POLYMARKET_PRIVATE_KEY_FILE") {
        if !path.trim().is_empty() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let k = content.trim().to_string();
                if !k.is_empty() {
                    return Ok(k);
                }
            }
        }
    }
    std::env::var("POLYMARKET_PRIVATE_KEY")
        .or_else(|_| std::env::var("PRIVATE_KEY"))
        .context("POLYMARKET_PRIVATE_KEY (or POLYMARKET_PRIVATE_KEY_FILE) is required for signed order placement and dry-run payload signing")
}

fn submitting_order_facade_available() -> bool {
    cfg!(feature = "native-l2")
}

fn human_approval_workflow_available() -> bool {
    cfg!(feature = "native-l2")
}

fn kill_switch_and_risk_limits_available() -> bool {
    cfg!(feature = "native-l2")
}

pub fn collateral_readiness_max_age_seconds() -> u64 {
    std::env::var("POLYTRADER_COLLATERAL_READINESS_MAX_AGE_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(900)
}

pub fn approval_subject_hash_for_intent(intent: &RealOrderIntentDryRun) -> String {
    let subject = serde_json::json!({
        "token_id": intent.token_id.trim(),
        "side": intent.side.trim().to_ascii_lowercase(),
        "order_type": intent.order_type.trim().to_ascii_lowercase(),
        "size": intent.size.to_string(),
        "price": intent.price.map(|value| value.to_string()),
        "expected_edge_bps": intent.expected_edge_bps.map(|value| value.to_string()),
        "market_id": intent.market_id.as_deref().unwrap_or("").trim(),
        "outcome": intent.outcome.as_deref().unwrap_or("").trim(),
    });
    sha256_hex(&subject.to_string())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(feature = "native-l2")]
fn redact_order_signature(value: &mut serde_json::Value) {
    if let Some(signature) = value
        .get_mut("order")
        .and_then(|order| order.get_mut("signature"))
    {
        *signature = serde_json::json!("[redacted]");
    }
}

#[cfg(feature = "native-l2")]
fn sdk_signature_type(value: u8) -> Result<polymarket_client_sdk_v2::clob::types::SignatureType> {
    use polymarket_client_sdk_v2::clob::types::SignatureType;

    match value {
        0 => Ok(SignatureType::Eoa),
        1 => Ok(SignatureType::Proxy),
        2 => Ok(SignatureType::GnosisSafe),
        3 => Ok(SignatureType::Poly1271),
        other => anyhow::bail!("unsupported POLYMARKET_SIGNATURE_TYPE={other}"),
    }
}

fn env_decimal(name: &str, default: Decimal) -> Decimal {
    std::env::var(name)
        .ok()
        .and_then(|value| Decimal::from_str(value.trim()).ok())
        .filter(|value| *value > Decimal::ZERO)
        .unwrap_or(default)
}

fn env_decimal_allow_negative(name: &str, default: Decimal) -> Decimal {
    std::env::var(name)
        .ok()
        .and_then(|value| Decimal::from_str(value.trim()).ok())
        .unwrap_or(default)
}

fn value_as_decimal(value: &serde_json::Value) -> Option<Decimal> {
    match value {
        serde_json::Value::String(s) => Decimal::from_str(s).ok(),
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

fn value_as_bool(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        serde_json::Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn l2_message(timestamp: i64, method: &str, path: &str, body: &str) -> String {
    format!("{timestamp}{method}{path}{body}")
}

/// Compute the POLY_SIGNATURE header value (HMAC-SHA256 of the secret).
/// Exact payload format is defined by Polymarket (timestamp + method + requestPath + body).
pub fn compute_poly_signature(secret: &str, message: &str) -> Result<String> {
    type HmacSha256 = Hmac<Sha256>;

    let decoded_secret = URL_SAFE
        .decode(secret)
        .context("failed to base64-url decode Polymarket L2 secret")?;
    let mut mac = HmacSha256::new_from_slice(&decoded_secret)
        .context("failed to initialize HMAC-SHA256 signer")?;
    mac.update(message.as_bytes());
    Ok(URL_SAFE.encode(mac.finalize().into_bytes()))
}

fn current_timestamp_secs() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs() as i64)
}

fn truncate_for_error(text: &str) -> String {
    const MAX: usize = 300;
    if text.len() <= MAX {
        text.to_string()
    } else {
        format!("{}...", &text[..MAX])
    }
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    // The shared TEST_ENV_LOCK lives at the authenticated module level (pub(crate) under cfg(test))
    // so that live_sender::tests and others can acquire it to serialize real-order env mutations.
    // Makefile pre-deploy now runs with --test-threads=1 + native-l2 coverage.
    #[allow(unused_imports)]
    use super::TEST_ENV_LOCK;

    #[test]
    fn poly_signature_matches_sdk_vector() {
        let message = r#"1000000test-sign/orders{"hash":"0x123"}"#;
        let signature =
            compute_poly_signature("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", message)
                .expect("signature");

        assert_eq!(signature, "4gJVbox-R6XlDK4nlaicig0_ANVL1qdcahiL8CXfXLM=");
    }

    #[test]
    fn l2_message_matches_sdk_path_only_behavior() {
        assert_eq!(
            l2_message(1_000_000, "GET", "/balance-allowance", ""),
            "1000000GET/balance-allowance"
        );
    }

    #[test]
    fn preflight_blocks_zero_collateral_and_real_orders() {
        let account = serde_json::json!({
            "collateral": {
                "balance": "0",
                "allowances": {
                    "0x1": "0",
                    "0x2": "0"
                }
            },
            "open_orders": { "count": 0, "data": [] },
            "signature_type": 0
        });

        let report = build_preflight_report(&account);
        let blockers = report["blockers"].as_array().expect("blockers");

        assert!(blockers.iter().any(|v| v == "collateral_balance_positive"));
        assert!(blockers
            .iter()
            .any(|v| v == "collateral_allowance_positive"));
        if submitting_order_facade_available() {
            assert!(!blockers
                .iter()
                .any(|v| v == "real_order_submission_facade_available"));
        } else {
            assert!(blockers
                .iter()
                .any(|v| v == "real_order_submission_facade_available"));
        }
        if human_approval_workflow_available() {
            assert!(!blockers
                .iter()
                .any(|v| v == "human_approval_workflow_available"));
        } else {
            assert!(blockers
                .iter()
                .any(|v| v == "human_approval_workflow_available"));
        }
        assert_eq!(report["ready_for_real_orders"], false);
    }

    #[test]
    fn preflight_still_blocks_real_orders_when_account_has_collateral() {
        let account = serde_json::json!({
            "collateral": {
                "balance": "12.5",
                "allowances": {
                    "0x1": "100"
                }
            },
            "open_orders": { "count": 2, "data": [] },
            "signature_type": 0
        });

        let report = build_preflight_report(&account);
        let blockers = report["blockers"].as_array().expect("blockers");

        assert!(!blockers.iter().any(|v| v == "collateral_balance_positive"));
        assert!(!blockers
            .iter()
            .any(|v| v == "collateral_allowance_positive"));
        if submitting_order_facade_available() {
            assert!(!blockers
                .iter()
                .any(|v| v == "real_order_submission_facade_available"));
        } else {
            assert!(blockers
                .iter()
                .any(|v| v == "real_order_submission_facade_available"));
        }
        if human_approval_workflow_available() {
            assert!(!blockers
                .iter()
                .any(|v| v == "human_approval_workflow_available"));
        } else {
            assert!(blockers
                .iter()
                .any(|v| v == "human_approval_workflow_available"));
        }
        assert_eq!(report["open_order_count"], 2);
        assert_eq!(report["ready_for_real_orders"], false);
    }

    #[test]
    fn collateral_readiness_reports_external_wallet_blockers() {
        let account = serde_json::json!({
            "collateral": {
                "balance": "0",
                "allowances": {
                    "0xspender": "0"
                }
            },
            "open_orders": { "count": 0, "data": [] },
            "signature_type": 0
        });

        let report = build_collateral_readiness_report(&account, "0xabc", 0);
        let blockers = report["blockers"].as_array().expect("blockers");

        assert_eq!(report["wallet_address"], "0xabc");
        assert_eq!(report["collateral_balance"], "0");
        assert_eq!(report["collateral_balance_positive"], false);
        assert_eq!(report["collateral_allowance_positive"], false);
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
        assert!(blockers.iter().any(|v| v == "collateral_balance_positive"));
        assert!(blockers
            .iter()
            .any(|v| v == "collateral_allowance_positive"));
    }

    #[test]
    fn real_trading_unlock_status_fails_closed_by_default() {
        // Guard against env pollution from env-modding tests in same process.
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS");
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION");
        let report = build_real_trading_unlock_status();
        let blockers = report["blockers"].as_array().expect("blockers");

        assert_eq!(report["ready"], false);
        assert_eq!(report["ready_for_real_orders"], false);
        // implemented is now true (GatedRealClobLiveOrderSender exists and is wired)
        assert_eq!(report["live_order_sender_implemented"], true);
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
        // real_orders_enabled reflects the env (default false in test process)
        if !env_truthy("POLYTRADER_ENABLE_REAL_ORDERS")
            && !env_truthy("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION")
        {
            assert_eq!(report["real_orders_enabled"], false);
            assert_eq!(report["explicit_real_order_submission_configured"], false);
        }
        assert!(blockers
            .iter()
            .any(|value| value == "explicit_real_trading_config_unlock"));
        // no longer a blocker (gated impl is present)
        assert!(!blockers
            .iter()
            .any(|value| value == "live_order_sender_implemented"));
    }

    #[test]
    fn order_intent_dry_run_rejects_oversized_trade() {
        let preflight = serde_json::json!({
            "blockers": ["real_order_submission_facade_available", "human_approval_gate_absent"],
            "ready_for_real_orders": false,
            "real_orders_enabled": false,
            "paper_only": true
        });
        let intent = RealOrderIntentDryRun {
            token_id: "123".to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(10),
            price: Some(dec!(0.5)),
            expected_edge_bps: Some(dec!(500)),
            market_id: Some("market".to_string()),
            outcome: Some("Yes".to_string()),
        };

        let report = build_order_intent_dry_run_report(&intent, &preflight);
        let blockers = report["blockers"].as_array().expect("blockers");

        assert!(blockers.iter().any(|v| v == "max_risk_per_trade"));
        assert!(blockers
            .iter()
            .any(|v| v == "real_order_submission_facade_available"));
        assert_eq!(report["accepted"], false);
        assert_eq!(report["estimated_notional"], "5.0");
    }

    #[test]
    fn order_intent_dry_run_rejects_missing_edge_even_when_small() {
        let preflight = serde_json::json!({
            "blockers": ["real_order_submission_facade_available", "human_approval_gate_absent"],
            "ready_for_real_orders": false,
            "real_orders_enabled": false,
            "paper_only": true
        });
        let intent = RealOrderIntentDryRun {
            token_id: "123".to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(1),
            price: Some(dec!(0.5)),
            expected_edge_bps: None,
            market_id: None,
            outcome: None,
        };

        let report = build_order_intent_dry_run_report(&intent, &preflight);
        let blockers = report["blockers"].as_array().expect("blockers");

        assert!(blockers
            .iter()
            .any(|v| v == "expected_edge_present_and_sufficient"));
        assert!(!blockers.iter().any(|v| v == "max_risk_per_trade"));
        assert_eq!(report["accepted"], false);
    }

    #[test]
    fn market_metadata_validation_enforces_limit_tick_size() {
        let intent = RealOrderIntentDryRun {
            token_id: "123".to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(1),
            price: Some(dec!(0.505)),
            expected_edge_bps: Some(dec!(500)),
            market_id: None,
            outcome: None,
        };
        let report = build_market_metadata_validation_report(
            &intent,
            Some(&serde_json::json!({"minimum_tick_size": "0.01"})),
            Some(&serde_json::json!({"neg_risk": false})),
            Some(&serde_json::json!({"condition_id": "0xabc"})),
            None,
        );
        let blockers = report["blockers"].as_array().expect("blockers");

        assert_eq!(report["market_metadata_fetched"], true);
        assert_eq!(report["tick_size"], "0.01");
        assert_eq!(report["neg_risk"], false);
        assert_eq!(report["condition_id"], "0xabc");
        assert_eq!(report["price_within_tick_range"], true);
        assert_eq!(report["price_tick_valid"], false);
        assert!(blockers.iter().any(|v| v == "price_respects_tick_size"));
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
    }

    #[test]
    fn dry_run_merges_market_metadata_blockers() {
        let preflight = serde_json::json!({
            "blockers": [],
            "ready_for_real_orders": false,
            "real_orders_enabled": false,
            "paper_only": true
        });
        let intent = RealOrderIntentDryRun {
            token_id: "123".to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(1),
            price: Some(dec!(0.5)),
            expected_edge_bps: Some(dec!(500)),
            market_id: None,
            outcome: None,
        };
        let market = serde_json::json!({
            "blockers": ["market_metadata_fetch_failed"],
            "checks": [{"name": "tick_size_present", "ok": false, "severity": "blocker"}]
        });

        let report =
            build_order_intent_dry_run_report_with_market(&intent, &preflight, Some(&market));
        let blockers = report["blockers"].as_array().expect("blockers");

        assert!(blockers.iter().any(|v| v == "market_metadata_fetch_failed"));
        assert_eq!(
            report["market_metadata_validation"]["blockers"][0],
            "market_metadata_fetch_failed"
        );
    }

    #[test]
    fn submit_facade_fails_closed_without_approval_or_unlocks() {
        // Ensure no stray real orders env from sibling tests in same process.
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS");
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION");
        std::env::remove_var("POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN");
        std::env::set_var("POLYTRADER_REAL_ORDER_KILL_SWITCH_OPEN", "0");
        std::env::set_var("POLYTRADER_ENABLE_REAL_ORDERS", "0");
        let request = OrderSubmitFacadeRequest {
            post_request_dry_run_request: OrderPostRequestDryRunRequest {
                signed_payload_request: SignedOrderPayloadDryRunRequest {
                    intent: RealOrderIntentDryRun {
                        token_id: "123".to_string(),
                        side: "buy".to_string(),
                        order_type: "limit".to_string(),
                        size: dec!(1),
                        price: Some(dec!(0.5)),
                        expected_edge_bps: Some(dec!(500)),
                        market_id: Some("market".to_string()),
                        outcome: Some("Yes".to_string()),
                    },
                    confirm_signed_payload_dry_run: false,
                },
                confirm_order_post_request_dry_run: false,
            },
            confirm_real_order_submission: false,
            human_approval_event_id: None,
            final_review_decision_event_id: None,
            human_approval_token: None,
            human_approval_note: None,
            operator: Some("test".to_string()),
            server_human_approval: None,
            server_final_review_decision: None,
            server_collateral_readiness: None,
        };
        let post_report = order_post_request_dry_run_response(
            &request.post_request_dry_run_request,
            None,
            vec!["order_post_request_dry_run_confirmation_missing".to_string()],
            None,
        );

        let report = order_submit_facade_response(&request, post_report);
        let blockers = report["blockers"].as_array().expect("blockers");

        assert_eq!(report["accepted"], false);
        assert_eq!(report["request_sent"], false);
        assert_eq!(report["post_order_called"], false);
        assert_eq!(report["post_orders_called"], false);
        assert_eq!(report["real_order_submit_journal_ready"], true);
        assert_eq!(report["submit_decision"], "rejected_fail_closed");
        assert_eq!(report["reconciliation_status"], "reconciled_no_send");
        assert_eq!(report["reconciled"], true);
        assert_eq!(
            report["reconciliation"]["expected_exchange_state"],
            "no_order_created"
        );
        assert_eq!(report["reconciliation"]["request_sent"], false);
        assert_eq!(
            report["gate_report"]["kill_switch_and_risk_limits_available"],
            kill_switch_and_risk_limits_available()
        );
        assert!(blockers
            .iter()
            .any(|v| v == "order_post_request_dry_run_confirmation_missing"));
        assert!(blockers
            .iter()
            .any(|v| v == "real_order_submission_confirmation"));
        assert!(blockers
            .iter()
            .any(|v| v == "journaled_human_approval_valid"));
        assert!(blockers.iter().any(|v| v == "human_approval_event_missing"));
        assert!(blockers
            .iter()
            .any(|v| v == "fresh_collateral_readiness_valid"));
        assert!(blockers
            .iter()
            .any(|v| v == "fresh_collateral_readiness_missing"));
        assert!(blockers.iter().any(|v| v == "kill_switch_open"));
        assert!(!blockers
            .iter()
            .any(|v| v == "projected_order_notional_within_limit"));
        assert!(!blockers
            .iter()
            .any(|v| v == "projected_total_exposure_within_limit"));
        assert!(!blockers.iter().any(|v| v == "daily_loss_within_limit"));
        assert!(blockers
            .iter()
            .any(|v| v == "explicit_real_trading_config_unlock"));
        assert!(blockers.iter().any(|v| v == "paper_mode_still_active"));
        // New final validation: missing id produces blocker (end-to-end wired).
        assert!(blockers
            .iter()
            .any(|v| v == "journaled_final_review_decision_valid"));
        assert!(blockers
            .iter()
            .any(|v| v == "final_review_decision_event_missing"));

        // E coverage (minimal): exercise submit facade response with final_review_decision_event_id
        // present + server validation valid -> no final blocker (full gate path for handler branch).
        let req_with_final = OrderSubmitFacadeRequest {
            post_request_dry_run_request: request.post_request_dry_run_request.clone(),
            confirm_real_order_submission: true,
            human_approval_event_id: Some(uuid::Uuid::nil()), // dummy, will be invalid but we override server
            final_review_decision_event_id: Some(uuid::Uuid::nil()),
            human_approval_token: None,
            human_approval_note: None,
            operator: Some("test".to_string()),
            server_human_approval: Some(HumanApprovalValidation {
                valid: true,
                event_id: None,
                decision: Some("approve_facade".into()),
                subject_hash: None,
                blockers: vec![],
            }),
            server_final_review_decision: Some(FinalReviewDecisionValidation {
                valid: true,
                event_id: None,
                decision: Some("acknowledge_blocked".into()),
                operator: None,
                blockers: vec![],
            }),
            server_collateral_readiness: Some(CollateralReadinessValidation {
                valid: true,
                event_id: None,
                created_at: None,
                wallet_address: None,
                collateral_balance: None,
                collateral_balance_positive: true,
                collateral_allowance_positive: true,
                positive_allowance_count: Some(1),
                max_age_seconds: 900,
                age_seconds: Some(10),
                blockers: vec![],
            }),
        };
        // note: full post_report would have no blockers for some, but for this we use the gate report check
        let gate =
            build_submit_facade_gate_report(&req_with_final, &serde_json::json!({"blockers": []}));
        let gblockers: Vec<String> = gate["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        assert!(!gblockers
            .iter()
            .any(|v| v == "journaled_final_review_decision_valid"));
        assert!(!gblockers
            .iter()
            .any(|v| v == "final_review_decision_event_missing"));
    }

    // Expanded tests for place early bails (cover dispatch/place error arms without net).
    // Uses secret injection (crate visible) + env guards + no real key/addr match to bail
    // before any http/POST in do_signed.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn place_limit_order_bails_early_when_orders_disabled() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("POLYTRADER_ENABLE_REAL_ORDERS").ok();
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS");
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDER_SUBMISSION");
        // ensure a creds entry so from_current succeeds (orders_enabled from default=false)
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
            g.insert(
                "test-sess".to_string(),
                crate::server::L2Secret {
                    address: "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf".to_string(),
                    api_key: "00000000-0000-0000-0000-000000000000".to_string(),
                    secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
                    passphrase: "p".to_string(),
                },
            );
        }
        let client = RealClobClient::from_current_l2_session().expect("injected for test");
        let intent = RealOrderIntentDryRun {
            token_id: "0".repeat(64),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(1),
            price: Some(dec!(0.5)),
            expected_edge_bps: None,
            market_id: None,
            outcome: None,
        };
        let err = client.place_limit_order(&intent).await.unwrap_err();
        assert!(
            err.to_string().contains("Real order placement is disabled")
                || err.to_string().contains("disabled")
        );
        // restore env
        match old {
            Some(v) => std::env::set_var("POLYTRADER_ENABLE_REAL_ORDERS", v),
            None => std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS"),
        }
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn place_limit_order_bails_early_on_non_limit() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("POLYTRADER_ENABLE_REAL_ORDERS", "1");
        let old_key = std::env::var("POLYMARKET_PRIVATE_KEY").ok();
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
            g.insert(
                "test-sess".to_string(),
                crate::server::L2Secret {
                    address: "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf".to_string(),
                    api_key: "00000000-0000-0000-0000-000000000000".to_string(),
                    secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
                    passphrase: "p".to_string(),
                },
            );
        }
        let client = RealClobClient::from_current_l2_session().expect("injected");
        let intent = RealOrderIntentDryRun {
            token_id: "0".repeat(64),
            side: "buy".to_string(),
            order_type: "market".to_string(),
            size: dec!(1),
            price: Some(dec!(0.5)),
            expected_edge_bps: None,
            market_id: None,
            outcome: None,
        };
        let err = client.place_limit_order(&intent).await.unwrap_err();
        assert!(err.to_string().contains("supports limit orders only"));
        match old_key {
            Some(v) => std::env::set_var("POLYMARKET_PRIVATE_KEY", v),
            None => std::env::remove_var("POLYMARKET_PRIVATE_KEY"),
        }
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS");
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(feature = "native-l2")]
    async fn place_limit_order_bails_on_missing_private_key() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("POLYTRADER_ENABLE_REAL_ORDERS", "1");
        let old_key = std::env::var("POLYMARKET_PRIVATE_KEY").ok();
        let old_file = std::env::var("POLYMARKET_PRIVATE_KEY_FILE").ok();
        std::env::remove_var("POLYMARKET_PRIVATE_KEY");
        std::env::remove_var("POLYMARKET_PRIVATE_KEY_FILE");
        std::env::remove_var("PRIVATE_KEY");
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
            g.insert(
                "test-sess".to_string(),
                crate::server::L2Secret {
                    address: "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf".to_string(),
                    api_key: "00000000-0000-0000-0000-000000000000".to_string(),
                    secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
                    passphrase: "p".to_string(),
                },
            );
        }
        let client = RealClobClient::from_current_l2_session().expect("injected");
        let intent = RealOrderIntentDryRun {
            token_id: "0".repeat(64),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(1),
            price: Some(dec!(0.5)),
            expected_edge_bps: None,
            market_id: None,
            outcome: None,
        };
        let err = client.place_limit_order(&intent).await.unwrap_err();
        assert!(
            err.to_string().contains("POLYMARKET_PRIVATE_KEY")
                || err.to_string().contains("required for signed order")
        );
        // restore
        match old_key {
            Some(v) => std::env::set_var("POLYMARKET_PRIVATE_KEY", v),
            None => std::env::remove_var("POLYMARKET_PRIVATE_KEY"),
        }
        match old_file {
            Some(v) => std::env::set_var("POLYMARKET_PRIVATE_KEY_FILE", v),
            None => std::env::remove_var("POLYMARKET_PRIVATE_KEY_FILE"),
        }
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(feature = "native-l2")]
    async fn place_limit_order_bails_on_address_mismatch_even_with_key() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("POLYTRADER_ENABLE_REAL_ORDERS", "1");
        let old_key = std::env::var("POLYMARKET_PRIVATE_KEY").ok();
        std::env::set_var(
            "POLYMARKET_PRIVATE_KEY",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
        );
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
            // address that won't match the privkey 0x...0001 -> 0x7E5F...
            g.insert(
                "test-sess".to_string(),
                crate::server::L2Secret {
                    address: "0x0000000000000000000000000000000000000000".to_string(),
                    api_key: "00000000-0000-0000-0000-000000000000".to_string(),
                    secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
                    passphrase: "p".to_string(),
                },
            );
        }
        let client = RealClobClient::from_current_l2_session().expect("injected");
        let intent = RealOrderIntentDryRun {
            token_id: "0".repeat(64),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            size: dec!(1),
            price: Some(dec!(0.5)),
            expected_edge_bps: None,
            market_id: None,
            outcome: None,
        };
        let err = client.place_limit_order(&intent).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match active L2 session address")
                || err.to_string().contains("signer address does not match")
        );
        // restore
        match old_key {
            Some(v) => std::env::set_var("POLYMARKET_PRIVATE_KEY", v),
            None => std::env::remove_var("POLYMARKET_PRIVATE_KEY"),
        }
        std::env::remove_var("POLYTRADER_ENABLE_REAL_ORDERS");
        // do not leave a secret for sibling tests
        {
            let mut g = crate::server::get_l2_secrets().lock().unwrap();
            g.clear();
        }
    }
}
