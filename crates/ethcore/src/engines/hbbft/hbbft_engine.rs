use std::{
    cmp::{max, min},
    collections::BTreeMap,
    convert::TryFrom,
    ops::BitXor,
    sync::{atomic::AtomicBool, Arc, Weak},
    time::Duration,
};

use super::block_reward_hbbft::BlockRewardContract;
use block::ExecutedBlock;
use client::traits::{EngineClient, ForceUpdateSealing, TransactionRequest};
use crypto::publickey::Signature;
use engines::{
    default_system_or_code_call, signer::EngineSigner, Engine, EngineError, ForkChoice, Seal,
    SealingState,
};
use error::{BlockError, Error};
use ethereum_types::{H256, H512, U256};
use ethjson::spec::HbbftParams;
use hbbft::{NetworkInfo, Target};
use io::{IoContext, IoHandler, IoService, TimerToken};
use itertools::Itertools;
use machine::EthereumMachine;
use parking_lot::RwLock;
use rlp;
use serde::Deserialize;
use serde_json;
use types::{
    header::{ExtendedHeader, Header},
    ids::BlockId,
    transaction::{SignedTransaction, TypedTransaction},
    BlockNumber,
};

use super::{
    contracts::{
        keygen_history::initialize_synckeygen,
        staking::start_time_of_next_phase_transition,
        validator_set::{get_pending_validators, is_pending_validator, ValidatorType},
    },
    contribution::{unix_now_millis, unix_now_secs},
    hbbft_state::{Batch, HbMessage, HbbftState, HoneyBadgerStep},
    keygen_transactions::KeygenTransactionSender,
    sealing::{self, RlpSig, Sealing},
    NodeId,
};
use engines::hbbft::contracts::validator_set::{
    get_validator_available_since, send_tx_announce_availability, staking_by_mining_address,
};
use std::{ops::Deref, sync::atomic::Ordering};

type TargetedMessage = hbbft::TargetedMessage<Message, NodeId>;

/// A message sent between validators that is part of Honey Badger BFT or the block sealing process.
#[derive(Debug, Deserialize, Serialize)]
enum Message {
    /// A Honey Badger BFT message.
    HoneyBadger(usize, HbMessage),
    /// A threshold signature share. The combined signature is used as the block seal.
    Sealing(BlockNumber, sealing::Message),
}

/// The Honey Badger BFT Engine.
pub struct HoneyBadgerBFT {
    transition_service: IoService<()>,
    client: Arc<RwLock<Option<Weak<dyn EngineClient>>>>,
    signer: Arc<RwLock<Option<Box<dyn EngineSigner>>>>,
    machine: EthereumMachine,
    hbbft_state: RwLock<HbbftState>,
    sealing: RwLock<BTreeMap<BlockNumber, Sealing>>,
    params: HbbftParams,
    message_counter: RwLock<usize>,
    random_numbers: RwLock<BTreeMap<BlockNumber, U256>>,
    keygen_transaction_sender: RwLock<KeygenTransactionSender>,
}

struct TransitionHandler {
    client: Arc<RwLock<Option<Weak<dyn EngineClient>>>>,
    engine: Arc<HoneyBadgerBFT>,
}

const DEFAULT_DURATION: Duration = Duration::from_secs(1);

impl TransitionHandler {
    /// Returns the approximate time duration between the latest block and the given offset
    /// (is 0 if the offset was passed) or the default time duration of 1s.
    fn block_time_until(&self, client: Arc<dyn EngineClient>, offset: u64) -> Duration {
        if let Some(block_header) = client.block_header(BlockId::Latest) {
            // The block timestamp and minimum block time are specified in seconds.
            let next_block_time = (block_header.timestamp() + offset) as u128 * 1000;

            // We get the current time in milliseconds to calculate the exact timer duration.
            let now = unix_now_millis();

            if now >= next_block_time {
                // If the current time is already past the minimum time for the next block
                // return 0 to signal readiness to create the next block.
                Duration::from_secs(0)
            } else {
                // Otherwise wait the exact number of milliseconds needed for the
                // now >= next_block_time condition to be true.
                // Since we know that "now" is smaller than "next_block_time" at this point
                // we also know that "next_block_time - now" will always be a positive number.
                match u64::try_from(next_block_time - now) {
                    Ok(value) => Duration::from_millis(value),
                    _ => {
                        error!(target: "consensus", "Could not convert duration to next block to u64");
                        DEFAULT_DURATION
                    }
                }
            }
        } else {
            error!(target: "consensus", "Latest Block Header could not be obtained!");
            DEFAULT_DURATION
        }
    }

