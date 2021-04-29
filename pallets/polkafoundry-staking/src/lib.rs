#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;
use frame_support::pallet;
#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;
mod taylor_series;

#[pallet]
pub mod pallet {
	use frame_support::{pallet_prelude::*, traits::{Currency, LockIdentifier, ReservableCurrency, CurrencyToVote}};
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::{Saturating, Verify};
	use sp_runtime::{MultiSignature, SaturatedConversion};
	use sp_core::crypto::AccountId32;
	use sp_std::{convert::{From, TryInto}, vec::Vec};
	use frame_support::sp_runtime::traits::{Bounded, AtLeast32BitUnsigned};
	use std::fmt::Debug;
	use std::cell::RefCell;
	use frame_election_provider_support::{ElectionProvider, VoteWeight, Supports, data_provider};
	use std::cmp::Ordering;


	/// Counter for the number of round that have passed
	pub type RoundIndex = u32;
	/// Counter for the number of "reward" points earned by a given collator
	pub type RewardPoint = u32;

	type BalanceOf<T> = <<T as Config>::Currency as Currency<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Overarching event type
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		/// The staking balance
		type Currency: Currency<Self::AccountId> + ReservableCurrency<Self::AccountId>;
		/// Number of block per round
		type BlocksPerRound: Get<u32>;
		/// Number of collators that nominators can be nominated for
		const MAX_COLLATORS_PER_NOMINATOR: u32;
		/// Maximum number of nominations per collator
		type MaxNominationsPerCollator: Get<u32>;
		/// Number of round that staked funds must remain bonded for
		type BondDuration: Get<RoundIndex>;
		/// Minimum stake required to be reserved to be a collator
		type MinCollatorStake: Get<BalanceOf<Self>>;
		/// Minimum stake required to be reserved to be a nominator
		type MinNominatorStake: Get<BalanceOf<Self>>;
		/// Number of round per payout
		type VestingAfter: Get<RoundIndex>;
		/// Something that provides the election functionality.
		type ElectionProvider: frame_election_provider_support::ElectionProvider<
			Self::AccountId,
			Self::BlockNumber,
			// we only accept an election provider that has staking as data provider.
			DataProvider = Module<Self>,
		>;
		/// Convert a balance into a number used for election calculation. This must fit into a `u64`
		/// but is allowed to be sensibly lossy. The `u64` is used to communicate with the
		/// [`sp_npos_elections`] crate which accepts u64 numbers and does operations in 128.
		/// Consequently, the backward convert is used convert the u128s from sp-elections back to a
		/// [`BalanceOf`].
		type CurrencyToVote: CurrencyToVote<BalanceOf<Self>>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_finalize(now: T::BlockNumber) {
			let mut current_round = CurrentRound::<T>::get();
			if current_round.should_goto_next_round(now) {
				current_round.update(now, T::BlocksPerRound::get());
				let round_index = current_round.index;
				CurrentRound::<T>::put(current_round);

				Self::update_collators(round_index);
				Self::update_nominators(round_index);
				Self::execute_exit_queue(round_index);
			}
		}
	}

	/// Just a Balance/BlockNumber tuple to encode when a chunk of funds will be unlocked.
	#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
	pub struct UnlockChunk<Balance> {
		/// Amount of funds to be unlocked.
		pub value: Balance,
		/// Round number at which point it'll be unlocked.
		pub round: RoundIndex,
	}
	/// Just a Balance/BlockNumber tuple to encode when a chunk of funds will be unbonded.
	#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
	pub struct UnBondChunk<Balance> {
		/// Amount of funds to be unbonded.
		pub value: Balance,
		/// Round number at which point it'll be unbonded.
		pub round: RoundIndex,
	}

	#[derive(Default, Clone, Encode, Decode, RuntimeDebug)]
	pub struct Bond<AccountId, Balance>  {
		pub owner: AccountId,
		pub amount: Balance,
	}

	impl<AccountId, Balance> PartialEq for Bond<AccountId, Balance>
		where AccountId: Ord
	{
		fn eq(&self, other: &Self) -> bool {
			self.owner == other.owner
		}
	}

	impl<AccountId, Balance> Eq for Bond<AccountId, Balance>
		where AccountId: Ord
	{}

	impl<AccountId, Balance> PartialOrd for Bond<AccountId, Balance>
		where AccountId: Ord
	{
		fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
			Some(self.cmp(&other))
		}
	}

	impl<AccountId, Balance> Ord for Bond<AccountId, Balance>
		where AccountId: Ord
	{
		fn cmp(&self, other: &Self) -> Ordering {
			self.owner.cmp(&other.owner)
		}
	}


	/// The ledger of a (bonded) stash.
	#[derive(Clone, Encode, Decode, RuntimeDebug)]
	pub struct StakingCollators<AccountId, Balance> {
		/// The total amount of the account's balance that we are currently accounting for.
		/// It's just `active` plus all the `unlocking` plus all the `nomination` balances then minus all the unbonding balances.
		pub total: Balance,
		/// The total amount of the stash's balance that will be at stake in any forthcoming
		/// rounds.
		pub active: Balance,
		/// The total amount of the nomination by nominator
		pub nominations: Vec<Bond<AccountId, Balance>>,
		/// Any balance that is becoming free, which may eventually be transferred out
		/// of the stash (assuming it doesn't get slashed first).
		pub unlocking: Vec<UnlockChunk<Balance>>,
		/// Any balance that is becoming free, which may eventually be transferred out
		/// of the stash (assuming it doesn't get slashed first).
		pub unbonding: Vec<UnBondChunk<Balance>>,
		/// Status of staker
		pub status: StakerStatus,
		/// List of eras for which the stakers behind a validator have claimed rewards. Only updated
		/// for validators.
		pub claimed_rewards: Vec<RoundIndex>,
	}

	impl <AccountId, Balance> StakingCollators<AccountId, Balance>
	where
		AccountId: Ord + Clone,
		Balance: Ord + Copy + Debug + Saturating + AtLeast32BitUnsigned + std::ops::AddAssign + From<u32>
	{
		pub fn new (amount: Balance, next_round: RoundIndex) -> Self {
			StakingCollators {
				total: amount,
				active: 0u32.into(),
				nominations: vec![],
				unlocking: vec![UnlockChunk {
					value: amount,
					round: next_round
				}],
				unbonding: vec![],
				status: StakerStatus::default(),
				claimed_rewards: vec![]
			}
		}

		pub fn is_active(&self) -> bool { self.status == StakerStatus::Active }

		/// Active the onboarding collator
		pub fn active_onboard(&mut self) {
			if self.status == StakerStatus::Onboarding {
				self.status = StakerStatus::Active
			}
		}
		/// Bond extra for collator
		/// Active in next round
		pub fn bond_extra (&mut self, extra: Balance, next_round: RoundIndex) {
			self.total += extra;
			self.unlocking.push(UnlockChunk {
				value: extra,
				round: next_round
			});
		}
		/// Bond less for collator
		/// Unbonding amount delay of `BondDuration` round
		pub fn bond_less (&mut self, less: Balance, can_withdraw_round: RoundIndex) -> Option<Balance> {
			if self.active > less {
				self.active -= less;
				self.unbonding.push(UnBondChunk {
					value: less,
					round: can_withdraw_round
				});

				Some(self.active)
			} else {
				None
			}
		}
		/// Unlocking all the bond be locked in the previous round
		fn consolidate_active(self, current_round: RoundIndex) -> Self {
			let mut active = self.active;
			let unlocking = self.unlocking.into_iter()
				.filter(|chunk| if chunk.round > current_round {
					true
				} else {
					active += chunk.value;
					false
				})
				.collect();

			Self {
				total: self.total,
				active,
				nominations: self.nominations,
				unlocking,
				unbonding: self.unbonding,
				status: self.status,
				claimed_rewards: self.claimed_rewards
			}
		}
		/// Remove all the locked bond after `BondDuration`
		pub fn consolidate_unbonded(self, current_round: RoundIndex) -> Self {
			let mut total = self.total;
			let unbonding = self.unbonding.into_iter()
				.filter(|chunk| if chunk.round > current_round  {
					true
				} else {
					total -= chunk.value;
					false
				})
				.collect();

			Self {
				total,
				active: self.active,
				nominations: self.nominations,
				unlocking: self.unlocking,
				unbonding,
				status: self.status,
				claimed_rewards: self.claimed_rewards
			}
		}

		/// Add nomination for collator
		/// Plus the `active` and `total`
		/// Will be count as vote weight for collator
		pub fn add_nomination(&mut self, nomination: Bond<AccountId, Balance>) -> bool {
			match self.nominations.binary_search(&nomination) {
				Ok(_) => false,
				Err(_) => {
					self.active += nomination.amount;
					self.total += nomination.amount;
					self.nominations.push(nomination);
					true
				}
			}
		}
		/// Nominate extra for exist nomination
		pub fn nominate_extra(&mut self, extra: Bond<AccountId, Balance>) -> Option<Balance> {
			for bond in &mut self.nominations {
				if bond.owner == extra.owner {
					self.active += extra.amount;
					self.total += extra.amount;
					bond.amount += extra.amount;
					return Some(bond.amount)
				}
			}
			None
		}
		/// Nominate less for exist nomination
		pub fn nominate_less(&mut self, less: Bond<AccountId, Balance>) -> Option<Option<Balance>> {
			for bond in &mut self.nominations {
				if bond.owner == less.owner {
					if bond.amount > less.amount {
						self.total -= less.amount;
						self.active -= less.amount;
						bond.amount -= less.amount;
						return Some(Some(bond.amount))
					} else {
						return Some(None)
					}
				}
			}
			None
		}
		/// Active the onboarding collator
		pub fn force_bond(&mut self) {
			self.active = self.total;
			self.unlocking = vec![];
			self.status = StakerStatus::Active
		}

		pub fn rm_nomination(&mut self, nominator: AccountId) -> Option<Balance> {
			let mut less: Option<Balance> = None;
			let nominations = self.nominations.clone()
				.into_iter()
				.filter_map(|n| {
					if n.owner == nominator {
						less = Some(n.amount);
						None
					} else {
						Some(n.clone())
					}
				}
				)
				.collect();
			if let Some(less) = less {
				self.nominations = nominations;
				self.total -= less;
				self.active -= less;
				Some(self.active)
			} else {
				None
			}
		}
	}

	#[derive(Clone, Encode, Decode, RuntimeDebug)]
	pub struct Leaving<Balance> {
		/// The `active` amount of collator before leaving.
		pub remaining: Balance,
		/// Any balance that is becoming free, which may eventually be transferred out
		/// of the stash (assuming it doesn't get slashed first).
		pub unbonding: Vec<UnBondChunk<Balance>>,
		/// Leaving in
		pub when: RoundIndex,
	}

	impl <Balance>Leaving <Balance>
		where Balance: Ord + Copy + Debug + Saturating + AtLeast32BitUnsigned + std::ops::AddAssign + From<u32>
	{
		pub fn new(remaining: Balance, unbonding: Vec<UnBondChunk<Balance>>, when: RoundIndex) -> Self {
			Self {
				remaining,
				unbonding,
				when
			}
		}
	}

	#[derive(Clone, PartialEq, Copy, Encode, Decode, RuntimeDebug)]
	pub enum StakerStatus {
		/// Declared desire in validating or already participating in it.
		Validator,
		/// Nominating for a group of other stakers.
		Nominator,
		/// Ready for produce blocks/nominate.
		Active,
		/// Onboarding to candidates pool in next round
		Onboarding,
		/// Chilling.
		Idle,
		/// Leaving.
		Leaving,
	}

	impl Default for StakerStatus {
		fn default() -> Self {
			StakerStatus::Onboarding
		}
	}

	#[derive(Default, Clone, Encode, Decode, RuntimeDebug)]
	pub struct StakingNominators<AccountId, Balance> {
		pub nominations: Vec<Bond<AccountId, Balance>>,
		/// The total amount of the account's balance that we are currently accounting for.
		pub total: Balance,
		/// Any balance that is becoming free, which may eventually be transferred out
		/// of the stash (assuming it doesn't get slashed first).
		pub unbonding: Vec<UnBondChunk<Balance>>,
		/// List of eras for which the stakers behind a validator have claimed rewards. Only updated
		/// for validators.
		pub claimed_rewards: Vec<RoundIndex>,
	}

	impl <AccountId, Balance> StakingNominators<AccountId, Balance>
		where
			AccountId: Clone + PartialEq + Ord,
			Balance: Copy + Debug + Saturating + AtLeast32BitUnsigned {
		pub fn new (nominations: Vec<Bond<AccountId, Balance>>, amount: Balance) -> Self {
			StakingNominators {
				nominations,
				total: amount,
				unbonding: vec![],
				claimed_rewards: vec![]
			}
		}
		/// Add nomination
		/// Plus `total` will be count as vote weight for nominator
		pub fn add_nomination(&mut self, nomination: Bond<AccountId, Balance>) -> bool {
			match self.nominations.binary_search(&nomination) {
				Ok(_) => false,
				Err(_) => {
					self.total += nomination.amount;
					self.nominations.push(nomination);
					true
				}
			}
		}
		/// Nominate extra for exist nomination
		pub fn nominate_extra(&mut self, extra: Bond<AccountId, Balance>) -> Option<Balance> {
			for nominate in &mut self.nominations {
				if nominate.owner == extra.owner {
					self.total += extra.amount;
					nominate.amount += extra.amount;

					return Some(nominate.amount);
				}
			}
			None
		}
		/// Nominate less for exist nomination
		/// The amount unbond will be locked due to `BondDuration`
		pub fn nominate_less(&mut self, less: Bond<AccountId, Balance>, can_withdraw_round: RoundIndex) -> Option<Option<Balance>> {
			for nominate in &mut self.nominations {
				if nominate.owner == less.owner {
					if nominate.amount > less.amount {
						nominate.amount -= less.amount;
						self.unbonding.push(UnBondChunk {
							value: less.amount,
							round: can_withdraw_round
						});

						return Some(Some(nominate.amount));
					} else {
						return Some(None);
					}
				}
			}
			None
		}
		/// Remove all locked bond after `BondDuration`
		pub fn consolidate_unbonded(self, current_round: RoundIndex) -> Self {
			let mut total = self.total;
			let unbonding = self.unbonding.into_iter()
				.filter(|chunk| if chunk.round > current_round {
					true
				} else {
					total -= chunk.value;
					false
				}).collect();

			Self {
				nominations: self.nominations,
				total,
				unbonding,
				claimed_rewards: self.claimed_rewards
			}
		}

		pub fn rm_nomination(&mut self, candidate: AccountId, can_withdraw_round: RoundIndex) -> Option<Balance> {
			let mut less: Option<Balance> = None;
			let nominations = self.nominations
				.clone()
				.into_iter()
				.filter_map(|n| {
					if n.owner == candidate {
						less = Some(n.amount);
						None
					} else {
						Some(n.clone())
					}
				})
				.collect();
			if let Some(less) = less {
				self.nominations = nominations;
				self.unbonding.push(UnBondChunk {
					value: less,
					round: can_withdraw_round
				});
				Some(self.total)
			} else {
				None
			}
		}
	}

	#[derive(Default, Copy, Clone, PartialEq, Eq, Encode, Decode, RuntimeDebug)]
	pub struct RoundInfo<BlockNumber> {
		/// Index of current round
		index: RoundIndex,
		/// Block where round to be started
		start_in: BlockNumber,
		/// Length of current round
		length: u32
	}

	impl<BlockNumber> RoundInfo<BlockNumber>
	where BlockNumber: PartialOrd
		+ Copy
		+ Debug
		+ From<u32>
		+ std::ops::Add<Output = BlockNumber>
	 	+ std::ops::Sub<Output = BlockNumber>
	{
		pub fn new(index: u32, start_in: BlockNumber, length: u32) -> Self {
			RoundInfo {
				index,
				start_in,
				length
			}
		}
		pub fn next_round_index(&self) -> u32 {
			&self.index + 1u32
		}

		/// New round
		pub fn update(&mut self, now: BlockNumber, length: u32) {
			self.index += 1u32;
			self.start_in = now;
			self.length = length;
		}

		pub fn should_goto_next_round (&self, now: BlockNumber) -> bool {
			now - self.start_in >= self.length.into()
		}

		pub fn next_election_prediction (&self, default_length: u32) -> BlockNumber {
			return if self.index % 2 == 0 {
				self.start_in + self.length.into()
			} else {
				self.start_in + self.length.into() + default_length.into()
			}
		}
	}

	// A value placed in storage that represents the current version of the Staking storage. This value
	// is used by the `on_runtime_upgrade` logic to determine whether we run storage migration logic.
	// This should match directly with the semantic versions of the Rust crate.
	#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, RuntimeDebug)]
	pub enum Releases {
		V1_0_0,
	}

	impl Default for Releases {
		fn default() -> Self {
			Releases::V1_0_0
		}
	}

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub stakers: Vec<(T::AccountId, BalanceOf<T>)>,
	}

	#[cfg(feature = "std")]
	impl <T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			Self {
				stakers: vec![],
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
		fn build(&self) {
			for &(ref staker, balance) in &self.stakers {
				assert!(
					T::Currency::free_balance(&staker) >= balance,
					"Account does not have enough balance to bond."
				);
				Pallet::<T>::bond(
					T::Origin::from(Some(staker.clone()).into()),
					balance.clone(),
				);
			}

			// Start Round 1 at Block 0
			let round: RoundInfo<T::BlockNumber> =
				RoundInfo::new(1u32, 0u32.into(), T::BlocksPerRound::get());
			CurrentRound::<T>::put(round);
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(0)]
		pub fn bond(
			origin: OriginFor<T>,
			amount: BalanceOf<T>
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			ensure!(
				Collators::<T>::get(&who).is_none(),
				Error::<T>::AlreadyBonded
			);

			if amount < T::MinCollatorStake::get() {
				Err(Error::<T>::BondBelowMin)?
			}

			let current_round = CurrentRound::<T>::get();
			let staker = StakingCollators::new(amount, current_round.next_round_index());

			Collators::<T>::insert(&who, staker);

			T::Currency::reserve(
				&who,
				amount,
			);
			Self::deposit_event(Event::Bonded(
				who,
				amount,
			));
			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn force_onboard(
			origin: OriginFor<T>,
			candidate: T::AccountId
		) -> DispatchResultWithPostInfo {
			ensure_root(origin)?;

			let mut collator = Collators::<T>::get(&candidate).ok_or(Error::<T>::CandidateNotExist)?;

			ensure!(
				!collator.is_active(),
				Error::<T>::CandidateNotActive
			);
			collator.force_bond();
			Collators::<T>::insert(&candidate, collator);

			Self::deposit_event(Event::CandidateOnboard(
				candidate,
			));
			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn bond_extra(
			origin: OriginFor<T>,
			extra: BalanceOf<T>
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut collator = Collators::<T>::get(&who).ok_or(Error::<T>::BondNotExist)?;
			ensure!(
				collator.is_active(),
				Error::<T>::CandidateNotActive
			);
			let current_round = CurrentRound::<T>::get();

			collator.bond_extra(extra, current_round.next_round_index());
			Collators::<T>::insert(&who, collator);

			T::Currency::reserve(
				&who,
				extra,
			);

			Self::deposit_event(Event::BondExtra(
				who,
				extra,
			));

			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn bond_less(
			origin: OriginFor<T>,
			less: BalanceOf<T>
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut collator = Collators::<T>::get(&who).ok_or(Error::<T>::BondNotExist)?;
			ensure!(
				collator.is_active(),
				Error::<T>::CandidateNotActive
			);
			let current_round = CurrentRound::<T>::get();
			let after = collator.bond_less(less, current_round.index + T::BondDuration::get()).ok_or(Error::<T>::Underflow)?;

			ensure!(
					after >= T::MinCollatorStake::get(),
					Error::<T>::BondBelowMin
			);

			Collators::<T>::insert(&who, collator);

			Self::deposit_event(Event::BondLess(
				who,
				less,
			));

			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn collator_unbond(
			origin: OriginFor<T>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut collator = Collators::<T>::get(&who).ok_or(Error::<T>::BondNotExist)?;

			let current_round = CurrentRound::<T>::get();
			let when = current_round.index + T::BondDuration::get();

			// leave all nominations
			for nomination in collator.nominations {
				collator.active -= nomination.amount;
				T::Currency::unreserve(&nomination.owner, nomination.amount);
			}

			let exit = Leaving::new(collator.active, collator.unbonding, when);

			ExitQueue::<T>::insert(&who, exit);
			Collators::<T>::remove(&who);

			Self::deposit_event(Event::CandidateLeaving(
				who,
				when,
			));
			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn nominate(
			origin: OriginFor<T>,
			candidate: T::AccountId,
			amount: BalanceOf<T>
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			ensure!(
				amount >= T::MinNominatorStake::get(),
				Error::<T>::NominateBelowMin
			);
			let mut collator = Collators::<T>::get(&candidate).ok_or(Error::<T>::CandidateNotExist)?;
			ensure!(
				collator.is_active(),
				Error::<T>::CandidateNotActive
			);

			ensure!(
					collator.nominations.len() < T::MaxNominationsPerCollator::get() as usize,
					Error::<T>::TooManyNominations
			);

			if let Some(mut nominator) = Nominators::<T>::get(&who) {
				ensure!(
					nominator.add_nomination(Bond {
						owner: candidate.clone(),
						amount,
					}),
					Error::<T>::AlreadyNominatedCollator
				);
				Nominators::<T>::insert(&who, nominator)
			} else {
				let nominator = StakingNominators::new(vec![Bond {
					owner: candidate.clone(), amount
				}], amount);

				Nominators::<T>::insert(&who, nominator)
			}
			ensure!(
					collator.add_nomination(Bond {
						owner: who.clone(),
						amount
					}),
					Error::<T>::NominationNotExist
			);

			Collators::<T>::insert(&candidate, collator);
			T::Currency::reserve(&who, amount);
			Self::deposit_event(Event::Nominate(candidate, amount));

			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn nominate_extra(
			origin: OriginFor<T>,
			candidate: T::AccountId,
			extra: BalanceOf<T>
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut collator = Collators::<T>::get(&candidate).ok_or(Error::<T>::CandidateNotExist)?;
			ensure!(
				collator.is_active(),
				Error::<T>::CandidateNotActive
			);
			let mut nominator = Nominators::<T>::get(&who).ok_or(Error::<T>::NominationNotExist)?;
			nominator.nominate_extra(Bond {
				owner: candidate.clone(),
				amount: extra
			}).ok_or(Error::<T>::CandidateNotExist)?;

			collator.nominate_extra(Bond {
				owner: who.clone(),
				amount: extra
			}).ok_or(Error::<T>::NominationNotExist)?;

			Collators::<T>::insert(&candidate, collator);
			Nominators::<T>::insert(&who, nominator);
			T::Currency::reserve(&who, extra);

			Self::deposit_event(Event::NominateExtra(
				candidate,
				extra,
			));

			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn nominate_less(
			origin: OriginFor<T>,
			candidate: T::AccountId,
			less: BalanceOf<T>
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut collator = Collators::<T>::get(&candidate).ok_or(Error::<T>::CandidateNotExist)?;
			ensure!(
				collator.is_active(),
				Error::<T>::CandidateNotActive
			);
			let mut nominator = Nominators::<T>::get(&who).ok_or(Error::<T>::NominationNotExist)?;
			let current_round = CurrentRound::<T>::get();

			let after = nominator.nominate_less(Bond {
				owner: candidate.clone(),
				amount: less
			}, current_round.index + T::BondDuration::get())
				.ok_or(Error::<T>::CandidateNotExist)?
				.ok_or(Error::<T>::Underflow)?;

			ensure!(
				after >= T::MinNominatorStake::get(),
				Error::<T>::NominateBelowMin
			);

			let after = collator.nominate_less(Bond {
				owner: who.clone(),
				amount: less
			})
				.ok_or(Error::<T>::NominationNotExist)?
				.ok_or(Error::<T>::Underflow)?;

			ensure!(
				after >= T::MinNominatorStake::get(),
				Error::<T>::NominateBelowMin
			);

			Collators::<T>::insert(&candidate, collator);
			Nominators::<T>::insert(&who, nominator);
			Self::deposit_event(Event::NominateLess(
				candidate,
				less,
			));

			Ok(Default::default())
		}
		#[pallet::weight(0)]
		pub fn nominator_leave_collator(
			origin: OriginFor<T>,
			candidate: T::AccountId,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut nomination = Nominators::<T>::get(&who).ok_or(Error::<T>::NominationNotExist)?;
			let mut collator = Collators::<T>::get(&candidate).ok_or(Error::<T>::CandidateNotExist)?;
			let current_round = CurrentRound::<T>::get();

			nomination.rm_nomination(candidate.clone(), current_round.index + T::BondDuration::get())
				.ok_or(Error::<T>::CandidateNotExist)?;

			collator.rm_nomination(who.clone())
				.ok_or(Error::<T>::NominationNotExist)?;

			Collators::<T>::insert(&candidate, collator);
			Nominators::<T>::insert(&who, nomination);
			Self::deposit_event(Event::NominatorLeaveCollator(
				who,
				candidate,
			));
			Ok(Default::default())
		}
	}

	impl <T: Config> Pallet<T> {
		fn enact_election(current_round: RoundIndex) -> Option<Vec<T::AccountId>> {
			T::ElectionProvider::elect()
				.map_err(|e| {
					log!(warn, "election provider failed due to {:?}", e)
				})
				.and_then(|(res, weight)| {
					<frame_system::Pallet<T>>::register_extra_weight_unchecked(
						weight,
						frame_support::weights::DispatchClass::Mandatory,
					);
					Self::process_election(res, current_round)
				})
				.ok()
		}

		pub fn process_election(
			flat_supports: frame_election_provider_support::Supports<T::AccountId>,
			current_era: EraIndex,
		) -> Result<Vec<T::AccountId>, ()> {

		}

		fn update_collators(current_round: RoundIndex) {
			for (acc, mut collator) in  Collators::<T>::iter() {
				// active onboarding collator
				collator.active_onboard();
				// locked bond become active bond
				collator = collator.consolidate_active(current_round.clone());

				let before_total = collator.total;
				// executed unbonding after delay BondDuration
				collator = collator.consolidate_unbonded(current_round.clone());

				T::Currency::unreserve(&acc, before_total - collator.total);
				Collators::<T>::insert(&acc, collator)
			}
		}

		fn update_nominators(current_round: RoundIndex) {
			for (acc, mut nominations) in Nominators::<T>::iter() {
				let before_total = nominations.total;
				// executed unbonding after delay BondDuration
				nominations = nominations.consolidate_unbonded(current_round.clone());

				T::Currency::unreserve(&acc, before_total - nominations.total);

				Nominators::<T>::insert(&acc, nominations)
			}
		}

		fn execute_exit_queue(current_round: RoundIndex) {
			for (acc, mut exit) in ExitQueue::<T>::iter() {
				if exit.when > current_round {
					let unbonding = exit.unbonding.into_iter()
						.filter(|chunk| if chunk.round > current_round {
							true
						} else {
							T::Currency::unreserve(&acc, chunk.value);
							false
						}).collect();

					exit.unbonding = unbonding;
					ExitQueue::<T>::insert(&acc, exit);
				} else {
					T::Currency::unreserve(&acc, exit.remaining);
					for unbond in exit.unbonding {
						T::Currency::unreserve(&acc, unbond.value);
					}
					ExitQueue::<T>::remove(&acc);
				}
			}
		}

		/// The total balance that can be slashed from a stash account as of right now.
		pub fn slashable_balance_of(stash: &T::AccountId, status: StakerStatus) -> BalanceOf<T> {
			// Weight note: consider making the stake accessible through stash.
			match status {
				StakerStatus::Validator => Self::collators(stash).filter(|c| c.is_active()).map(|c| c.active).unwrap_or_default(),
				StakerStatus::Nominator => Self::nominators(stash).map(|l| l.total).unwrap_or_default(),
				_ => Default::default(),
			}
		}

		/// Internal impl of [`Self::slashable_balance_of`] that returns [`VoteWeight`].
		pub fn slashable_balance_of_vote_weight(
			stash: &T::AccountId,
			issuance: BalanceOf<T>,
			status: StakerStatus
		) -> VoteWeight {
			T::CurrencyToVote::to_vote(Self::slashable_balance_of(stash, status), issuance)
		}

		/// Returns a closure around `slashable_balance_of_vote_weight` that can be passed around.
		///
		/// This prevents call sites from repeatedly requesting `total_issuance` from backend. But it is
		/// important to be only used while the total issuance is not changing.
		pub fn slashable_balance_of_fn(status: StakerStatus) -> Box<dyn Fn(&T::AccountId) -> VoteWeight> {
			// NOTE: changing this to unboxed `impl Fn(..)` return type and the module will still
			// compile, while some types in mock fail to resolve.
			let issuance = T::Currency::total_issuance();
			Box::new(move |who: &T::AccountId| -> VoteWeight {
				Self::slashable_balance_of_vote_weight(who, issuance, status)
			})
		}

		/// Get all of the voters that are eligible for the npos election.
		///
		/// This will use all on-chain nominators, and all the validators will inject a self vote.
		///
		/// ### Slashing
		///
		/// All nominations that have been submitted before the last non-zero slash of the validator are
		/// auto-chilled.
		///
		/// Note that this is VERY expensive. Use with care.
		fn get_npos_voters() -> Vec<(T::AccountId, VoteWeight, Vec<T::AccountId>)> {
			let weight_of_validator = Self::slashable_balance_of_fn(StakerStatus::Validator);
			let weight_of_nominator = Self::slashable_balance_of_fn(StakerStatus::Nominator);
			let mut all_voters = Vec::new();

			for (validator, _) in <Collators<T>>::iter() {
				// append self vote
				let self_vote = (validator.clone(), weight_of_validator(&validator), vec![validator.clone()]);
				all_voters.push(self_vote);
			}

			for (nominator, nominations) in Nominators::<T>::iter() {
				let StakingNominators { nominations, .. } = nominations;
				let mut targets = vec![];
				for bond in nominations {
					targets.push(bond.owner.clone())
				}

				let vote_weight = weight_of_nominator(&nominator);
				all_voters.push((nominator, vote_weight, targets))
			}

			all_voters
		}

		pub fn get_npos_targets() -> Vec<T::AccountId> {
			<Collators<T>>::iter().map(|(v, _)| v).collect::<Vec<_>>()
		}
	}

	impl<T: Config> frame_election_provider_support::ElectionDataProvider<T::AccountId, T::BlockNumber>
	for Pallet<T>
	{
		const MAXIMUM_VOTES_PER_VOTER: u32 = T::MAX_COLLATORS_PER_NOMINATOR;

		fn targets(maybe_max_len: Option<usize>) -> data_provider::Result<(Vec<T::AccountId>, Weight)> {
			let target_count = <Collators<T>>::iter().filter(|c| c.1.is_active()).count();

			if maybe_max_len.map_or(false, |max_len| target_count > max_len) {
				return Err("Target snapshot too big");
			}

			let weight = <T as frame_system::Config>::DbWeight::get().reads(target_count as u64);
			Ok((Self::get_npos_targets(), weight))
		}

		fn voters(maybe_max_len: Option<usize>) -> data_provider::Result<(Vec<(T::AccountId, VoteWeight, Vec<T::AccountId>)>, Weight)> {
			let nominator_count = Nominators::<T>::iter().count();
			let validator_count = <Collators<T>>::iter().filter(|c| c.1.is_active()).count();
			let voter_count = nominator_count.saturating_add(validator_count);

			if maybe_max_len.map_or(false, |max_len| voter_count > max_len) {
				return Err("Voter snapshot too big");
			}
			let weight = <T as frame_system::Config>::DbWeight::get().reads(voter_count as u64);

			Ok((Self::get_npos_voters(), weight))
		}

		fn desired_targets() -> data_provider::Result<(u32, Weight)> {
			Ok((10u32, <T as frame_system::Config>::DbWeight::get().reads(1)))
		}

		fn next_election_prediction(_: T::BlockNumber) -> T::BlockNumber {
			let current_round = Self::current_round();
			current_round.next_election_prediction(T::BlocksPerRound::get())
		}
	}

	#[pallet::storage]
	#[pallet::getter(fn current_round)]
	pub type CurrentRound<T: Config> =
	StorageValue<_, RoundInfo<T::BlockNumber>, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn collators)]
	pub type Collators<T: Config> =
	StorageMap<_, Twox64Concat, T::AccountId, StakingCollators<T::AccountId, BalanceOf<T>>>;

	#[pallet::storage]
	#[pallet::getter(fn exit_queue)]
	pub type ExitQueue<T: Config> =
	StorageMap<_, Twox64Concat, T::AccountId, Leaving<BalanceOf<T>>>;

	#[pallet::storage]
	#[pallet::getter(fn nominators)]
	pub type Nominators<T: Config> =
	StorageMap<_, Twox64Concat, T::AccountId, StakingNominators<T::AccountId, BalanceOf<T>>>;

	#[pallet::storage]
	#[pallet::getter(fn storage_version)]
	pub type StorageVersion<T: Config> =
	StorageValue<_, Releases, ValueQuery>;


	#[pallet::error]
	pub enum Error<T> {
		/// Candidate already bonded
		AlreadyBonded,
		/// Candidate already in queue
		AlreadyInQueue,
		/// Bond not exist
		BondNotExist,
		/// Value under flow
		Underflow,
		/// Bond less than minimum value
		BondBelowMin,
		/// Bond less than minimum value
		NominateBelowMin,
		/// Nominate not exist candidate
		CandidateNotExist,
		/// Too many candidates supplied
		TooManyCandidates,
		/// Nomination not exist
		NominationNotExist,
		/// Already nominated collator
		AlreadyNominatedCollator,
		/// Too many nomination candidates supplied
		TooManyNominations,
		/// Candidate not active
		CandidateNotActive,
		/// Candidate is leaving
		AlreadyLeaving,
	}

	#[pallet::event]
	#[pallet::generate_deposit(fn deposit_event)]
	pub enum Event<T: Config> {
		Bonded(T::AccountId, BalanceOf<T>),
		BondExtra(T::AccountId, BalanceOf<T>),
		BondLess(T::AccountId, BalanceOf<T>),
		Nominate(T::AccountId, BalanceOf<T>),
		NominateExtra(T::AccountId, BalanceOf<T>),
		NominateLess(T::AccountId, BalanceOf<T>),
		CandidateOnboard(T::AccountId),
		CandidateLeaving(T::AccountId, RoundIndex),
		NominatorLeaveCollator(T::AccountId, T::AccountId),
	}
}
