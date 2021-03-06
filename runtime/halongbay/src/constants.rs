pub mod time {
	use runtime_primitives::{BlockNumber};
	/// This determines the average expected block time that we are targeting.
	/// Blocks will be produced at a minimum duration defined by `SLOT_DURATION`.
	/// `SLOT_DURATION` is picked up by `pallet_timestamp` which is in turn picked
	/// up by `pallet_aura` to implement `fn slot_duration()`.
	///
	/// Change this to adjust the block time.
	pub const MILLISECS_PER_BLOCK: u64 = 12000;

	pub const SLOT_DURATION: u64 = MILLISECS_PER_BLOCK;

	// NOTE: Currently it is not possible to change the epoch duration after the chain has started.
	//       Attempting to do so will brick block production.
	pub const EPOCH_DURATION_IN_BLOCKS: BlockNumber = 10 * MINUTES;
	pub const EPOCH_DURATION_IN_SLOTS: u32 = 1 * HOURS;

	// Time is measured by number of blocks.
	pub const MINUTES: BlockNumber = 60_000 / (MILLISECS_PER_BLOCK as BlockNumber);
	pub const HOURS: BlockNumber = MINUTES * 60;
	pub const DAYS: BlockNumber = HOURS * 24;

	pub const CHAIN_ID: u64 = 11;
	pub const SS58PREFIX: u8 = 42;
}

pub mod weights {
	use frame_support::weights::constants::WEIGHT_PER_SECOND;
	/// Current approximation of the gas/s consumption considering
	/// EVM execution over compiled WASM (on 4.4Ghz CPU).
	/// Given the 500ms Weight, from which 75% only are used for transactions,
	/// the total EVM execution gas limit is: GAS_PER_SECOND * 0.500 * 0.75 ~= 15_000_000.
	pub const GAS_PER_SECOND: u64 = 40_000_000;

	/// Approximate ratio of the amount of Weight per Gas.
	/// u64 works for approximations because Weight is a very small unit compared to gas.
	pub const WEIGHT_PER_GAS: u64 = WEIGHT_PER_SECOND / GAS_PER_SECOND;
}

pub mod version {
	use sp_version::RuntimeVersion;
	use sp_runtime::create_runtime_str;

	pub const VERSION: RuntimeVersion = RuntimeVersion {
		spec_name: create_runtime_str!("halongbay"),
		impl_name: create_runtime_str!("halongbay"),
		authoring_version: 1,
		spec_version: 1,
		impl_version: 1,
		apis: crate::RUNTIME_API_VERSIONS,
		transaction_version: 1,
	};
}
