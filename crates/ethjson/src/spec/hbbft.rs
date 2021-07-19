// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of OpenEthereum.

// OpenEthereum is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// OpenEthereum is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with OpenEthereum.  If not, see <http://www.gnu.org/licenses/>.

//! Hbbft parameter deserialization.

use ethereum_types::Address;
use serde::Deserialize;

/// Hbbft parameters.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct HbbftParams {
    /// The minimum time duration between blocks, in seconds.
    pub minimum_block_time: u64,
    /// The maximum time duration between blocks, in seconds.
    pub maximum_block_time: u64,
    /// The length of the transaction queue at which block creation should be triggered.
    pub transaction_queue_size_trigger: usize,
    /// Should be true when running unit tests to avoid starting timers.
    pub is_unit_test: Option<bool>,
    /// Block reward contract address.
    pub block_reward_contract_address: Option<Address>,
}

/// Hbbft engine config.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hbbft {
    /// Hbbft parameters.
    pub params: HbbftParams,
}

#[cfg(test)]
mod tests {
    use super::Hbbft;
    use ethereum_types::Address;
    use std::str::FromStr;

    #[test]
    fn hbbft_deserialization() {
        let s = r#"{
			"params": {
				"minimumBlockTime": 0,
				"maximumBlockTime": 600,
				"transactionQueueSizeTrigger": 1,
				"isUnitTest": true,
				"blockRewardContractAddress": "0x2000000000000000000000000000000000000002"
			}
		}"#;

        let deserialized: Hbbft = serde_json::from_str(s).unwrap();
        assert_eq!(deserialized.params.minimum_block_time, 0);
        assert_eq!(deserialized.params.maximum_block_time, 600);
        assert_eq!(deserialized.params.transaction_queue_size_trigger, 1);
        assert_eq!(deserialized.params.is_unit_test, Some(true));
        assert_eq!(
            deserialized.params.block_reward_contract_address,
            Address::from_str("2000000000000000000000000000000000000002").ok()
        );
    }
}