    // Returns the time remaining until minimum block time is passed or the default time duration of 1s.
    fn min_block_time_remaining(&self, client: Arc<dyn EngineClient>) -> Duration {
        self.block_time_until(client, self.engine.params.minimum_block_time)
    }

    // Returns the time remaining until maximum block time is passed or the default time duration of 1s.
    fn max_block_time_remaining(&self, client: Arc<dyn EngineClient>) -> Duration {
        self.block_time_until(client, self.engine.params.maximum_block_time)
    }
}

// Arbitrary identifier for the timer we register with the event handler.
const ENGINE_TIMEOUT_TOKEN: TimerToken = 1;

impl IoHandler<()> for TransitionHandler {
    fn initialize(&self, io: &IoContext<()>) {
        // Start the event loop with an arbitrary timer
        io.register_timer_once(ENGINE_TIMEOUT_TOKEN, DEFAULT_DURATION)
            .unwrap_or_else(
                |e| warn!(target: "consensus", "Failed to start consensus timer: {}.", e),
            )
    }

    fn timeout(&self, io: &IoContext<()>, timer: TimerToken) {
        if timer == ENGINE_TIMEOUT_TOKEN {
            //trace!(target: "consensus", "Honey Badger IoHandler timeout called");
            // The block may be complete, but not have been ready to seal - trigger a new seal attempt.
            // TODO: In theory, that should not happen. The seal is ready exactly when the sealing entry is `Complete`.
            if let Some(ref weak) = *self.client.read() {
                if let Some(c) = weak.upgrade() {
                    c.update_sealing(ForceUpdateSealing::No);
                }
            }

            // Periodically allow messages received for future epochs to be processed.
            self.engine.replay_cached_messages();

            if let Err(e) = self.engine.do_availability_handling() {
                error!(target: "engine", "Error during do_availability_handling: {}", e)
            }

            // The client may not be registered yet on startup, we set the default duration.
            let mut timer_duration = DEFAULT_DURATION;
            if let Some(ref weak) = *self.client.read() {
                if let Some(c) = weak.upgrade() {
                    timer_duration = self.min_block_time_remaining(c.clone());

                    // If the minimum block time has passed we are ready to trigger new blocks.
                    if timer_duration == Duration::from_secs(0) {
                        // Always create blocks if we are in the keygen phase.
                        self.engine.start_hbbft_epoch_if_next_phase();

                        // Transactions may have been submitted during creation of the last block, trigger the
                        // creation of a new block if the transaction threshold has been reached.
                        self.engine.on_transactions_imported();

                        // If the maximum block time has been reached we trigger a new block in any case.
                        if self.max_block_time_remaining(c.clone()) == Duration::from_secs(0) {
                            self.engine.start_hbbft_epoch(c);
                        }

                        // Set timer duration to the default period (1s)
                        timer_duration = DEFAULT_DURATION;
                    }

                    // The duration should be at least 1ms and at most self.engine.params.minimum_block_time
                    timer_duration = max(timer_duration, Duration::from_millis(1));
                    timer_duration = min(
                        timer_duration,
                        Duration::from_secs(self.engine.params.minimum_block_time),
                    );
                }
            }

            io.register_timer_once(ENGINE_TIMEOUT_TOKEN, timer_duration)
				.unwrap_or_else(
					|e| warn!(target: "consensus", "Failed to restart consensus step timer: {}.", e),
				);
        }
    }
}

