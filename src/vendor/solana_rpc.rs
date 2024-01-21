// code copied out of solana repo because of version conflicts

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use bincode::Options;
use jsonrpsee::core::RpcResult;
use solana_sdk::{bs58, packet::PACKET_DATA_SIZE};
use solana_transaction_status::TransactionBinaryEncoding;

use crate::errors::invalid_request;

const MAX_BASE58_SIZE: usize = 1683; // Golden, bump if PACKET_DATA_SIZE changes
const MAX_BASE64_SIZE: usize = 1644; // Golden, bump if PACKET_DATA_SIZE changes
pub fn decode_and_deserialize<T>(
    encoded: String,
    encoding: TransactionBinaryEncoding,
) -> RpcResult<(Vec<u8>, T)>
where
    T: serde::de::DeserializeOwned,
{
    let wire_output = match encoding {
        TransactionBinaryEncoding::Base58 => {
            if encoded.len() > MAX_BASE58_SIZE {
                return Err(invalid_request("base58 encoded too large"));
            }
            bs58::decode(encoded)
                .into_vec()
                .map_err(|e| invalid_request(format!("invalid base58 encoding: {e:?}").as_str()))?
        }
        TransactionBinaryEncoding::Base64 => {
            if encoded.len() > MAX_BASE64_SIZE {
                return Err(invalid_request("base64 encoded too large"));
            }
            BASE64_STANDARD
                .decode(encoded)
                .map_err(|e| invalid_request(&format!("invalid base64 encoding: {e:?}")))?
        }
    };
    if wire_output.len() > PACKET_DATA_SIZE {
        return Err(invalid_request("decoded too large"));
    }
    bincode::options()
        .with_limit(PACKET_DATA_SIZE as u64)
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .deserialize_from(&wire_output[..])
        .map_err(|err| invalid_request(&format!("failed to deserialize: {}", &err.to_string())))
        .map(|output| (wire_output, output))
}
