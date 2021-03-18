To install: clone this repository
```
git clone git@github.com:valentinewallace/ldk-tutorial-node.git
```

To run: 
```
cd ldk-tutorial-node
cargo run <bitcoind-rpc-username>:<bitcoind-rpc-password>@<bitcoind-rpc-host>:<bitcoind-rpc-port> <ldk_storage_directory_path> [<ldk-incoming-peer-listening-port>] [bitcoin-network]
```
where bitcoin-network defaults to `testnet`, with possible options being `testnet` or `regtest`.
