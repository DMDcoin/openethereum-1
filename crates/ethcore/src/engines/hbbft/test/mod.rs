use super::{
    contracts::{
        staking::{
            get_posdao_epoch, start_time_of_next_phase_transition,
            tests::{create_staker, is_pool_active},
        },
        validator_set::{is_pending_validator, mining_by_staking_address},
    },
    contribution::unix_now_secs,
    test::hbbft_test_client::{create_hbbft_client, create_hbbft_clients},
};
use client::traits::BlockInfo;
use crypto::publickey::{Generator, KeyPair, Random, Secret};
use ethereum_types::{Address, U256};
use std::str::FromStr;
use types::ids::BlockId;

pub mod create_transactions;
pub mod hbbft_test_client;
pub mod network_simulator;

lazy_static! {
    static ref MASTER_OF_CEREMONIES_KEYPAIR: KeyPair = KeyPair::from_secret(
        Secret::from_str("18f059a4d72d166a96c1edfb9803af258a07b5ec862a961b3a1d801f443a1762")
            .expect("Secret from hex string must succeed")
    )
    .expect("KeyPair generation from secret must succeed");
}

#[test]
fn test_miner_transaction_injection() {
    let mut test_data = create_hbbft_client(MASTER_OF_CEREMONIES_KEYPAIR.clone());

    // Verify that we actually start at block 0.
    assert_eq!(test_data.client.chain().best_block_number(), 0);

    // Inject a transaction, with instant sealing a block will be created right away.
    test_data.create_some_transaction(None);

    // Expect a new block to be created.
    assert_eq!(test_data.client.chain().best_block_number(), 1);

    // Expect one transaction in the block.
    let block = test_data
        .client
        .block(BlockId::Number(1))
        .expect("Block 1 must exist");
    assert_eq!(block.transactions_count(), 1);
}

#[test]
fn test_staking_account_creation() {
    // Create Master of Ceremonies
    let mut moc = create_hbbft_client(MASTER_OF_CEREMONIES_KEYPAIR.clone());

    // Verify the master of ceremony is funded.
    assert!(moc.balance(&moc.address()) > U256::from(10000000));

    // Create a potential validator.
    let miner_1 = create_hbbft_client(Random.generate());

    // Verify the pending validator is unfunded.
    assert_eq!(moc.balance(&miner_1.address()), U256::from(0));

    // Verify that we actually start at block 0.
    assert_eq!(moc.client.chain().best_block_number(), 0);

    let transaction_funds = U256::from(9000000000000000000u64);

    // Inject a transaction, with instant sealing a block will be created right away.
    moc.transfer_to(&miner_1.address(), &transaction_funds);

    // Expect a new block to be created.
    assert_eq!(moc.client.chain().best_block_number(), 1);

    // Verify the pending validator is now funded.
    assert_eq!(moc.balance(&miner_1.address()), transaction_funds);

    // Create staking address
    let funder = moc.keypair.clone();
    let staker_1 = create_staker(&mut moc, &funder, &miner_1, transaction_funds);

    // Expect two new blocks to be created, one for the transfer of staking funds,
    // one for registering the staker as pool.
    assert_eq!(moc.client.chain().best_block_number(), 3);

    // Expect one transaction in the block.
    let block = moc
        .client
        .block(BlockId::Number(3))
        .expect("Block must exist");
    assert_eq!(block.transactions_count(), 1);

    assert_ne!(
        mining_by_staking_address(moc.client.as_ref(), &staker_1.address())
            .expect("Constant call must succeed."),
        Address::zero()
    );

    // Check if the staking pool is active.
    assert_eq!(
        is_pool_active(moc.client.as_ref(), staker_1.address())
            .expect("Pool active query must succeed."),
        true
    );
}