impl HoneyBadgerBFT {
    /// Creates an instance of the Honey Badger BFT Engine.
    pub fn new(params: HbbftParams, machine: EthereumMachine) -> Result<Arc<Self>, Error> {
        let engine = Arc::new(HoneyBadgerBFT {
            transition_service: IoService::<()>::start("Hbbft")?,
            client: Arc::new(RwLock::new(None)),
            signer: Arc::new(RwLock::new(None)),
            machine,
            hbbft_state: RwLock::new(HbbftState::new()),
            sealing: RwLock::new(BTreeMap::new()),
            params,
            message_counter: RwLock::new(0),
            random_numbers: RwLock::new(BTreeMap::new()),
            keygen_transaction_sender: RwLock::new(KeygenTransactionSender::new()),
        });

        if !engine.params.is_unit_test.unwrap_or(false) {
            let handler = TransitionHandler {
                client: engine.client.clone(),
                engine: engine.clone(),
            };
            engine
                .transition_service
                .register_handler(Arc::new(handler))?;
        }

        Ok(engine)
    }

    fn process_output(
        &self,
        client: Arc<dyn EngineClient>,
        output: Vec<Batch>,
        network_info: &NetworkInfo<NodeId>,
    ) {
        // TODO: Multiple outputs are possible,
        //       process all outputs, respecting their epoch context.
        if output.len() > 1 {
            error!(target: "consensus", "UNHANDLED EPOCH OUTPUTS!");
            panic!("UNHANDLED EPOCH OUTPUTS!");
        }
        let batch = match output.first() {
            None => return,
            Some(batch) => batch,
        };

        trace!(target: "consensus", "Batch received for epoch {}, creating new Block.", batch.epoch);

        // Decode and de-duplicate transactions
        let batch_txns: Vec<_> = batch
            .contributions
            .iter()
            .flat_map(|(_, c)| &c.transactions)
            .filter_map(|ser_txn| {
                // TODO: Report proposers of malformed transactions.
                TypedTransaction::decode(ser_txn).ok()
            })
            .unique()
            .filter_map(|txn| {
                // TODO: Report proposers of invalidly signed transactions.
                SignedTransaction::new(txn).ok()
            })
            .collect();

        // We use the median of all contributions' timestamps
        let timestamps = batch
            .contributions
            .iter()
            .map(|(_, c)| c.timestamp)
            .sorted();

        let timestamp = match timestamps.iter().nth(timestamps.len() / 2) {
            Some(t) => t.clone(),
            None => {
                error!(target: "consensus", "Error calculating the block timestamp");
                return;
            }
        };

        let random_number = batch
            .contributions
            .iter()
            .fold(U256::zero(), |acc, (n, c)| {
                if c.random_data.len() >= 32 {
                    U256::from(&c.random_data[0..32]).bitxor(acc)
                } else {
                    // TODO: Report malicious behavior by node!
                    error!(target: "consensus", "Insufficient random data from node {}", n);
                    acc
                }
            });

        self.random_numbers
            .write()
            .insert(batch.epoch, random_number);

        if let Some(header) = client.create_pending_block_at(batch_txns, timestamp, batch.epoch) {
            let block_num = header.number();
            let hash = header.bare_hash();
            trace!(target: "consensus", "Sending signature share of {} for block {}", hash, block_num);
            let step = match self
                .sealing
                .write()
                .entry(block_num)
                .or_insert_with(|| self.new_sealing(network_info))
                .sign(hash)
            {
                Ok(step) => step,
                Err(err) => {
                    // TODO: Error handling
                    error!(target: "consensus", "Error creating signature share for block {}: {:?}", block_num, err);
                    return;
                }
            };
            self.process_seal_step(client, step, block_num, network_info);
        } else {
            error!(target: "consensus", "Could not create pending block for hbbft epoch {}: ", batch.epoch);
        }
    }

    fn process_hb_message(
        &self,
        msg_idx: usize,
        message: HbMessage,
        sender_id: NodeId,
    ) -> Result<(), EngineError> {
        let client = self.client_arc().ok_or(EngineError::RequiresClient)?;
        trace!(target: "consensus", "Received message of idx {}  {:?} from {}", msg_idx, message, sender_id);
        let step = self.hbbft_state.write().process_message(
            client.clone(),
            &self.signer,
            sender_id,
            message,
        );

        if let Some((step, network_info)) = step {
            self.process_step(client, step, &network_info);
            self.join_hbbft_epoch()?;
        }
        Ok(())
    }

