import requests
import json
import base58
from solathon.core.instructions import transfer
from solathon import Client, Transaction, PublicKey, Keypair


url = "https://atlas-txn-sender-qqmv5cpcjq-an.a.run.app"
headers = {"Content-Type": "application/json"}

config = {
    "skipPreflight": True,
	"maxRetries": 5
}



client = Client("https://api.mainnet-beta.solana.com")
recent_blockhash = client.get_recent_blockhash().blockhash

with open('../account1.json', 'r') as f:
    priv_key = json.load(f)
    sender = Keypair.from_private_key(priv_key)

# Specify recipient's public key and amount to transfer (0.01 SOL)
receiver = PublicKey("xjtCtLnxnAFUFAwLFvr4zery2JSbyWhGy4SZeDUeDXt")
amount = 0.01 * 1e9  # Convert SOL to lamports

# Create transfer transaction
instruction = transfer(
    from_public_key=sender.public_key,
    to_public_key=receiver,
    lamports=int(amount)
)


transaction = Transaction(instructions=[instruction], signers=[sender], recent_blockhash=recent_blockhash)
transaction.sign()

serialized_txn = transaction.serialize()
encoded_txn = base58.b58encode(serialized_txn).decode('utf-8')

data = {
    "jsonrpc": "2.0",
    "method": "sendTransaction",
    "params": [
        encoded_txn,
        config
    ],
    "id": 1
}
print('='*10)
print('Sending transaction to %s.' % url)
print(data)


response = requests.post(url, data=json.dumps(data), headers=headers)
print(response.json())


###
# 
# https://docs.rs/solana-client/latest/solana_client/rpc_client/struct.RpcClient.html#method.send_transaction_with_config