#[test]
fn test_epoch_transition() {
    // Create Master of Ceremonies
    let mut moc = create_hbbft_client(MASTER_OF_CEREMONIES_KEYPAIR.clone());
    // To avoid performing external transactions with the MoC we create and fund a random address.
    let transactor: KeyPair = Random.generate();

    let genesis_transition_time = start_time_of_next_phase_transition(moc.client.as_ref())
        .expect("Constant call must succeed");

    // Genesis block is at time 0, current unix time must be much larger.
    assert!(genesis_transition_time.as_u64() < unix_now_secs());

    // We should not be in the pending validator set at the genesis block.
    assert!(!is_pending_validator(moc.client.as_ref(), &moc.address())
        .expect("Constant call must succeed"));

    // Fund the transactor.
    // Also triggers the creation of a block.
    // This implicitly calls the block reward contract, which should trigger a phase transition
    // since we already verified that the genesis transition time threshold has been reached.
    let transaction_funds = U256::from(9000000000000000000u64);
    moc.transfer_to(&transactor.address(), &transaction_funds);

    // Expect a new block to be created.
    assert_eq!(moc.client.chain().best_block_number(), 1);

    // Now we should be part of the pending validator set.
    assert!(is_pending_validator(moc.client.as_ref(), &moc.address())
        .expect("Constant call must succeed"));

    // Check if we are still in the first epoch.
    assert_eq!(
        get_posdao_epoch(moc.client.as_ref(), BlockId::Latest).expect("Constant call must succeed"),
        U256::from(0)
    );

    // First the validator realizes it is in the next validator set and sends his part.
    moc.create_some_transaction(Some(&transactor));

    // The part will be included in the block triggered by this transaction, but not part of the global state yet,
    // so it sends the transaction another time.
    moc.create_some_transaction(Some(&transactor));

    // Now the part is part of the global chain state, and we send our acks.
    moc.create_some_transaction(Some(&transactor));

    // The acks will be included in the block triggered by this transaction, but not part of the global state yet.
    moc.create_some_transaction(Some(&transactor));

    // Now the acks are part of the global block state, and the key generation is complete and the next epoch begins
    moc.create_some_transaction(Some(&transactor));

    // At this point we should be in the new epoch.
    assert_eq!(
        get_posdao_epoch(moc.client.as_ref(), BlockId::Latest).expect("Constant call must succeed"),
        U256::from(1)
    );

    // Let's do another one to check if the transition to the new honey badger and keys works.
    moc.create_some_transaction(Some(&transactor));
}

#[test]
fn sync_two_validators() {
    // Create the MOC client
    let mut moc = create_hbbft_client(MASTER_OF_CEREMONIES_KEYPAIR.clone());

    // To avoid performing external transactions with the MoC we create and fund a random address.
    let transactor: KeyPair = Random.generate();

    // Fund the transactor.
    // Also triggers the creation of a block.
    // This implicitly calls the block reward contract, which should trigger a phase transition
    // since we already verified that the genesis transition time threshold has been reached.
    let transaction_funds = U256::from(9000000000000000000u64);
    moc.transfer_to(&transactor.address(), &transaction_funds);

    // Expect a new block to be created.
    assert_eq!(moc.client.chain().best_block_number(), 1);

    // Verify the pending validator is now funded.
    assert_eq!(moc.balance(&transactor.address()), transaction_funds);

    // Create the pool 1 client and set up the pool
    let mut validator_1 = create_hbbft_client(Random.generate());

    // Verify the pending validator is now funded.
    assert_eq!(validator_1.balance(&transactor.address()), U256::zero());

    // Sync blocks from moc to validator 1
    moc.sync_blocks_to(&mut validator_1);

    // Check if new blocks have been added to validator_1 client.
    assert_eq!(
        validator_1.balance(&transactor.address()),
        transaction_funds
    );

    moc.sync_transactions_to(&mut validator_1);
}