    fn process_sealing_message(
        &self,
        message: sealing::Message,
        sender_id: NodeId,
        block_num: BlockNumber,
    ) -> Result<(), EngineError> {
        let client = self.client_arc().ok_or(EngineError::RequiresClient)?;
        trace!(target: "consensus", "Received sealing message  {:?} from {}", message, sender_id);
        if let Some(latest) = client.block_number(BlockId::Latest) {
            if latest >= block_num {
                return Ok(()); // Message is obsolete.
            }
        }

        let network_info = match self.hbbft_state.write().network_info_for(
            client.clone(),
            &self.signer,
            block_num,
        ) {
            Some(n) => n,
            None => {
                error!(target: "consensus", "Sealing message for block #{} could not be processed due to missing/mismatching network info.", block_num);
                return Err(EngineError::UnexpectedMessage);
            }
        };

        trace!(target: "consensus", "Received signature share for block {} from {}", block_num, sender_id);
        let step_result = self
            .sealing
            .write()
            .entry(block_num)
            .or_insert_with(|| self.new_sealing(&network_info))
            .handle_message(&sender_id, message);
        match step_result {
            Ok(step) => self.process_seal_step(client, step, block_num, &network_info),
            Err(err) => error!(target: "consensus", "Error on ThresholdSign step: {:?}", err), // TODO: Errors
        }
        Ok(())
    }

    fn dispatch_messages<I>(
        &self,
        client: &Arc<dyn EngineClient>,
        messages: I,
        net_info: &NetworkInfo<NodeId>,
    ) where
        I: IntoIterator<Item = TargetedMessage>,
    {
        for m in messages {
            let ser =
                serde_json::to_vec(&m.message).expect("Serialization of consensus message failed");
            match m.target {
                Target::Nodes(set) => {
                    trace!(target: "consensus", "Dispatching message {:?} to {:?}", m.message, set);
                    for node_id in set.into_iter().filter(|p| p != net_info.our_id()) {
                        trace!(target: "consensus", "Sending message to {}", node_id.0);
                        client.send_consensus_message(ser.clone(), Some(node_id.0));
                    }
                }
                Target::AllExcept(set) => {
                    trace!(target: "consensus", "Dispatching exclusive message {:?} to all except {:?}", m.message, set);
                    for node_id in net_info
                        .all_ids()
                        .filter(|p| (p != &net_info.our_id() && !set.contains(p)))
                    {
                        trace!(target: "consensus", "Sending exclusive message to {}", node_id.0);
                        client.send_consensus_message(ser.clone(), Some(node_id.0));
                    }
                }
            }
        }
    }

    fn process_seal_step(
        &self,
        client: Arc<dyn EngineClient>,
        step: sealing::Step,
        block_num: BlockNumber,
        network_info: &NetworkInfo<NodeId>,
    ) {
        let messages = step
            .messages
            .into_iter()
            .map(|msg| msg.map(|m| Message::Sealing(block_num, m)));
        self.dispatch_messages(&client, messages, network_info);
        if let Some(sig) = step.output.into_iter().next() {
            trace!(target: "consensus", "Signature for block {} is ready", block_num);
            let state = Sealing::Complete(sig);
            self.sealing.write().insert(block_num, state);
            client.update_sealing(ForceUpdateSealing::No);
        }
    }

    fn process_step(
        &self,
        client: Arc<dyn EngineClient>,
        step: HoneyBadgerStep,
        network_info: &NetworkInfo<NodeId>,
    ) {
        let mut message_counter = self.message_counter.write();
        let messages = step.messages.into_iter().map(|msg| {
            *message_counter += 1;
            TargetedMessage {
                target: msg.target,
                message: Message::HoneyBadger(*message_counter, msg.message),
            }
        });
        self.dispatch_messages(&client, messages, network_info);
        self.process_output(client, step.output, network_info);
    }

