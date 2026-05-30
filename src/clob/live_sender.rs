//! Fail-closed boundary for any future live Polymarket order sender.
//!
//! RISK: This module intentionally does not know how to submit, cancel, fund,
//! refresh allowances, or mutate exchange state. It exists so the codebase can
//! name the future live-order boundary and test that the current implementation
//! rejects before any network dispatch.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Minimal already-reviewed order packet shape for a future sender boundary.
///
/// The current implementation only uses this for fail-closed tests/status. A
/// real sender must not be added here until every pre-submit guard is revalidated
/// immediately before network dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveOrderSendRequest {
    pub local_order_id: String,
    pub order_intent_event_id: String,
    pub signed_payload_event_id: String,
    pub human_approval_event_id: String,
    pub final_review_decision_event_id: String,
    pub market_id: String,
    pub token_id: String,
    pub side: String,
    pub order_type: String,
    pub size: Decimal,
    pub price: Decimal,
}

/// Conservative result for a live-sender boundary evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveOrderSendResult {
    pub sender_name: String,
    pub accepted_for_network_dispatch: bool,
    pub submit_decision: String,
    pub rejection_reason: String,
    pub exchange_order_id: Option<String>,
    pub request_sent: bool,
    pub would_send: bool,
    pub post_order_called: bool,
    pub post_orders_called: bool,
    pub real_orders_enabled: bool,
    pub ready_for_real_orders: bool,
}

/// Boundary trait for a future live sender.
///
/// RISK: The trait is synchronous today because the only implementation rejects
/// locally. A future network implementation must add explicit async/network
/// review rather than silently changing this no-send behavior.
pub trait LiveOrderSender {
    fn name(&self) -> &'static str;
    fn send(&self, request: &LiveOrderSendRequest) -> LiveOrderSendResult;
}

/// The only live sender implementation currently allowed in the codebase.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailClosedLiveOrderSender;

impl LiveOrderSender for FailClosedLiveOrderSender {
    fn name(&self) -> &'static str {
        "FailClosedLiveOrderSender"
    }

    fn send(&self, _request: &LiveOrderSendRequest) -> LiveOrderSendResult {
        LiveOrderSendResult {
            sender_name: self.name().to_string(),
            accepted_for_network_dispatch: false,
            submit_decision: "rejected_before_network".to_string(),
            rejection_reason: "live_sender_fail_closed_boundary_only".to_string(),
            exchange_order_id: None,
            request_sent: false,
            would_send: false,
            post_order_called: false,
            post_orders_called: false,
            real_orders_enabled: false,
            ready_for_real_orders: false,
        }
    }
}

pub fn sample_boundary_request() -> LiveOrderSendRequest {
    LiveOrderSendRequest {
        local_order_id: "boundary-status-local-order".to_string(),
        order_intent_event_id: "00000000-0000-0000-0000-000000000000".to_string(),
        signed_payload_event_id: "00000000-0000-0000-0000-000000000000".to_string(),
        human_approval_event_id: "00000000-0000-0000-0000-000000000000".to_string(),
        final_review_decision_event_id: "00000000-0000-0000-0000-000000000000".to_string(),
        market_id: "boundary-status-market".to_string(),
        token_id: "boundary-status-token".to_string(),
        side: "buy".to_string(),
        order_type: "limit".to_string(),
        size: Decimal::ONE,
        price: Decimal::new(50, 2),
    }
}

/// Build a redacted status packet proving the boundary exists and fails closed.
pub fn build_live_sender_boundary_status() -> serde_json::Value {
    let sender = FailClosedLiveOrderSender;
    let request = sample_boundary_request();
    let result = sender.send(&request);

    serde_json::json!({
        "boundary_name": "LiveOrderSender",
        "implementation_name": result.sender_name,
        "trait_defined": true,
        "fail_closed_implementation_present": true,
        "network_sender_present": false,
        "implementation_permitted": false,
        "paper_only": true,
        "real_orders_enabled": result.real_orders_enabled,
        "ready_for_real_orders": result.ready_for_real_orders,
        "ready_for_live_sender_implementation": false,
        "accepted_for_network_dispatch": result.accepted_for_network_dispatch,
        "submit_decision": result.submit_decision,
        "rejection_reason": result.rejection_reason,
        "exchange_order_id": result.exchange_order_id,
        "request_sent": result.request_sent,
        "would_send": result.would_send,
        "post_order_called": result.post_order_called,
        "post_orders_called": result.post_orders_called,
        "required_next_step": "Keep this as the only implementation until the design review is accepted and every external unlock, collateral, allowance, kill-switch, risk, paper-mode, and human-review gate is deliberate.",
        "note": "Fail-closed live-sender boundary only. It cannot submit, cancel, fund, refresh allowances, mutate balances, or place real orders."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_closed_sender_rejects_before_network() {
        let sender = FailClosedLiveOrderSender;
        let result = sender.send(&sample_boundary_request());

        assert_eq!(result.sender_name, "FailClosedLiveOrderSender");
        assert!(!result.accepted_for_network_dispatch);
        assert_eq!(result.submit_decision, "rejected_before_network");
        assert_eq!(
            result.rejection_reason,
            "live_sender_fail_closed_boundary_only"
        );
        assert_eq!(result.exchange_order_id, None);
        assert!(!result.request_sent);
        assert!(!result.would_send);
        assert!(!result.post_order_called);
        assert!(!result.post_orders_called);
        assert!(!result.real_orders_enabled);
        assert!(!result.ready_for_real_orders);
    }

    #[test]
    fn boundary_status_reports_no_network_sender() {
        let status = build_live_sender_boundary_status();

        assert_eq!(status["boundary_name"], "LiveOrderSender");
        assert_eq!(status["implementation_name"], "FailClosedLiveOrderSender");
        assert_eq!(status["trait_defined"], true);
        assert_eq!(status["fail_closed_implementation_present"], true);
        assert_eq!(status["network_sender_present"], false);
        assert_eq!(status["implementation_permitted"], false);
        assert_eq!(status["request_sent"], false);
        assert_eq!(status["post_order_called"], false);
        assert_eq!(status["post_orders_called"], false);
    }
}