#[test]
fn test_moc_to_first_validator() {
    // Create MOC client
    let mut moc = create_hbbft_client(MASTER_OF_CEREMONIES_KEYPAIR.clone());

    // Create first validator client
    let mut validator_1 = create_hbbft_client(Random.generate());

    // To avoid performing external transactions with the MoC we create and fund a random address.
    let transactor: KeyPair = Random.generate();

    // Fund the transactor.
    // Also triggers the creation of a block.
    // This implicitly calls the block reward contract, which should trigger a phase transition
    // since we already verified that the genesis transition time threshold has been reached.
    moc.transfer_to(
        &transactor.address(),
        &U256::from_dec_str("1000000000000000000000000").unwrap(),
    );

    let transaction_funds = U256::from(9000000000000000000u64);
    moc.transfer(&transactor, &validator_1.address(), &transaction_funds);

    // Create first pool
    // Create staking address
    let _staker_1 = create_staker(&mut moc, &transactor, &validator_1, transaction_funds);

    // Wait for moc keygen phase to finish
    moc.create_some_transaction(Some(&transactor));
    moc.create_some_transaction(Some(&transactor));
    //moc.create_some_transaction(Some(&transactor));

    // In the next block the POSDAO contracts realize they need to
    // switch to the new validator.
    moc.create_some_transaction(Some(&transactor));
    // We need to create another block to give the new validator a chance
    // to find out it is in the pending validator set.
    moc.create_some_transaction(Some(&transactor));

    // Now we should be part of the pending validator set.
    assert!(
        is_pending_validator(moc.client.as_ref(), &validator_1.address())
            .expect("Constant call must succeed")
    );
    // ..and the MOC should not be a pending validator.
    assert!(!is_pending_validator(moc.client.as_ref(), &moc.address())
        .expect("Constant call must succeed"));

    // Sync blocks from MOC to validator_1.
    // On importing the last block validator_1 should realize he is the next
    // validator and generate a Parts transaction.
    moc.sync_blocks_to(&mut validator_1);

    // validator_1 created a transaction to write its part, but it is not
    // the current validator and cannot create a block.
    // We need to gossip the transaction from validator_1 to the moc for a new block
    // to be created, including the transaction from validator_1.
    validator_1.sync_transactions_to(&mut moc);

    // Write another dummy block to give validator_1 the chance to realize he wrote
    // his Part already so he sends his Acks.
    moc.create_some_transaction(Some(&transactor));

    // At this point the transaction from validator_1 has written its Keygen part,
    // and we need to sync the new blocks from moc to validator_1.
    moc.sync_blocks_to(&mut validator_1);

    // At this point validator_1 realizes his Part is included on the chain and
    // generates a transaction to write it Acks.
    // We need to gossip the transactions from validator_1 to the moc.
    validator_1.sync_transactions_to(&mut moc);

    // Create a dummy transaction for the moc to see the Acks on the chain state,
    // and make him switch to the new validator.
    moc.create_some_transaction(Some(&transactor));

    // Sync blocks from moc to validator_1, which is now the only active validator.
    moc.sync_blocks_to(&mut validator_1);

    let pre_block_nr = validator_1.client.chain().best_block_number();

    // Create a dummy transaction on the validator_1 client to verify it can create blocks.
    validator_1.create_some_transaction(Some(&transactor));

    assert_eq!(
        validator_1.client.chain().best_block_number(),
        pre_block_nr + 1
    );
}

#[test]
fn test_initialize_n_validators() {
    let mut moc = create_hbbft_client(MASTER_OF_CEREMONIES_KEYPAIR.clone());

    let funder: KeyPair = Random.generate();
    let fund_amount = U256::from_dec_str("1000000000000000000000000").unwrap();
    moc.transfer_to(&funder.address(), &fund_amount);

    let mut clients = create_hbbft_clients(moc, 2, &funder);

    assert_eq!(
        clients
            .iter()
            .nth(1)
            .unwrap()
            .read()
            .balance(&funder.address()),
        U256::zero()
    );

    network_simulator::crank_network(&mut clients);

    assert_eq!(
        clients
            .iter()
            .nth(1)
            .unwrap()
            .read()
            .balance(&funder.address()),
        fund_amount
    );
}
