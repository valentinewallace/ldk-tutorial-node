use base64;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use serde_json;
use std::sync::Mutex;
use tokio::runtime::Runtime;

pub struct BitcoindClient {
    bitcoind_rpc_client: Mutex<RpcClient>,
    host: String,
    port: u16,
    rpc_user: String,
    rpc_password: String,
    runtime: Mutex<Runtime>,
}

impl BitcoindClient {
    pub fn new(host: String, port: u16, rpc_user: String, rpc_password: String) ->
        std::io::Result<Self>
    {
        let http_endpoint = HttpEndpoint::for_host(host.clone()).with_port(port);
        let rpc_credentials = base64::encode(format!("{}:{}", rpc_user.clone(), rpc_password.clone()));
        let bitcoind_rpc_client = RpcClient::new(&rpc_credentials, http_endpoint)?;
        let client = Self {
            bitcoind_rpc_client: Mutex::new(bitcoind_rpc_client),
            host,
            port,
            rpc_user,
            rpc_password,
            runtime: Mutex::new(Runtime::new().unwrap()),
        };
        Ok(client)
    }

    pub fn get_new_rpc_client(&self) -> std::io::Result<RpcClient> {
        let http_endpoint = HttpEndpoint::for_host(self.host.clone()).with_port(self.port);
        let rpc_credentials = base64::encode(format!("{}:{}", self.rpc_user.clone(), self.rpc_password.clone()));
        RpcClient::new(&rpc_credentials, http_endpoint)
    }
}

impl FeeEstimator for BitcoindClient {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        let runtime = self.runtime.lock().unwrap();
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();
        match confirmation_target {
            ConfirmationTarget::Background => {
                let conf_target = serde_json::json!(144);
                let estimate_mode = serde_json::json!("ECONOMICAL");
                let resp = runtime.block_on(rpc.call_method::<serde_json::Value>("estimatesmartfee", &vec![conf_target, estimate_mode])).unwrap();
                if !resp["errors"].is_null() && resp["errors"].as_array().unwrap().len() > 0 {
                    return 253
                }
                resp["feerate"].as_u64().unwrap() as u32
            },
            ConfirmationTarget::Normal => {
                let conf_target = serde_json::json!(18);
                let estimate_mode = serde_json::json!("ECONOMICAL");
                let resp = runtime.block_on(rpc.call_method::<serde_json::Value>("estimatesmartfee", &vec![conf_target, estimate_mode])).unwrap();
                if !resp["errors"].is_null() && resp["errors"].as_array().unwrap().len() > 0 {
                    return 253
                }
                resp["feerate"].as_u64().unwrap() as u32
            },
            ConfirmationTarget::HighPriority => {
                let conf_target = serde_json::json!(6);
                let estimate_mode = serde_json::json!("CONSERVATIVE");
                let resp = runtime.block_on(rpc.call_method::<serde_json::Value>("estimatesmartfee", &vec![conf_target, estimate_mode])).unwrap();
                if !resp["errors"].is_null() && resp["errors"].as_array().unwrap().len() > 0 {
                    return 253
                }
                resp["feerate"].as_u64().unwrap() as u32
            },
        }
    }
}

impl BroadcasterInterface for BitcoindClient {
	  fn broadcast_transaction(&self, tx: &Transaction) {
        let mut rpc = self.bitcoind_rpc_client.lock().unwrap();
        let runtime = self.runtime.lock().unwrap();
        let tx_serialized = serde_json::json!(encode::serialize_hex(tx));
        runtime.block_on(rpc.call_method::<serde_json::Value>("sendrawtransaction", &vec![tx_serialized])).unwrap();
    }
}
