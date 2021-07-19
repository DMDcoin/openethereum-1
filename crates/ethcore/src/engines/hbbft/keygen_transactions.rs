use client::traits::{EngineClient, TransactionRequest};
use engines::{
    hbbft::{
        contracts::{
            keygen_history::{
                engine_signer_to_synckeygen, has_acks_of_address_data, has_part_of_address_data,
                key_history_contract, part_of_address, PublicWrapper, KEYGEN_HISTORY_ADDRESS,
            },
            staking::get_posdao_epoch,
            validator_set::{get_validator_pubkeys, ValidatorType},
        },
        utils::bound_contract::CallError,
    },
    signer::EngineSigner,
};
use ethereum_types::U256;
use itertools::Itertools;
use parking_lot::RwLock;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use types::ids::BlockId;

pub struct KeygenTransactionSender {
    last_part_sent: u64,
    last_acks_sent: u64,
    resend_delay: u64,
}

impl KeygenTransactionSender {
    pub fn new() -> Self {
        KeygenTransactionSender {
            last_part_sent: 0,
            last_acks_sent: 0,
            resend_delay: 10,
        }
    }

    fn part_threshold_reached(&self, block_number: u64) -> bool {
        self.last_part_sent == 0 || block_number > (self.last_part_sent + self.resend_delay)
    }

    fn acks_threshold_reached(&self, block_number: u64) -> bool {
        self.last_acks_sent == 0 || block_number > (self.last_acks_sent + self.resend_delay)
    }

    /// Returns a collection of transactions the pending validator has to submit in order to
    /// complete the keygen history contract data necessary to generate the next key and switch to the new validator set.
    pub fn send_keygen_transactions(
        &mut self,
        client: &dyn EngineClient,
        signer: &Arc<RwLock<Option<Box<dyn EngineSigner>>>>,
    ) -> Result<(), CallError> {
        static LAST_PART_SENT: AtomicU64 = AtomicU64::new(0);
        static LAST_ACKS_SENT: AtomicU64 = AtomicU64::new(0);

        // If we have no signer there is nothing for us to send.
        let address = match signer.read().as_ref() {
            Some(signer) => signer.address(),
            None => {
                trace!(target: "engine", "Could not send keygen transactions, because signer module could not be retrieved");
                return Err(CallError::ReturnValueInvalid);
            }
        };
        trace!(target:"engine", "getting full client...");
        let full_client = client.as_full_client().ok_or(CallError::NotFullClient)?;

        // If the chain is still syncing, do not send Parts or Acks.
        if full_client.is_major_syncing() {
            trace!(target:"engine", "skipping sending key gen transaction, because we are syncing");
            return Ok(());
        }

        trace!(target:"engine", " get_validator_pubkeys...");

        let vmap = get_validator_pubkeys(&*client, BlockId::Latest, ValidatorType::Pending)?;
        let pub_keys: BTreeMap<_, _> = vmap
            .values()
            .map(|p| (*p, PublicWrapper { inner: p.clone() }))
            .collect();

        // if synckeygen creation fails then either signer or validator pub keys are problematic.
        // Todo: We should expect up to f clients to write invalid pub keys. Report and re-start pending validator set selection.
        let (mut synckeygen, part) = engine_signer_to_synckeygen(signer, Arc::new(pub_keys))
            .map_err(|_| CallError::ReturnValueInvalid)?;

        // If there is no part then we are not part of the pending validator set and there is nothing for us to do.
        let part_data = match part {
            Some(part) => part,
            None => return Err(CallError::ReturnValueInvalid),
        };

        let upcoming_epoch = get_posdao_epoch(client, BlockId::Latest)? + 1;
        trace!(target:"engine", "preparing to send PARTS for upcomming epoch: {}", upcoming_epoch);

        let cur_block = client
            .block_number(BlockId::Latest)
            .ok_or(CallError::ReturnValueInvalid)?;

        // Check if we already sent our part.
        if (LAST_PART_SENT.load(Ordering::SeqCst) + 10 < cur_block)
            && !has_part_of_address_data(client, address)?
        {
            let serialized_part = match bincode::serialize(&part_data) {
                Ok(part) => part,
                Err(_) => return Err(CallError::ReturnValueInvalid),
            };
            let serialized_part_len = serialized_part.len();
            let write_part_data =
                key_history_contract::functions::write_part::call(upcoming_epoch, serialized_part);

            // the required gas values have been approximated by
            // experimenting and it's a very rough estimation.
            // it can be further fine tuned to be just above the real consumption.
            // ACKs require much more gas,
            // and usually run into the gas limit problems.
            let gas: usize = serialized_part_len * 750 + 100_000;

            trace!(target: "engine", "Hbbft part transaction gas: part-len: {} gas: {}", serialized_part_len, gas);

            let part_transaction =
                TransactionRequest::call(*KEYGEN_HISTORY_ADDRESS, write_part_data.0)
                    .gas(U256::from(gas))
                    .nonce(full_client.nonce(&address, BlockId::Latest).unwrap())
                    .gas_price(U256::from(10000000000u64));
            full_client
                .transact_silently(part_transaction)
                .map_err(|_| CallError::ReturnValueInvalid)?;
            LAST_PART_SENT.store(cur_block, Ordering::SeqCst);
        }

        trace!(target:"engine", "checking for acks...");
        // Return if any Part is missing.
        let mut acks = Vec::new();
        for v in vmap.keys().sorted() {
            acks.push(
				match part_of_address(&*client, *v, &vmap, &mut synckeygen, BlockId::Latest) {
					Ok(part_result) => {
						match part_result {
							    Some(ack) => ack,
							    None => {
							        trace!(target:"engine", "could not retrieve part for {}", *v);
							        return Err(CallError::ReturnValueInvalid);
							    }
							}
					}
					Err(err) => {
						error!(target:"engine", "could not retrieve part for {} call failed. Error: {:?}", *v, err);
						return Err(err);
					}
				}
            );
        }

        trace!(target:"engine", "has_acks_of_address_data: {:?}", has_acks_of_address_data(client, address));

        // Now we are sure all parts are ready, let's check if we sent our Acks.
        if (LAST_ACKS_SENT.load(Ordering::SeqCst) + 10 < cur_block)
            && !has_acks_of_address_data(client, address)?
        {
            let mut serialized_acks = Vec::new();
            let mut total_bytes_for_acks = 0;

            for ack in acks {
                let ack_to_push = match bincode::serialize(&ack) {
                    Ok(serialized_ack) => serialized_ack,
                    Err(_) => return Err(CallError::ReturnValueInvalid),
                };
                total_bytes_for_acks += ack_to_push.len();
                serialized_acks.push(ack_to_push);
            }

            let write_acks_data =
                key_history_contract::functions::write_acks::call(upcoming_epoch, serialized_acks);

            // the required gas values have been approximated by
            // experimenting and it's a very rough estimation.
            // it can be further fine tuned to be just above the real consumption.
            let gas = total_bytes_for_acks * 800 + 200_000;
            trace!(target: "engine","acks-len: {} gas: {}", total_bytes_for_acks, gas);

            let acks_transaction =
                TransactionRequest::call(*KEYGEN_HISTORY_ADDRESS, write_acks_data.0)
                    .gas(U256::from(gas))
                    .nonce(full_client.nonce(&address, BlockId::Latest).unwrap())
                    .gas_price(U256::from(10000000000u64));
            full_client
                .transact_silently(acks_transaction)
                .map_err(|_| CallError::ReturnValueInvalid)?;
            LAST_ACKS_SENT.store(cur_block, Ordering::SeqCst);
        }

        Ok(())
    }
}
