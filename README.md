To install:
```
git clone git@github.com:valentinewallace/ldk-tutorial-node.git
```

To run: 
```
cd ldk-tutorial-node
cargo run <bitcoind-rpc-username>:<bitcoind-rpc-password>@<bitcoind-rpc-host>:<bitcoind-rpc-port> <ldk_storage_directory_path> [<ldk-peer-listening-port>] [bitcoin-network]
```
`bitcoin-network`: defaults to `testnet`. Options: `testnet`, `regtest`.
`ldk-peer-listening-port`: defaults to 9735.
