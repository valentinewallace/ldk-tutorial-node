mod bitcoind_client;
mod cli;
mod utils;

use background_processor::BackgroundProcessor;
use bitcoin::{BlockHash, Txid};
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::consensus::encode;
use bitcoin::hashes::Hash;
use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::util::address::Address;
use bitcoin_bech32::WitnessProgram;
use crate::bitcoind_client::BitcoindClient;
use lightning::chain;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::chainmonitor::ChainMonitor;
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::Filter;
use lightning::chain::keysinterface::{InMemorySigner, KeysInterface, KeysManager};
use lightning::chain::transaction::OutPoint;
use lightning::chain::Watch;
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{ChannelManagerReadArgs, PaymentHash, PaymentPreimage,
                                    SimpleArcChannelManager};
use lightning::ln::peer_handler::{MessageHandler, SimpleArcPeerManager};
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, EventsProvider};
use lightning::util::logger::{Logger, Record};
use lightning::util::ser::{ReadableArgs, Writer};
use lightning_block_sync::UnboundedCache;
use lightning_block_sync::SpvClient;
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::poll::{ChainTip, Poll};
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use rand::{thread_rng, Rng};
use lightning::routing::network_graph::NetGraphMsgHandler;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};
use time::OffsetDateTime;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

pub(crate) const NETWORK: Network = Network::Regtest;

pub(crate) struct FilesystemLogger{}
impl Logger for FilesystemLogger {
	  fn log(&self, record: &Record) {
		    let raw_log = record.args.to_string();
			  let log = format!("{} {:<5} [{}:{}] {}\n", OffsetDateTime::now_utc().format("%F %T"),
                  record.level.to_string(), record.module_path, record.line, raw_log);
        fs::create_dir_all("logs").unwrap();
        fs::OpenOptions::new().create(true).append(true).open("./logs/logs.txt").unwrap()
            .write_all(log.as_bytes()).unwrap();
	  }
}

fn read_channelmonitors_from_disk(path: String, keys_manager: Arc<KeysManager>) ->
    Result<HashMap<OutPoint, (Option<BlockHash>, ChannelMonitor<InMemorySigner>)>, std::io::Error>
{
    if !Path::new(&path).exists() {
        return Ok(HashMap::new())
    }
    let mut outpoint_to_channelmonitor = HashMap::new();
    for file_option in fs::read_dir(path).unwrap() {
        let file = file_option.unwrap();
        let owned_file_name = file.file_name();
        let filename = owned_file_name.to_str();
        if !filename.is_some() || !filename.unwrap().is_ascii() || filename.unwrap().len() < 65 {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid ChannelMonitor file name"));
        }

        let txid = Txid::from_hex(filename.unwrap().split_at(64).0);
        if txid.is_err() {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid tx ID in filename"));
        }

        let index = filename.unwrap().split_at(65).1.split('.').next().unwrap().parse();
        if index.is_err() {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid tx index in filename"));
        }

        let contents = fs::read(&file.path())?;

        if let Ok((blockhash, channel_monitor)) =
            <(Option<BlockHash>, ChannelMonitor<InMemorySigner>)>::read(&mut Cursor::new(&contents),
                                                        &*keys_manager)
        {
                outpoint_to_channelmonitor.insert(OutPoint { txid: txid.unwrap(), index: index.unwrap() },
                                                  (blockhash, channel_monitor));
            } else {
                return Err(std::io::Error::new(std::io::ErrorKind::Other,
                                           "Failed to deserialize ChannelMonitor"));
            }
    }
    Ok(outpoint_to_channelmonitor)
}

pub(crate) enum HTLCDirection {
    Inbound,
    Outbound
}

