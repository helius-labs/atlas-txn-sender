use std::error::Error;

use jsonrpsee::types::{error::INVALID_PARAMS_CODE, ErrorObjectOwned};

pub fn invalid_request(reason: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        INVALID_PARAMS_CODE,
        format!("Invalid Request: {reason}"),
        None::<String>,
    )
}

#[derive(Debug)]
pub enum AtlasTxnSenderError {
    Custom(String),
    NoStartSlot,
}

impl Error for AtlasTxnSenderError {}

impl std::fmt::Display for AtlasTxnSenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AtlasTxnSenderError::Custom(msg) => write!(f, "{}", msg),
            AtlasTxnSenderError::NoStartSlot => {
                write!(f, "Slot has not yet arrived from gRPC")
            }
        }
    }
}

impl From<String> for AtlasTxnSenderError {
    fn from(msg: String) -> Self {
        AtlasTxnSenderError::Custom(msg)
    }
}
