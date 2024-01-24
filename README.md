## Atlas Txn Sender

This package uses the min required dependencies to send transactions to Solana leaders.

The service has the following envs:

`RPC_URL` - RPC url used to fetch next leaders with `getSlotLeaders`
`GRPC_URL` - GRPC url used to stream latest slots and blocks. Slots tell us what to call `getSlotLeaders` with, blocks tell us if the txns we've sent were sent successfully.
`TPU_CONNECTION_POOL_SIZE` - Number of leaders to cache connections to, and send transactions to. The default in the solana client is 4.

To run this service locally, all you need to do is clone it, set the envs above, and run

`cargo run`