pub(crate) enum HTLCStatus {
    Pending,
    Succeeded,
    Failed,
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, (Option<PaymentPreimage>, HTLCDirection, HTLCStatus)>>>;

type ArcChainMonitor = ChainMonitor<InMemorySigner, Arc<dyn Filter>, Arc<BitcoindClient>,
Arc<BitcoindClient>, Arc<FilesystemLogger>, Arc<FilesystemPersister>>;

pub(crate) type PeerManager = SimpleArcPeerManager<SocketDescriptor, ArcChainMonitor, BitcoindClient,
BitcoindClient, dyn chain::Access, FilesystemLogger>;

pub(crate) type ChannelManager = SimpleArcChannelManager<ArcChainMonitor, BitcoindClient, BitcoindClient,
FilesystemLogger>;


fn handle_ldk_events(peer_manager: Arc<PeerManager>, channel_manager: Arc<ChannelManager>,
                     chain_monitor: Arc<ArcChainMonitor>, bitcoind_client: Arc<BitcoindClient>,
                     keys_manager: Arc<KeysManager>, payment_storage: PaymentInfoStorage)
{
    let runtime = Runtime::new().unwrap();
    let mut rpc = bitcoind_client.get_new_rpc_client().unwrap();
    let mut pending_txs: HashMap<OutPoint, Transaction> = HashMap::new();
    loop {
        peer_manager.process_events();
        let loop_channel_manager = channel_manager.clone();
        let mut events = channel_manager.get_and_clear_pending_events();
		    events.append(&mut chain_monitor.get_and_clear_pending_events());
        for event in events {
			      match event {
				        Event::FundingGenerationReady { temporary_channel_id, channel_value_satoshis,
                                                output_script, .. } => {
					          let addr = WitnessProgram::from_scriptpubkey(&output_script[..], match NETWORK {
							          Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
							          Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
							          Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
							          Network::Signet => panic!("Signet unsupported"),
						        }
					          ).expect("Lightning funding tx should always be to a SegWit output").to_address();
                    let mut output = serde_json::Map::new();
                    output.insert(addr, serde_json::json!(channel_value_satoshis as f64 / 1_000_000_00.0));
                    let outputs = serde_json::json!(vec![output]);
                    let tx_hex: serde_json::Value = runtime.block_on(rpc.call_method::<serde_json::Value>("createrawtransaction", &vec![serde_json::json!([]), outputs])).unwrap();
                    let funded_tx: serde_json::Value = runtime.block_on(rpc.call_method("fundrawtransaction", &[tx_hex])).unwrap();
                    let change_output_position = funded_tx["changepos"].as_i64().unwrap();
							      assert!(change_output_position == 0 || change_output_position == 1);
							      let funded_tx = funded_tx["hex"].as_str().unwrap();
                    let signed_tx: serde_json::Value = runtime.block_on(rpc.call_method("signrawtransactionwithwallet",
                                                                       &vec![serde_json::json!(funded_tx)])).unwrap();
								    assert_eq!(signed_tx["complete"].as_bool().unwrap(), true);
                    let final_tx: Transaction = encode::deserialize(&utils::hex_to_vec(&signed_tx["hex"].as_str().unwrap()).unwrap()).unwrap();
								    let outpoint = OutPoint {
                        txid: final_tx.txid(),
                        index: if change_output_position == 0 { 1 } else { 0 }
                    };
                    loop_channel_manager.funding_transaction_generated(&temporary_channel_id,
                                                                       outpoint);
                    pending_txs.insert(outpoint, final_tx);
				        },
				        Event::FundingBroadcastSafe { funding_txo, .. } => {
                    let funding_tx = pending_txs.remove(&funding_txo).unwrap();
                    bitcoind_client.broadcast_transaction(&funding_tx);
                    println!("\nEVENT: broadcasted funding transaction");
				        },
				        Event::PaymentReceived { payment_hash, payment_secret, amt: amt_msat } => {
                    let mut payments = payment_storage.lock().unwrap();
                    if let Some((Some(preimage), _, _)) = payments.get(&payment_hash) {
						            assert!(loop_channel_manager.claim_funds(preimage.clone(), &payment_secret,
                                                                 amt_msat));
                        println!("\nEVENT: received payment from payment_hash {} of {} satoshis",
                                 utils::hex_str(&payment_hash.0), amt_msat / 1000);
                        let (_, _, ref mut status) = payments.get_mut(&payment_hash).unwrap();
                        *status = HTLCStatus::Succeeded;
                    } else {
                        println!("\nERROR: we received a payment but didn't know the preimage");
                        loop_channel_manager.fail_htlc_backwards(&payment_hash, &payment_secret);
                        payments.insert(payment_hash, (None, HTLCDirection::Inbound, HTLCStatus::Failed));
                    }
				        },
				        Event::PaymentSent { payment_preimage } => {
                    let hashed = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());
                    let mut payments = payment_storage.lock().unwrap();
                    for (payment_hash, (preimage_option, _, status)) in payments.iter_mut() {
                        if *payment_hash == hashed {
                            *preimage_option = Some(payment_preimage);
                            *status = HTLCStatus::Succeeded;
                            println!("\nNEW EVENT: successfully sent payment from payment hash\
                                         {:?} with preimage {:?}", utils::hex_str(&payment_hash.0),
                                     utils::hex_str(&payment_preimage.0));
                        }
                    }
				        },
				        Event::PaymentFailed { payment_hash, rejected_by_dest } => {
                    print!("\nNEW EVENT: Failed to send payment to payment hash {:?}:",
                           utils::hex_str(&payment_hash.0));
                    if rejected_by_dest {
                        println!("rejected by destination node");
                    } else {
                        println!("route failed");
                    }

                    let mut payments = payment_storage.lock().unwrap();
                    if payments.contains_key(&payment_hash) {
                        let (_, _, ref mut status) = payments.get_mut(&payment_hash).unwrap();
                        *status = HTLCStatus::Failed;
                    }
				        },
				        Event::PendingHTLCsForwardable { .. } => {
                    loop_channel_manager.process_pending_htlc_forwards();
				        },
                Event::SpendableOutputs { outputs } => {
                    let addr_args = vec![serde_json::json!("LDK output address")];
                    let destination_address_str: serde_json::Value = runtime.block_on(rpc.call_method("getnewaddress", &addr_args)).unwrap();
                    let destination_address = Address::from_str(destination_address_str.as_str().unwrap()).unwrap();
                    let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
                    let tx_feerate = bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
                    let spending_tx = keys_manager.spend_spendable_outputs(output_descriptors,
                                                                           Vec::new(),
                                                                           destination_address.script_pubkey(),
                                                                           tx_feerate, &Secp256k1::new()).unwrap();
                    bitcoind_client.broadcast_transaction(&spending_tx);
                    // XXX maybe need to rescan and blah? but contrary to what matt's saying, it
                    // looks like spend_spendable's got us covered
                }
            }
        }
        thread::sleep(Duration::new(1, 0));
    }
}

