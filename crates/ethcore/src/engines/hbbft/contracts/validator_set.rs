use client::{
    traits::{EngineClient, TransactionRequest},
    BlockChainClient,
};
use crypto::publickey::Public;
use engines::hbbft::utils::bound_contract::{BoundContract, CallError};
use ethereum_types::{Address, U256};
use std::{collections::BTreeMap, str::FromStr};
use types::{ids::BlockId, transaction::Error};

use_contract!(
    validator_set_hbbft,
    "res/contracts/validator_set_hbbft.json"
);

lazy_static! {
    static ref VALIDATOR_SET_ADDRESS: Address =
        Address::from_str("1000000000000000000000000000000000000001").unwrap();
}

macro_rules! call_const_validator {
	($c:ident, $x:ident $(, $a:expr )*) => {
		$c.call_const(validator_set_hbbft::functions::$x::call($($a),*))
	};
}

pub enum ValidatorType {
    Current,
    Pending,
}

pub fn get_validator_pubkeys(
    client: &dyn EngineClient,
    block_id: BlockId,
    validator_type: ValidatorType,
) -> Result<BTreeMap<Address, Public>, CallError> {
    let c = BoundContract::bind(client, block_id, *VALIDATOR_SET_ADDRESS);
    let validators = match validator_type {
        ValidatorType::Current => call_const_validator!(c, get_validators)?,
        ValidatorType::Pending => call_const_validator!(c, get_pending_validators)?,
    };
    let mut validator_map = BTreeMap::new();
    for v in validators {
        let pubkey = call_const_validator!(c, get_public_key, v)?;

        if pubkey.len() != 64 {
            return Err(CallError::ReturnValueInvalid);
        }
        let pubkey = Public::from_slice(&pubkey);

        //println!("Validator {:?} with public key {}", v, pubkey);
        validator_map.insert(v, pubkey);
    }
    Ok(validator_map)
}

#[cfg(test)]
pub fn mining_by_staking_address(
    client: &dyn EngineClient,
    staking_address: &Address,
) -> Result<Address, CallError> {
    let c = BoundContract::bind(client, BlockId::Latest, *VALIDATOR_SET_ADDRESS);
    call_const_validator!(c, mining_by_staking_address, staking_address.clone())
}

pub fn staking_by_mining_address(
    client: &dyn EngineClient,
    mining_address: &Address,
) -> Result<Address, CallError> {
    let c = BoundContract::bind(client, BlockId::Latest, *VALIDATOR_SET_ADDRESS);
    call_const_validator!(c, staking_by_mining_address, mining_address.clone())
}

pub fn is_pending_validator(
    client: &dyn EngineClient,
    staking_address: &Address,
) -> Result<bool, CallError> {
    let c = BoundContract::bind(client, BlockId::Latest, *VALIDATOR_SET_ADDRESS);
    call_const_validator!(c, is_pending_validator, staking_address.clone())
}

pub fn get_validator_available_since(
    client: &dyn EngineClient,
    address: &Address,
) -> Result<U256, CallError> {
    let c = BoundContract::bind(client, BlockId::Latest, *VALIDATOR_SET_ADDRESS);
    call_const_validator!(c, validator_available_since, address.clone())
}

pub fn get_pending_validators(client: &dyn EngineClient) -> Result<Vec<Address>, CallError> {
    let c = BoundContract::bind(client, BlockId::Latest, *VALIDATOR_SET_ADDRESS);
    call_const_validator!(c, get_pending_validators)
}

pub fn send_tx_announce_availability(
    full_client: &dyn BlockChainClient,
    address: &Address,
) -> Result<(), Error> {
    // chain.latest_nonce(address)
    // we need to get the real latest nonce.
    //let nonce_from_full_client =  full_client.nonce(address,BlockId::Latest);

    let mut nonce = full_client.next_nonce(&address);

    match full_client.nonce(address, BlockId::Latest) {
        Some(new_nonce) => {
            if new_nonce != nonce {
                info!(target:"consensus", "got better nonce for announce availability: {} => {}", nonce, new_nonce);
                nonce = new_nonce;
            }
        }
        None => {}
    }

    let send_data = validator_set_hbbft::functions::announce_availability::call();
    let transaction = TransactionRequest::call(*VALIDATOR_SET_ADDRESS, send_data.0)
        .gas(U256::from(250_000))
        .nonce(nonce);

    info!(target:"consensus", "sending announce availability with nonce: {}", nonce);

    full_client.transact_silently(transaction)?;

    return Ok(());
}