    /// Conditionally joins the current hbbft epoch if the number of received
    /// contributions exceeds the maximum number of tolerated faulty nodes.
    fn join_hbbft_epoch(&self) -> Result<(), EngineError> {
        let client = self.client_arc().ok_or(EngineError::RequiresClient)?;
        if self.is_syncing(&client) {
            trace!(target: "consensus", "tried to join HBBFT Epoch, but still syncing.");
            return Ok(());
        }
        let step = self
            .hbbft_state
            .write()
            .contribute_if_contribution_threshold_reached(client.clone(), &self.signer);
        if let Some((step, network_info)) = step {
            self.process_step(client, step, &network_info)
        }
        Ok(())
    }

    fn start_hbbft_epoch(&self, client: Arc<dyn EngineClient>) {
        if self.is_syncing(&client) {
            return;
        }
        let step = self
            .hbbft_state
            .write()
            .try_send_contribution(client.clone(), &self.signer);
        if let Some((step, network_info)) = step {
            self.process_step(client, step, &network_info)
        }
    }

    fn transaction_queue_and_time_thresholds_reached(
        &self,
        client: &Arc<dyn EngineClient>,
    ) -> bool {
        if let Some(block_header) = client.block_header(BlockId::Latest) {
            let target_min_timestamp = block_header.timestamp() + self.params.minimum_block_time;
            let now = unix_now_secs();
            let queue_length = client.queued_transactions().len();
            (self.params.minimum_block_time == 0 || target_min_timestamp <= now)
                && queue_length >= self.params.transaction_queue_size_trigger
        } else {
            false
        }
    }

    fn new_sealing(&self, network_info: &NetworkInfo<NodeId>) -> Sealing {
        Sealing::new(network_info.clone())
    }

    fn client_arc(&self) -> Option<Arc<dyn EngineClient>> {
        self.client.read().as_ref().and_then(Weak::upgrade)
    }

    fn start_hbbft_epoch_if_next_phase(&self) {
        match self.client_arc() {
            None => return,
            Some(client) => {
                // Get the next phase start time
                let genesis_transition_time = match start_time_of_next_phase_transition(&*client) {
                    Ok(time) => time,
                    Err(_) => return,
                };

                // If current time larger than phase start time, start a new block.
                if genesis_transition_time.as_u64() < unix_now_secs() {
                    self.start_hbbft_epoch(client);
                }
            }
        }
    }

    fn replay_cached_messages(&self) -> Option<()> {
        let client = self.client_arc()?;
        let steps = self
            .hbbft_state
            .write()
            .replay_cached_messages(client.clone());
        let mut processed_step = false;
        if let Some((steps, network_info)) = steps {
            for step in steps {
                match step {
                    Ok(step) => {
                        trace!(target: "engine", "Processing cached message step");
                        processed_step = true;
                        self.process_step(client.clone(), step, &network_info)
                    }
                    Err(e) => error!(target: "engine", "Error handling replayed message: {}", e),
                }
            }
        }

        if processed_step {
            if let Err(e) = self.join_hbbft_epoch() {
                error!(target: "engine", "Error trying to join epoch: {}", e);
            }
        }

        Some(())
    }