// #[tokio::main]
// async fn main() {
fn main() {
    let bitcoind_host = "127.0.0.1".to_string();
    let bitcoind_port = 18443;
    let rpc_user = "polaruser".to_string();
    let rpc_password = "polarpass".to_string();
    // XXX make names less ambiguous between bitcoind_client and bitcoind_rpc_client
    let bitcoind_client = Arc::new(BitcoindClient::new(bitcoind_host.clone(), bitcoind_port,
                                                       rpc_user.clone(), rpc_password.clone()).unwrap());
    let mut bitcoind_rpc_client = bitcoind_client.get_new_rpc_client().unwrap();

    // ## Setup
    // Step 1: Initialize the FeeEstimator
    let fee_estimator = bitcoind_client.clone();

    // Step 2: Initialize the Logger
    let logger = Arc::new(FilesystemLogger{});

    // Step 3: Initialize the BroadcasterInterface
    let broadcaster = bitcoind_client.clone();

    // Step 4: Initialize Persist
    let persister = Arc::new(FilesystemPersister::new(".".to_string()));

    // Step 5: Initialize the ChainMonitor
    let chain_monitor: Arc<ArcChainMonitor> = Arc::new(ChainMonitor::new(None, broadcaster.clone(),
                                                           logger.clone(), fee_estimator.clone(),
                                                           persister.clone()));

    // Step 6: Initialize the KeysManager
	  let node_privkey = if let Ok(seed) = fs::read("./key_seed") { // the private key that corresponds
		    assert_eq!(seed.len(), 32);                               // to our lightning node's pubkey
		    let mut key = [0; 32];
		    key.copy_from_slice(&seed);
		    key
	  } else {
		    let mut key = [0; 32];
		    thread_rng().fill_bytes(&mut key);
		    let mut f = File::create("./key_seed").unwrap();
		    f.write_all(&key).expect("Failed to write seed to disk");
		    f.sync_all().expect("Failed to sync seed to disk");
		    key
	  };
	  let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let keys_manager = Arc::new(KeysManager::new(&node_privkey, cur.as_secs(), cur.subsec_nanos()));

    // Step 7: Read ChannelMonitor state from disk
    let mut outpoint_to_channelmonitor = read_channelmonitors_from_disk("./monitors".to_string(),
                                                                    keys_manager.clone()).unwrap();

    // Step 9: Read ChannelManager state from disk
    let user_config = UserConfig::default();
    let mut channel_manager: ChannelManager;
    let mut channel_manager_last_blockhash: Option<BlockHash> = None;
    let mut runtime = Runtime::new().unwrap();
    if let Ok(mut f) = fs::File::open("./manager") {
        let (last_block_hash_option, channel_manager_from_disk) = {
            let mut channel_monitor_mut_references = Vec::new();
            for (_, channel_monitor) in outpoint_to_channelmonitor.iter_mut() {
                channel_monitor_mut_references.push(&mut channel_monitor.1);
            }
            let read_args = ChannelManagerReadArgs::new(keys_manager.clone(), fee_estimator.clone(),
                                                        chain_monitor.clone(), broadcaster.clone(),
                                                        logger.clone(), user_config,
                                                        channel_monitor_mut_references);
            <(Option<BlockHash>, ChannelManager)>::read(&mut f, read_args).unwrap()
        };
        channel_manager = channel_manager_from_disk;
        channel_manager_last_blockhash = last_block_hash_option;
    } else {
        // let mut bitcoind_rpc_client = bitcoind_client.bitcoind_rpc_client.lock().unwrap();
        // let current_chain_height: usize = bitcoind_rpc_client
        //     .call_method::<serde_json::Value>("getblockchaininfo", &vec![]).await.unwrap()["blocks"].as_u64().unwrap() as usize;
        let current_chain_height: usize = runtime.block_on(bitcoind_rpc_client
                                                           .call_method::<serde_json::Value>("getblockchaininfo", &vec![])).unwrap()["blocks"].as_u64().unwrap() as usize;
        channel_manager = channelmanager::ChannelManager::new(Network::Regtest, fee_estimator.clone(),
                                                       chain_monitor.clone(), broadcaster.clone(),
                                                       logger.clone(), keys_manager.clone(),
                                                       user_config, current_chain_height);
    }

    // Step 10: Sync ChannelMonitors to chain tip if restarting
    let mut chain_tip = None;
    let mut chain_listener_channel_monitors = Vec::new();
    let mut cache = UnboundedCache::new();
    // let rpc_credentials = base64::encode(format!("{}:{}", rpc_user, rpc_password));
    // let mut block_source = RpcClient::new(&rpc_credentials, HttpEndpoint::for_host(bitcoind_host)
    //                                       .with_port(bitcoind_port)).unwrap();
    if outpoint_to_channelmonitor.len() > 0 {
        for (outpoint, blockhash_and_monitor) in outpoint_to_channelmonitor.drain() {
            let blockhash_option = blockhash_and_monitor.0;
            let channel_monitor = blockhash_and_monitor.1;
            if let Some(blockhash) = blockhash_option {
                chain_listener_channel_monitors.push((blockhash, (channel_monitor,
                                                                  broadcaster.clone(), fee_estimator.clone(),
                                                                  logger.clone()), outpoint));
            }
        }

        let mut chain_listeners = Vec::new();
        for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
            chain_listeners.push((monitor_listener_info.0,
                                  &mut monitor_listener_info.1 as &mut dyn chain::Listen));
        }
        // Because `sync_listeners` is an async function and we want to run it synchronously,
        // we run it in a tokio Runtime.
        chain_tip = Some(runtime.block_on(init::synchronize_listeners(&mut bitcoind_rpc_client, Network::Regtest,
                                                               &mut cache, chain_listeners)).unwrap());
        // chain_tip = Some(init::synchronize_listeners(&mut bitcoind_rpc_client, Network::Regtest,
        //                                       &mut cache, chain_listeners).await.unwrap());
    }

    // Step 11: Give ChannelMonitors to ChainMonitor
    if chain_listener_channel_monitors.len() > 0 {
        for item in chain_listener_channel_monitors.drain(..) {
            let channel_monitor = item.1.0;
            let funding_outpoint = item.2;
            chain_monitor.watch_channel(funding_outpoint, channel_monitor).unwrap();
        }
    }

    // Step 12: Sync ChannelManager to chain tip if restarting
    if let Some(channel_manager_blockhash) = channel_manager_last_blockhash {
        let chain_listener = vec![
            (channel_manager_blockhash, &mut channel_manager as &mut dyn chain::Listen)];
        // chain_tip = Some(init::synchronize_listeners(&mut block_source, Network::Regtest,
        //                                                        &mut cache, chain_listener).await.unwrap());
        chain_tip = Some(runtime.block_on(init::synchronize_listeners(&mut bitcoind_rpc_client, Network::Regtest,
                                                               &mut cache, chain_listener)).unwrap());
    }

    // Step 13: Optional: Initialize the NetGraphMsgHandler
    // XXX persist routing data
    let genesis = genesis_block(Network::Regtest).header.block_hash();
    let router = Arc::new(NetGraphMsgHandler::new(genesis, None::<Arc<dyn chain::Access>>, logger.clone()));

    // Step 14: Initialize the PeerManager
    let channel_manager = Arc::new(channel_manager);
	  let mut ephemeral_bytes = [0; 32];
	  rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
    let lightning_msg_handler = MessageHandler { chan_handler: channel_manager.clone(),
                                                 route_handler: router.clone() };
    let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(lightning_msg_handler,
                                                        keys_manager.get_node_secret(),
                                                        &ephemeral_bytes, logger.clone()));

    // ## Running LDK
    // Step 15: Initialize LDK Event Handling
    let (event_ntfn_sender, mut event_ntfn_receiver) = mpsc::channel(2);
    let peer_manager_event_listener = peer_manager.clone();
    let channel_manager_event_listener = channel_manager.clone();
    let chain_monitor_event_listener = chain_monitor.clone();
    let keys_manager_listener = keys_manager.clone();
    let payment_info: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
    let payment_info_for_events = payment_info.clone();
    thread::spawn(move || {
        handle_ldk_events(peer_manager_event_listener, channel_manager_event_listener,
                          chain_monitor_event_listener, bitcoind_client.clone(),
                          keys_manager_listener, payment_info_for_events);
    });

    // Step 16: Initialize Peer Connection Handling
    let peer_manager_connection_handler = peer_manager.clone();
    let event_notifier = event_ntfn_sender.clone();
    runtime.spawn(async move {
	      let listener = std::net::TcpListener::bind("0.0.0.0:9735").unwrap();
        loop {
            let tcp_stream = listener.accept().unwrap().0;
            lightning_net_tokio::setup_inbound(peer_manager_connection_handler.clone(),
                                               event_notifier.clone(), tcp_stream).await;
        }
    });

    // Step 17: Connect and Disconnect Blocks
    if chain_tip.is_none() {
        // chain_tip = Some(init::validate_best_block_header(&mut block_source).await.unwrap());
        chain_tip = Some(runtime.block_on(init::validate_best_block_header(&mut bitcoind_rpc_client)).unwrap());
    }
    let channel_manager_listener = channel_manager.clone();
    let chain_monitor_listener = chain_monitor.clone();
    runtime.spawn(async move {
        let chain_poller = poll::ChainPoller::new(&mut bitcoind_rpc_client, Network::Regtest);
        let chain_listener = (chain_monitor_listener, channel_manager_listener);
        let mut spv_client = SpvClient::new(chain_tip.unwrap(), chain_poller, &mut cache,
                                            &chain_listener);
        loop {
            spv_client.poll_best_tip().await.unwrap();
            thread::sleep(Duration::new(1, 0));
        }
    });

    // Step 17 & 18: Initialize ChannelManager persistence & Once Per Minute: ChannelManager's
    // timer_chan_freshness_every_min() and PeerManager's timer_tick_occurred
    let persist_channel_manager_callback = move |node: &ChannelManager| {
        FilesystemPersister::persist_manager("./".to_string(), &*node)
    };
    BackgroundProcessor::start(persist_channel_manager_callback, channel_manager.clone(),
    logger.clone());
    let peer_manager_processor = peer_manager.clone();
    runtime.spawn(async move {
        loop {
            peer_manager_processor.timer_tick_occured();
            thread::sleep(Duration::new(60, 0));
        }
    });
    cli::poll_for_user_input(peer_manager.clone(), channel_manager.clone(), router.clone(),
                             payment_info, keys_manager.get_node_secret(), event_ntfn_sender,
                             logger.clone(), runtime);
}
