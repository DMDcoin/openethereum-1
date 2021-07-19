mod block_reward_hbbft;
mod contracts;
mod contribution;
mod hbbft_engine;
mod hbbft_state;
mod keygen_transactions;
mod sealing;
#[cfg(test)]
mod test;
mod utils;

pub use self::hbbft_engine::HoneyBadgerBFT;

use crypto::publickey::Public;
use std::fmt;

#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub Public);

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "{:6}", hex_fmt::HexFmt(&self.0))
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "NodeId({})", self.0)
    }
}
