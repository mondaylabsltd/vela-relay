use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};

use vela_relay_core::account::{
    AccountStatus, account_status, entry_point_nonce_calldata, normalize_address,
};

use crate::{
    app::{AppState, rpc::SUPPORTED_ENTRY_POINTS},
    utils::rpc,
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountInfo {
    chain_id: u64,
    entry_point: &'static str,
    safe_address: String,
    settlement_recipient: String,
    onchain_balance: String,
    spendable_balance: String,
    latest_nonce: u64,
    pending_nonce: u64,
    status: &'static str,
    rpc_used: String,
}

pub async fn handle(
    State(state): State<AppState>,
    Path((chain_id, safe_address)): Path<(u64, String)>,
    headers: HeaderMap,
) -> Response {
    let safe_address = match normalize_address(&safe_address) {
        Some(address) => address,
        None => return error(StatusCode::BAD_REQUEST, "invalid safeAddress"),
    };
    let settlement_recipient = match state.settlement_recipient() {
        Some(address) => address.to_owned(),
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "settlement recipient is not configured",
            );
        }
    };
    let entry_point = SUPPORTED_ENTRY_POINTS[0];
    let user_rpc_url = headers.get(rpc::USER_RPC_URL_HEADER);

    let (vault_balance, latest_nonce, pending_nonce) = tokio::join!(
        rpc::call(
            chain_id,
            user_rpc_url,
            "eth_getBalance",
            json!([settlement_recipient.as_str(), "latest"]),
        ),
        rpc::call(
            chain_id,
            user_rpc_url,
            "eth_call",
            entry_point_nonce_params(entry_point, &safe_address, "latest"),
        ),
        rpc::call(
            chain_id,
            user_rpc_url,
            "eth_call",
            entry_point_nonce_params(entry_point, &safe_address, "pending"),
        ),
    );

    let vault_balance = match vault_balance {
        Ok(result) => result,
        Err(()) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "account RPC is unavailable",
            );
        }
    };
    let vault_rpc_url = vault_balance.rpc_url.clone();
    let latest_nonce = match latest_nonce.and_then(|result| parse_nonce(&result.value)) {
        Ok(nonce) => nonce,
        Err(()) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "account RPC returned an invalid nonce",
            );
        }
    };
    let pending_nonce = match pending_nonce.and_then(|result| parse_nonce(&result.value)) {
        Ok(nonce) => nonce,
        Err(()) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "account RPC returned an invalid nonce",
            );
        }
    };
    let vault_balance = match parse_quantity(&vault_balance.value) {
        Ok(balance) => balance,
        Err(()) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "account RPC returned an invalid balance",
            );
        }
    };
    let status = account_status(&vault_balance, latest_nonce, pending_nonce).as_str();
    (
        StatusCode::OK,
        Json(AccountInfo {
            chain_id,
            entry_point,
            safe_address,
            settlement_recipient,
            onchain_balance: vault_balance.clone(),
            spendable_balance: vault_balance,
            latest_nonce,
            pending_nonce,
            status,
            rpc_used: vault_rpc_url,
        }),
    )
        .into_response()
}

/// The JSON hops only; the grammar and the verdict are the core's
/// (`vela_relay_core::account`), shared with the Cloudflare shell so a wallet
/// gets the same answer from either deployment.
fn parse_quantity(value: &Value) -> Result<String, ()> {
    vela_relay_core::treasury::parse_quantity(value.as_str().ok_or(())?)
}

fn parse_nonce(value: &Value) -> Result<u64, ()> {
    vela_relay_core::account::parse_nonce(value.as_str().ok_or(())?)
}

fn entry_point_nonce_params(entry_point: &str, safe_address: &str, block_tag: &str) -> Value {
    let data = entry_point_nonce_calldata(safe_address).expect("Safe address is normalized");
    json!([{ "to": entry_point, "data": data }, block_tag])
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        AccountInfo, account_status, entry_point_nonce_params, parse_nonce, parse_quantity,
    };
    use vela_relay_core::account::AccountStatus;

    #[test]
    fn parses_rpc_quantities_without_losing_the_hex_response_shape() {
        assert_eq!(parse_quantity(&json!("0x000F")), Ok("0x000f".into()));
        assert_eq!(parse_nonce(&json!("0x2c")), Ok(44));
        assert!(parse_quantity(&json!("44")).is_err());
    }

    #[test]
    fn requests_the_safe_user_operation_nonce_from_the_entry_point() {
        assert_eq!(
            entry_point_nonce_params(
                "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
                "0x14fb1fb21751e29f7ec48dc450017552e3d1ea5c",
                "latest",
            ),
            json!([
                {
                    "to": "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
                    "data": "0x35567e1a00000000000000000000000014fb1fb21751e29f7ec48dc450017552e3d1ea5c0000000000000000000000000000000000000000000000000000000000000000"
                },
                "latest"
            ])
        );
    }

    #[test]
    fn derives_status_from_balance_and_transaction_nonce_state() {
        assert!(matches!(
            account_status("0x1", 44, 44),
            AccountStatus::Active
        ));
        assert!(matches!(
            account_status("0x0", 44, 44),
            AccountStatus::InsufficientBalance
        ));
        assert!(matches!(
            account_status("0x1", 44, 45),
            AccountStatus::LockedPendingUnknown
        ));
    }

    #[test]
    fn returns_the_settlement_vault_balance_in_account_balance_fields() {
        let response = serde_json::to_value(AccountInfo {
            chain_id: 1,
            entry_point: "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
            safe_address: "0x0000000000000000000000000000000000000001".into(),
            settlement_recipient: "0x0000000000000000000000000000000000000002".into(),
            onchain_balance: "0x2a".into(),
            spendable_balance: "0x2a".into(),
            latest_nonce: 0,
            pending_nonce: 0,
            status: AccountStatus::Active.as_str(),
            rpc_used: "https://rpc.example".into(),
        })
        .unwrap();

        assert_eq!(response["onchainBalance"], json!("0x2a"));
        assert_eq!(response["spendableBalance"], json!("0x2a"));
    }
}
