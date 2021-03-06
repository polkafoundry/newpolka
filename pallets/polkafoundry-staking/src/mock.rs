use crate::{self as stake, Config, CollatorPoints, TotalPoints};
use frame_support::{
	construct_runtime, parameter_types,
	traits::{GenesisBuild, OnFinalize, OnInitialize},
};
use sp_io;
use sp_runtime::{
	Perbill,
	testing::Header,
	traits::{BlakeTwo256, IdentityLookup},
};
use sp_std::convert::{From};
use sp_core::H256;
use frame_election_provider_support::onchain;

pub type AccountId = u64;
pub type Balance = u128;

parameter_types! {
	pub const BlockHashCount: u64 = 250;
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(1024);
}

impl frame_system::Config for Test {
	type BaseCallFilter = ();
	type BlockWeights = ();
	type BlockLength = ();
	type Origin = Origin;
	type Index = u64;
	type Call = Call;
	type BlockNumber = u64;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Header = Header;
	type Event = Event;
	type BlockHashCount = BlockHashCount;
	type DbWeight = ();
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = pallet_balances::AccountData<Balance>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type OnSetCode = ();
	type SystemWeightInfo = ();
	type SS58Prefix = ();
}

parameter_types! {
	pub const ExistentialDeposit: u128 = 1;
}

impl pallet_balances::Config for Test {
	type MaxLocks = ();
	type Balance = Balance;
	type Event = Event;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
}

impl pallet_utility::Config for Test {
	type Event = Event;
	type Call = Call;
	type WeightInfo = ();
}

impl onchain::Config for Test {
	type AccountId = u64;
	type BlockNumber = u64;
	type BlockWeights = BlockWeights;
	type Accuracy = Perbill;
	type DataProvider = Staking;
}

parameter_types! {
	pub const BlocksPerRound: u32 = 10;
	pub const MaxCollatorsPerNominator: u32 = 5;
	pub const MaxNominationsPerCollator: u32 = 2;
	pub const BondDuration: u32 = 2;
	pub const MinCollatorStake: u32 = 500;
	pub const MinNominatorStake: u32 = 100;
	pub const PayoutDuration: u32 = 2;
	pub const DesiredTarget: u32 = 2;
}

impl Config for Test {
	const MAX_COLLATORS_PER_NOMINATOR: u32 = 5u32;
	type Event = Event;
	type Currency = Balances;
	type BlocksPerRound = BlocksPerRound;
	type MaxNominationsPerCollator = MaxNominationsPerCollator;
	type BondDuration = BondDuration;
	type MinCollatorStake = MinCollatorStake;
	type MinNominatorStake = MinNominatorStake;
	type PayoutDuration = PayoutDuration;
	type ElectionProvider = onchain::OnChainSequentialPhragmen<Self>;
	type CurrencyToVote = frame_support::traits::SaturatingCurrencyToVote;
	type DesiredTarget = DesiredTarget;
}

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlock<Test>;

construct_runtime!(
	pub enum Test where
		Block = Block,
		NodeBlock = Block,
		UncheckedExtrinsic = UncheckedExtrinsic,
	{
		System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		Staking: stake::{Pallet, Call, Storage, Event<T>},
		Utility: pallet_utility::{Pallet, Call, Storage, Event},
	}
);

pub struct ExtBuilder;

impl ExtBuilder {
	pub fn build(
		balances: Vec<(AccountId, Balance)>,
		stakers: Vec<(AccountId, Balance)>,
	) -> sp_io::TestExternalities {
		let mut storage = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		pallet_balances::GenesisConfig::<Test> { balances }
			.assimilate_storage(&mut storage)
			.unwrap();
		stake::GenesisConfig::<Test> {
			stakers,
		}.assimilate_storage(&mut storage)
			.unwrap();

		let mut ext = sp_io::TestExternalities::from(storage);
		ext.execute_with(|| {
			System::set_block_number(1)
		});

		ext
	}
}

pub(crate) fn mock_test() -> sp_io::TestExternalities {
	ExtBuilder::build(vec![
		// collator
		(1, 1000),
		(2, 500),
		(3, 800),
		(100, 5000),
		(200, 2000),
		(300, 3000),
		(400, 3000),
		// nominator
		(10, 1000),
		(20, 500),
		(30, 800),
		(999, 200000000),
	], vec![
		(100, 500),
		(200, 500),
		(300, 600),
		(400, 400),
	])
}

pub(crate) fn events() -> Vec<super::Event<Test>> {
	System::events()
		.into_iter()
		.map(|r| r.event)
		.filter_map(|e| {
			if let Event::stake(inner) = e {
				Some(inner)
			} else {
				None
			}
		})
		.collect::<Vec<_>>()
}

pub(crate) fn run_to_block(n: u64) {
	while System::block_number() < n {
		Staking::on_finalize(System::block_number());
		Balances::on_finalize(System::block_number());
		System::on_finalize(System::block_number());
		System::set_block_number(System::block_number() + 1);
		System::on_initialize(System::block_number());
		Balances::on_initialize(System::block_number());
		Staking::on_initialize(System::block_number());
	}
}

pub(crate) fn set_author(round: u32, acc: u64, pts: u32) {
	<TotalPoints<Test>>::mutate(round, |p| *p += pts);
	<CollatorPoints<Test>>::mutate(round, acc, |p| *p += pts);
	println!("total point ne {:?}", <TotalPoints<Test>>::get(round));
}
