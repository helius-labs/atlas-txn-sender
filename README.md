## Atlas Txn Sender

This package uses the min required dependencies to send transactions to Solana leaders.

**Note:** This service does not handle preflight checks, and also doesn't validate blockhashes before sending to leader

The service has the following envs:

`RPC_URL` - RPC url used to fetch next leaders with `getSlotLeaders`

`GRPC_URL` - Yellowstone GRPC Geyser url used to stream latest slots and blocks. Slots tell us what to call `getSlotLeaders` with, blocks tell us if the txns we've sent were sent successfully.

`X_TOKEN` - token used to authenticate with the grpc url

`TPU_CONNECTION_POOL_SIZE` - Number of leaders to cache connections to, and send transactions to. The default in the solana client is 4.

`NUM_LEADERS` - Number of leaders to send transactions to

`LEADER_OFFSET` - Offset of the leader schedule. Default is 0. 

`IDENTITY_KEYPAIR_FILE` - Path to the keypair file. If this is a validator key it will use a staked connection to connect to leaders.

`PORT` - Port to run the service on. Default is 4040.

### Install Dependencies

`sudo apt-get install libssl-dev libudev-dev pkg-config zlib1g-dev llvm clang cmake make libprotobuf-dev protobuf-compiler`

### Running

set the above envs and install dependencies, then run `cargo run --release`. 

### Deploying service with ansible

Deploying the service with ansible will setup a systemd service with haproxy so that you can access the service over port 80.
It will also install datadog for metrics.

First you need to install the datadog role with the following command

```
ansible-galaxy install datadog.datadog
```

Then you need to update the file `ansible/inventory/hosts.yml` with the name/ip address/user of the server you want to deploy to.

Then you need to set these in the `ansible/deploy_atlas_txn_sender.yml` file

```
rpc_url
grpc_url
x_token
datadog_api_key
datadog_site
```

Then you can run the following command to deploy the service

```
ansible-playbook -i ansible/inventory/hosts.yml ansible/deploy_atlas_txn_sender.yml
```