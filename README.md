## Atlas Txn Sender

This package uses the min required dependencies to send transactions to Solana leaders.

**Note:** This service does not handle preflight checks, and also doesn't validate blockhashes before sending to leader

The service has the following envs:

`RPC_URL` - RPC url used to fetch next leaders with `getSlotLeaders`

`GRPC_URL` - Yellowstone GRPC Geyser url used to stream latest slots and blocks. Slots tell us what to call `getSlotLeaders` with, blocks tell us if the txns we've sent were sent successfully.

`X_TOKEN` - token used to authenticate with the grpc url

`TPU_CONNECTION_POOL_SIZE` - Number of leaders to cache connections to, and send transactions to. The default in the solana client is 4.

`NUM_LEADERS` - Number of leaders to send transactions to

`IDENTITY_KEYPAIR_FILE` - Path to the keypair file. If this is a validator key it will use a staked connection to connect to leaders.

`PORT` - Port to run the service on. Default is 4040.

### Install Dependencies

`sudo apt-get install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler`

### Running

set the above envs and install dependencies, then run `cargo run --release`. 

## Systemd Setup

And example systemd unit file for this service is below (filling in <rpc-url>, <grpc-url>, <x-token>, <path-to-repo>, <user>). Copy this into `/etc/systemd/system/atlas-txn-sender.service``

```
[Unit]
Description=atlas-txn-sender
After=network.target

[Service]
Environment=RPC_URL=<rpc-url>
Environment=GRPC_URL=<grpc-url>
Environment=X_TOKEN=<x-token>
Environment=TPU_CONNECTION_POOL_SIZE=4
Environment=NUM_LEADERS=8
ExecStart=<path-to-repo>/atlas-txn-sender/target/release/atlas_txn_sender
User=<user>
Restart=on-failure
LimitNOFILE=1000000

[Install]
WantedBy=multi-user.target
```

Then run 

```
sudo systemctl daemon-reload
sudo systemctl enable atlas-txn-sender.service
sudo systemctl restart atlas-txn-sender.service
```

### Setting up Datadog

Update `/etc/datadog-agent/datadog.yaml` with (filling in <dd-api-key>)

```
site: us5.datadoghq.com


api_key: <dd-api-key>

dogstatsd_port: 7998
logs_enabled: true
tags:
- staked:false
- env:prod
- service:atlas_txn_sender
- network:mainnet
- region:pitt
use_dogstatsd: true
```

Update `/etc/datadog-agent/conf.d/journald.d/conf.yaml` with 

```
logs:
    - type: journald
      include_units:
          - atlas-txn-sender.service
```

Then run 

```
sudo usermod -a -G systemd-journal dd-agent
sudo systemctl restart datadog-agent
```