    fn do_availability_handling(&self) -> Result<(), String> {
        // only try once on startup-
        static HAS_SENT: AtomicBool = AtomicBool::new(false);

        if !HAS_SENT.load(Ordering::SeqCst) {
            // If we have no signer there is nothing for us to send.
            let address = match self.signer.read().as_ref() {
                Some(signer) => signer.address(),
                None => {
                    // warn!("Could not retrieve address for writing availability transaction.");
                    return Ok(());
                }
            };

            match self.client_arc() {
                Some(client) => {
                    if !self.is_syncing(&client) {
                        let engine_client = client.deref();

                        match staking_by_mining_address(engine_client, &address) {
                            Ok(staking_address) => {
                                if staking_address.is_zero() {
                                    //TODO: here some fine handling can improve performance.
                                    //with this implementation every node (validator or not)
                                    //needs to query this state every block.
                                    //trace!(target: "engine", "availability handling not a validator");
                                    return Ok(());
                                }
                            }
                            Err(call_error) => {
                                error!(target: "engine", "unable to ask for corresponding staking address for given mining address: {:?}", call_error);
                                let message = format!("unable to ask for corresponding staking address for given mining address: {:?}", call_error);
                                return Err(message.into());
                            }
                        }

                        match get_validator_available_since(engine_client, &address) {
                            Ok(s) => {
                                if s.is_zero() {
                                    //let c : &dyn BlockChainClient = client.into();
                                    match client.as_full_client() {
                                        Some(c) => {
                                            //debug!(target: "engine", "sending announce availability transaction");
                                            info!("sending announce availability transaction");
                                            match send_tx_announce_availability(c, &address) {
                                                Ok(()) => {}
                                                Err(call_error) => {
                                                    //error!(target: "engine", "CallError during announce availability. {:?}", call_error);
                                                    return Err(format!("CallError during announce availability. {:?}", call_error));
                                                }
                                            }
                                        }
                                        None => {
                                            return Err(
                                                "Unable to retrieve client.as_full_client()".into(),
                                            );
                                        }
                                    }

                                    HAS_SENT.store(true, Ordering::SeqCst);
                                }
                            }
                            Err(e) => {
                                //return Err(format!("Error trying to send availability check: {:?}", e));
                                return Err(format!(
                                    "Error trying to send availability check: {:?}",
                                    e
                                ));
                            }
                        }
                    }
                }
                None => {
                    return Err("could not send availability announcement because client_arc could not be retrieved:".into());
                }
            }
        }

        return Ok(());
    }

    /// Returns true if we are in the keygen phase and a new key has been generated.
    fn do_keygen(&self) -> bool {
        match self.client_arc() {
            None => false,
            Some(client) => {
                // If we are not in key generation phase, return false.
                match get_pending_validators(&*client) {
                    Err(_) => return false,
                    Ok(validators) => {
                        // If the validator set is empty then we are not in the key generation phase.
                        if validators.is_empty() {
                            return false;
                        }
                    }
                }

                // Check if a new key is ready to be generated, return true to switch to the new epoch in that case.
                if let Ok(synckeygen) = initialize_synckeygen(
                    &*client,
                    &self.signer,
                    BlockId::Latest,
                    ValidatorType::Pending,
                ) {
                    if synckeygen.is_ready() {
                        return true;
                    }
                }

                // Otherwise check if we are in the pending validator set and send Parts and Acks transactions.
                // @todo send_keygen_transactions initializes another synckeygen structure, a potentially
                //       time consuming process. Move sending of keygen transactions into a separate function
                //       and call it periodically using timer events instead of on close block.
                if let Some(signer) = self.signer.read().as_ref() {
                    if let Ok(is_pending) = is_pending_validator(&*client, &signer.address()) {
                        trace!(target: "engine", "is_pending_validator: {}", is_pending);
                        if is_pending {
                            let _err = self
                                .keygen_transaction_sender
                                .write()
                                .send_keygen_transactions(&*client, &self.signer);
                            match _err {
                                Ok(()) => {}
                                Err(e) => {
                                    error!(target: "engine", "Error sending keygen transactions {:?}", e);
                                }
                            }
                        }
                    }
                }
                false
            }
        }
    }

    fn check_for_epoch_change(&self) -> Option<()> {
        let client = self.client_arc()?;
        if let None = self.hbbft_state.write().update_honeybadger(
            client,
            &self.signer,
            BlockId::Latest,
            false,
        ) {
            error!(target: "consensus", "Fatal: Updating Honey Badger instance failed!");
        }
        Some(())
    }

    fn is_syncing(&self, client: &Arc<dyn EngineClient>) -> bool {
        match client.as_full_client() {
            Some(full_client) => full_client.is_major_syncing(),
            // We only support full clients at this point.
            None => true,
        }
    }
}

impl Engine<EthereumMachine> for HoneyBadgerBFT {
    fn name(&self) -> &str {
        "HoneyBadgerBFT"
    }

    fn machine(&self) -> &EthereumMachine {
        &self.machine
    }

    fn fork_choice(&self, new: &ExtendedHeader, current: &ExtendedHeader) -> ForkChoice {
        crate::engines::total_difficulty_fork_choice(new, current)
    }

