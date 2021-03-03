use bitcoin::network::constants::Network;
use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::secp256k1::key::{PublicKey, SecretKey};
use crate::{ChannelManager, FilesystemLogger, HTLCDirection, HTLCStatus, NETWORK,
            PaymentInfoStorage, PeerManager};
use crate::utils;
use lightning::chain;
use lightning::ln::channelmanager::{PaymentHash, PaymentPreimage};
use lightning::routing::network_graph::NetGraphMsgHandler;
use lightning::routing::router;
use rand;
use rand::Rng;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::io::{BufRead, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

pub(crate) fn poll_for_user_input(peer_manager: Arc<PeerManager>, channel_manager:
                           Arc<ChannelManager>, router: Arc<NetGraphMsgHandler<Arc<dyn
                           chain::Access>, Arc<FilesystemLogger>>>, payment_storage:
                           PaymentInfoStorage, node_privkey: SecretKey, event_notifier:
                           mpsc::Sender<()>, logger: Arc<FilesystemLogger>, mut runtime: Runtime) {
    let path = Path::new("./channel_peer_data");
    match read_channel_peer_data_from_disk(path) {
        Ok(mut info) => {
            for (pubkey, peer_addr) in info.drain() {
                runtime = connect_to_peer_if_necessary(pubkey, peer_addr, peer_manager.clone(), event_notifier.clone(), runtime).0;
            }
        },
        Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
    }
    println!("LDK startup successful. To view available commands: \"help\".\nLDK logs are available in the `logs` folder of the current directory.");
    let stdin = io::stdin();
    print!("> "); io::stdout().flush().unwrap(); // Without flushing, the `>` doesn't print
    for line in stdin.lock().lines() {
        let _ = event_notifier.try_send(());
        let line = line.unwrap();
        let mut words = line.split_whitespace();
        if let Some(word) = words.next() {
            match word {
                "help" => handle_help(),
                "openchannel" => {
                    let peer_pubkey_and_ip_addr = words.next();
                    let channel_value_sat = words.next();
                    if peer_pubkey_and_ip_addr.is_none() || channel_value_sat.is_none() {
                        println!("ERROR: openchannel takes 2 arguments: `openchannel pubkey@host:port channel_amt_satoshis`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let peer_pubkey_and_ip_addr = peer_pubkey_and_ip_addr.unwrap();
                    let (pubkey, peer_addr) = match parse_peer_info(peer_pubkey_and_ip_addr.to_string()) {
                        Ok(info) => info,
                        Err(e) => {
                            println!("{:?}", e.into_inner().unwrap());
                            print!("> "); io::stdout().flush().unwrap(); continue;
                        }
                    };

                    let chan_amt_sat: Result<u64, _> = channel_value_sat.unwrap().parse();
                    if chan_amt_sat.is_err() {
                        println!("ERROR: channel amount must be a number");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    let (runtime_returned, connect_result) = connect_to_peer_if_necessary(pubkey, peer_addr, peer_manager.clone(),
                                                           event_notifier.clone(), runtime);
                    // connect_to_peer_if_necessary needs to return the runtime back to us so we can reuse
                    // it for future commands.
                    runtime = runtime_returned;
                    if connect_result.is_err() {
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    if handle_open_channel(pubkey, chan_amt_sat.unwrap(),
                                           channel_manager.clone()).is_ok() {
                        persist_channel_peer(path, peer_pubkey_and_ip_addr);
                    }
                },
                "sendpayment" => {
                    let invoice_str = words.next();
                    if invoice_str.is_none() {
                        println!("ERROR: sendpayment requires an invoice: `sendpayment <invoice>`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    let invoice_res = lightning_invoice::Invoice::from_str(invoice_str.unwrap());
                    if invoice_res.is_err() {
                        println!("ERROR: invalid invoice: {:?}", invoice_res.unwrap_err());
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let invoice = invoice_res.unwrap();

                    // get_route needs msat
                    let amt_pico_btc = invoice.amount_pico_btc();
                    if amt_pico_btc.is_none () {
                        println!("ERROR: invalid invoice: must contain amount to pay");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let amt_msat = amt_pico_btc.unwrap() / 10;

                    let payee_pubkey = invoice.recover_payee_pub_key();
                    let final_cltv = *invoice.min_final_cltv_expiry().unwrap_or(&9) as u32;

										let mut payment_hash = PaymentHash([0; 32]);
										payment_hash.0.copy_from_slice(&invoice.payment_hash()[..]);
                    handle_send_payment(payee_pubkey, amt_msat, final_cltv, payment_hash,
                                        invoice.routes(), router.clone(), channel_manager.clone(),
                                        payment_storage.clone(), logger.clone());
                },
                "getinvoice" => {
                    let amt_str = words.next();
                    if amt_str.is_none() {
                        println!("ERROR: getinvoice requires an amount in satoshis");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }

                    let amt_sat: Result<u64, _> = amt_str.unwrap().parse();
                    if amt_sat.is_err() {
                        println!("ERROR: getinvoice provided payment amount was not a number");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    handle_get_invoice(amt_sat.unwrap(), payment_storage.clone(), node_privkey.clone());
                },
                "connectpeer" => {
                    let peer_pubkey_and_ip_addr = words.next();
                    if peer_pubkey_and_ip_addr.is_none() {
                        println!("ERROR: connectpeer requires peer connection info: `connectpeer pubkey@host:port`");
                        print!("> "); io::stdout().flush().unwrap(); continue;
                    }
                    let (pubkey, peer_addr) = match parse_peer_info(peer_pubkey_and_ip_addr.unwrap().to_string()) {
                        Ok(info) => info,
                        Err(e) => {
                            println!("{:?}", e.into_inner().unwrap());
                            print!("> "); io::stdout().flush().unwrap(); continue;
                        }
                    };
                    let (runtime_returned, connect_result) = connect_to_peer_if_necessary(pubkey, peer_addr, peer_manager.clone(),
                                                                                          event_notifier.clone(), runtime);
                    runtime = runtime_returned;
                    if connect_result.is_ok() {
                        println!("SUCCESS: connected to peer {}", pubkey);
                    }

                },
                "listchannels" => handle_list_channels(channel_manager.clone()),
                _ => println!("Unknown command. See `\"help\" for available commands.")
            }
        }
        print!("> "); io::stdout().flush().unwrap();

    }
}

// async fn handle_help() {
fn handle_help() {

}

fn handle_list_channels(channel_manager: Arc<ChannelManager>) {
    print!("[");
    for chan_info in channel_manager.list_channels() {
        println!("");
        println!("\t{{");
        println!("\t\tchannel_id: {},", utils::hex_str(&chan_info.channel_id[..]));
        println!("\t\tpeer_pubkey: {},", utils::hex_str(&chan_info.remote_network_id.serialize()));
        let mut pending_channel = false;
        match chan_info.short_channel_id {
            Some(id) => println!("\t\tshort_channel_id: {},", id),
            None => {
                pending_channel = true;
            }
        }
        println!("\t\tpending_open: {},", pending_channel);
        println!("\t\tchannel_value_satoshis: {},", chan_info.channel_value_satoshis);
        println!("\t\tchannel_can_send_payments: {},", chan_info.is_live);
        println!("\t}},");
    }
    println!("]");
}

fn connect_to_peer_if_necessary(pubkey: PublicKey, peer_addr: SocketAddr, peer_manager:
                                Arc<PeerManager>, event_notifier: mpsc::Sender<()>, mut runtime: Runtime) -> (Runtime, Result<(), ()>) {
    for node_pubkey in peer_manager.get_peer_node_ids() {
        if node_pubkey == pubkey {
            return (runtime, Ok(()))
        }
    }
    match TcpStream::connect_timeout(&peer_addr, Duration::from_secs(10)) {
        Ok(stream) => {
            let peer_mgr = peer_manager.clone();
            let event_ntfns = event_notifier.clone();
            runtime.spawn(async move {
                lightning_net_tokio::setup_outbound(peer_mgr, event_ntfns,
                                                    pubkey, stream).await;
            });
            let mut peer_connected = false;
            while !peer_connected {
                for node_pubkey in peer_manager.get_peer_node_ids() {
                    if node_pubkey == pubkey { peer_connected = true; }
                }
            }
        },
        Err(e) => {
            println!("ERROR: failed to connect to peer: {:?}", e);
            return (runtime, Err(()))
        }
    }
    (runtime, Ok(()))
}

// async fn handle_open_channel(peer_pubkey: PublicKey, peer_socket: SocketAddr,
fn handle_open_channel(peer_pubkey: PublicKey, channel_amt_sat: u64, channel_manager:
                       Arc<ChannelManager>) -> Result<(), ()> {
    match channel_manager.create_channel(peer_pubkey, channel_amt_sat, 0, 0, None) {
        Ok(_) => {
            println!("SUCCESS: channel created! Mine some blocks to open it.");
            return Ok(())
        },
        Err(e) => {
            println!("ERROR: failed to open channel: {:?}", e);
            return Err(())
        }
    }
}

fn handle_send_payment(payee: PublicKey, amt_msat: u64, final_cltv: u32, payment_hash: PaymentHash,
                       routes: Vec<&lightning_invoice::Route>, router:
                       Arc<NetGraphMsgHandler<Arc<dyn chain::Access>, Arc<FilesystemLogger>>>,
                       channel_manager: Arc<ChannelManager>, payment_storage: PaymentInfoStorage,
                       logger: Arc<FilesystemLogger>) {
    let graph = router.network_graph.read().unwrap();
    let first_hops = channel_manager.list_usable_channels();
    let payer_pubkey = channel_manager.get_our_node_id();
    // XXX: test w/ no first_hops
    let route = router::get_route(&payer_pubkey, &graph, &payee, Some(&first_hops.iter().collect::<Vec<_>>()), &vec![], amt_msat,
                                 final_cltv, logger);
    if let Err(e) = route {
        println!("ERROR: failed to find route: {}", e.err);
        return
    }
    let status = match channel_manager.send_payment(&route.unwrap(), payment_hash, &None) {
        Ok(()) => {
            println!("SUCCESS: initiated sending {} msats to {}", amt_msat, payee);
            HTLCStatus::Pending
        },
        Err(e) => {
            println!("ERROR: failed to send payment: {:?}", e);
            HTLCStatus::Failed
        }
    };
    let mut payments = payment_storage.lock().unwrap();
    payments.insert(payment_hash, (None, HTLCDirection::Outbound, status));
}

fn handle_get_invoice(amt_sat: u64, payment_storage: PaymentInfoStorage, payee_node_privkey: SecretKey) {
    let mut payments = payment_storage.lock().unwrap();
    let secp_ctx = Secp256k1::new();

    let mut preimage = [0; 32];
		rand::thread_rng().fill_bytes(&mut preimage);
		let payment_hash = Sha256Hash::hash(&preimage);

    let payee_node_pubkey = PublicKey::from_secret_key(&secp_ctx, &payee_node_privkey);

		let invoice_res = lightning_invoice::InvoiceBuilder::new(match NETWORK {
				Network::Bitcoin => lightning_invoice::Currency::Bitcoin,
				Network::Testnet => lightning_invoice::Currency::BitcoinTestnet,
				Network::Regtest => lightning_invoice::Currency::Regtest,
        Network::Signet => panic!("Signet invoices not supported")
		}).payment_hash(payment_hash).description("rust-lightning-bitcoinrpc invoice".to_string())
				.amount_pico_btc(amt_sat * 10)
				.current_timestamp()
        .payee_pub_key(payee_node_pubkey)
				.build_signed(|msg_hash| {
						secp_ctx.sign_recoverable(msg_hash, &payee_node_privkey)
				});
		match invoice_res {
				Ok(invoice) => println!("SUCCESS: generated invoice: {}", invoice),
				Err(e) => println!("ERROR: failed to create invoice: {:?}", e),
		}

    payments.insert(PaymentHash(payment_hash.into_inner()), (Some(PaymentPreimage(preimage)),
                                                             HTLCDirection::Inbound,
                                                             HTLCStatus::Pending));
}

fn persist_channel_peer(path: &Path, peer_info: &str) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{}\n", peer_info).as_bytes())
}

fn read_channel_peer_data_from_disk(path: &Path) -> Result<HashMap<PublicKey, SocketAddr>, std::io::Error> {
    let mut peer_data = HashMap::new();
    if !Path::new(&path).exists() {
        return Ok(HashMap::new())
    }
    let file = File::open(path)?;
    let reader = io::BufReader::new(file);
    for line in reader.lines() {
        match parse_peer_info(line.unwrap()) {
            Ok((pubkey, socket_addr)) => {
                peer_data.insert(pubkey, socket_addr);
            },
            Err(e) => return Err(e)
        }
    }
    Ok(peer_data)
}

fn parse_peer_info(peer_pubkey_and_ip_addr: String) -> Result<(PublicKey, SocketAddr), std::io::Error> {
    let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
    let pubkey = pubkey_and_addr.next();
    let peer_addr_str = pubkey_and_addr.next();
    if peer_addr_str.is_none() || peer_addr_str.is_none() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "ERROR: incorrectly formatted peer
        info. Should be formatted as: `pubkey@host:port`"));
    }

    let peer_addr: Result<SocketAddr, _> = peer_addr_str.unwrap().parse();
    if peer_addr.is_err() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "ERROR: couldn't parse pubkey@host:port into a socket address"));
    }

    let pubkey = utils::hex_to_compressed_pubkey(pubkey.unwrap());
    if pubkey.is_none() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "ERROR: unable to parse given pubkey for node"));
    }

    Ok((pubkey.unwrap(), peer_addr.unwrap()))
}
