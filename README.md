## Atlas Txn Sender

This package uses the min required dependencies to send transactions to Solana leaders.

The service has the following envs:

`RPC_URL` - RPC url used to fetch next leaders with `getSlotLeaders`

`GRPC_URL` - GRPC url used to stream latest slots and blocks. Slots tell us what to call `getSlotLeaders` with, blocks tell us if the txns we've sent were sent successfully.

`X_TOKEN` - token used to authenticate with the grpc url

`TPU_CONNECTION_POOL_SIZE` (Optional)- Number of leaders to cache connections to, and send transactions to. The default in the solana client is 4.

`IDENTITY_KEYPAIR_FILE` - Path to the keypair file. If this is a validator key it will use a staked connection to the validator.

`PORT` - Port to run the service on. Default is 4040.

To run this service locally, all you need to do is clone it, set the envs above, and run

`cargo run`