    fn verify_local_seal(&self, _header: &Header) -> Result<(), Error> {
        self.check_for_epoch_change();
        Ok(())
    }

    /// Phase 1 Checks
    fn verify_block_basic(&self, _header: &Header) -> Result<(), Error> {
        Ok(())
    }

    /// Pase 2 Checks
    fn verify_block_unordered(&self, _header: &Header) -> Result<(), Error> {
        Ok(())
    }

    /// Phase 3 Checks
    /// We check the signature here since at this point the blocks are imported in-order.
    /// To verify the signature we need the parent block already imported on the chain.
    fn verify_block_family(&self, header: &Header, _parent: &Header) -> Result<(), Error> {
        let client = self.client_arc().ok_or(EngineError::RequiresClient)?;

        let latest_block_nr = client.block_number(BlockId::Latest).expect("must succeed");

        if header.number() > (latest_block_nr + 1) {
            error!(target: "engine", "Phase 3 block verification out of order!");
            return Err(BlockError::InvalidSeal.into());
        }

        if header.seal().len() != 1 {
            return Err(BlockError::InvalidSeal.into());
        }

        let RlpSig(sig) = rlp::decode(header.seal().first().ok_or(BlockError::InvalidSeal)?)?;
        if self
            .hbbft_state
            .write()
            .verify_seal(client, &self.signer, &sig, header)
        {
            Ok(())
        } else {
            error!(target: "engine", "Invalid seal for block #{}!", header.number());
            Err(BlockError::InvalidSeal.into())
        }
    }

    // Phase 4
    fn verify_block_external(&self, _header: &Header) -> Result<(), Error> {
        Ok(())
    }

    fn register_client(&self, client: Weak<dyn EngineClient>) {
        *self.client.write() = Some(client.clone());
        if let Some(client) = self.client_arc() {
            if let None = self.hbbft_state.write().update_honeybadger(
                client,
                &self.signer,
                BlockId::Latest,
                true,
            ) {
                // As long as the client is set we should be able to initialize as a regular node.
                error!(target: "engine", "Error during HoneyBadger initialization!");
            }
        }
    }

    fn set_signer(&self, signer: Option<Box<dyn EngineSigner>>) {
        *self.signer.write() = signer;
        if let Some(client) = self.client_arc() {
            if let None = self.hbbft_state.write().update_honeybadger(
                client,
                &self.signer,
                BlockId::Latest,
                true,
            ) {
                info!(target: "engine", "HoneyBadger Algorithm could not be created, Client possibly not set yet.");
            }
        }
    }

    fn sign(&self, hash: H256) -> Result<Signature, Error> {
        match self.signer.read().as_ref() {
            Some(signer) => signer
                .sign(hash)
                .map_err(|_| EngineError::RequiresSigner.into()),
            None => Err(EngineError::RequiresSigner.into()),
        }
    }

    fn generate_engine_transactions(
        &self,
        block: &ExecutedBlock,
    ) -> Result<Vec<SignedTransaction>, Error> {
        self.check_for_epoch_change();
        let _random_number = match self.random_numbers.read().get(&block.header.number()) {
            None => {
                return Err(EngineError::Custom(
                    "No value available for calling randomness contract.".into(),
                )
                .into())
            }
            Some(r) => r,
        };
        Ok(Vec::new())
    }

    fn sealing_state(&self) -> SealingState {
        // Purge obsolete sealing processes.
        let client = match self.client_arc() {
            None => return SealingState::NotReady,
            Some(client) => client,
        };
        let next_block = match client.block_number(BlockId::Latest) {
            None => return SealingState::NotReady,
            Some(block_num) => block_num + 1,
        };
        let mut sealing = self.sealing.write();
        *sealing = sealing.split_off(&next_block);

        // We are ready to seal if we have a valid signature for the next block.
        if let Some(next_seal) = sealing.get(&next_block) {
            if next_seal.signature().is_some() {
                return SealingState::Ready;
            }
        }
        SealingState::NotReady
    }

    fn on_transactions_imported(&self) {
        self.check_for_epoch_change();
        if let Some(client) = self.client_arc() {
            if self.transaction_queue_and_time_thresholds_reached(&client) {
                self.start_hbbft_epoch(client);
            }
        }
    }

    fn handle_message(&self, message: &[u8], node_id: Option<H512>) -> Result<(), EngineError> {
        self.check_for_epoch_change();
        let node_id = NodeId(node_id.ok_or(EngineError::UnexpectedMessage)?);
        match serde_json::from_slice(message) {
            Ok(Message::HoneyBadger(msg_idx, hb_msg)) => {
                self.process_hb_message(msg_idx, hb_msg, node_id)
            }
            Ok(Message::Sealing(block_num, seal_msg)) => {
                self.process_sealing_message(seal_msg, node_id, block_num)
            }
            Err(_) => Err(EngineError::MalformedMessage(
                "Serde message decoding failed.".into(),
            )),
        }
    }

    fn seal_fields(&self, _header: &Header) -> usize {
        1
    }

    fn generate_seal(&self, block: &ExecutedBlock, _parent: &Header) -> Seal {
        let client = match self.client_arc() {
            None => return Seal::None,
            Some(client) => client,
        };

        let block_num = block.header.number();
        let sealing = self.sealing.read();
        let sig = match sealing.get(&block_num).and_then(Sealing::signature) {
            None => return Seal::None,
            Some(sig) => sig,
        };
        if !self
            .hbbft_state
            .write()
            .verify_seal(client, &self.signer, &sig, &block.header)
        {
            error!(target: "consensus", "generate_seal: Threshold signature does not match new block.");
            return Seal::None;
        }
        trace!(target: "consensus", "Returning generated seal for block {}.", block_num);
        Seal::Regular(vec![rlp::encode(&RlpSig(sig))])
    }

    fn should_miner_prepare_blocks(&self) -> bool {
        false
    }

    fn use_block_author(&self) -> bool {
        false
    }

    fn on_close_block(&self, block: &mut ExecutedBlock) -> Result<(), Error> {
        self.check_for_epoch_change();
        if let Some(address) = self.params.block_reward_contract_address {
            let header_number = block.header.number();
            let mut call = default_system_or_code_call(&self.machine, block);
            let is_epoch_end = self.do_keygen();
            trace!(target: "consensus", "calling reward function for block {} isEpochEnd? {} on address: {}", header_number,  is_epoch_end, address);
            let contract = BlockRewardContract::new_from_address(address);
            let _total_reward = contract.reward(&mut call, is_epoch_end)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{contribution::Contribution, test::create_transactions::create_transaction};
    use crypto::publickey::{Generator, Random};
    use ethereum_types::U256;
    use hbbft::{
        honey_badger::{HoneyBadger, HoneyBadgerBuilder},
        NetworkInfo,
    };
    use rand_065;
    use std::sync::Arc;
    use types::transaction::SignedTransaction;

    #[test]
    fn test_single_contribution() {
        let mut rng = rand_065::thread_rng();
        let net_infos = NetworkInfo::generate_map(0..1usize, &mut rng)
            .expect("NetworkInfo generation is expected to always succeed");

        let net_info = net_infos
            .get(&0)
            .expect("A NetworkInfo must exist for node 0");

        let mut builder: HoneyBadgerBuilder<Contribution, _> =
            HoneyBadger::builder(Arc::new(net_info.clone()));

        let mut honey_badger = builder.build();

        let mut pending: Vec<SignedTransaction> = Vec::new();
        let keypair = Random.generate();
        pending.push(create_transaction(&keypair, &U256::from(1)));
        let input_contribution = Contribution::new(&pending);

        let step = honey_badger
            .propose(&input_contribution, &mut rng)
            .expect("Since there is only one validator we expect an immediate result");

        // Assure the contribution returned by HoneyBadger matches the input
        assert_eq!(step.output.len(), 1);
        let out = step.output.first().unwrap();
        assert_eq!(out.epoch, 0);
        assert_eq!(out.contributions.len(), 1);
        assert_eq!(out.contributions.get(&0).unwrap(), &input_contribution);
    }
}
