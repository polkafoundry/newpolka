#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;
use frame_support::pallet;

#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod tests;

#[pallet]
pub mod pallet {
	use frame_support::{
		dispatch::fmt::Debug,
		pallet_prelude::*,
		traits::{Currency, ExistenceRequirement::AllowDeath, IsType},
		PalletId,
	};
	use frame_system::pallet_prelude::*;
	use sp_runtime::{traits::{AccountIdConversion, CheckedSub, Saturating, Zero}, SaturatedConversion, Perbill};
	use sp_std::{
		convert::{From, TryInto},
		vec::Vec,
		ops::{Add, Sub},
	};

	#[pallet::config]
	pub trait Config: frame_system::Config + cumulus_pallet_parachain_system::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The crowdloan's module id, used for deriving its sovereign account ID.
		type PalletId: Get<PalletId>;

		/// The reward balance.
		type RewardCurrency: Currency<Self::AccountId>;
	}

	pub type BalanceOf<T> = <<T as Config>::RewardCurrency as Currency<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	pub type BlockNumberOf<T> = <T as frame_system::Config>::BlockNumber;
	pub type AccountIdOf<T> = <T as frame_system::Config>::AccountId;

	#[derive(Default, Clone, Encode, Decode, RuntimeDebug)]
	pub struct RewardInfo<T: Config> {
		pub total_reward: BalanceOf<T>, // Total of rewarded token
		pub init_locked: BalanceOf<T>, // The initialize locked token = total_reward - Distributed Token at TGE
		pub claimed_reward: BalanceOf<T>,
		pub last_paid: BlockNumberOf<T>,
	}

	#[derive(Default, PartialEq, Eq, Copy, Clone, Encode, Decode, RuntimeDebug)]
	pub struct Setting<BlockNumber> {
		pub tge_rate: u32, // Percentage rates of token at Token generating event (TGE)
		pub reward_start_block: BlockNumber,
		pub reward_end_block: BlockNumber,
	}

	impl<BlockNumber> Setting<BlockNumber> where
		BlockNumber: PartialOrd
		+ Copy
		+ Debug
		+ Add<Output = BlockNumber>
		+ Sub<Output = BlockNumber>
		+ From<u32>,
	{
		pub fn new(tge_rate: u32, reward_start_block: BlockNumber, reward_end_block: BlockNumber) -> Self {
			Setting {
				tge_rate,
				reward_start_block,
				reward_end_block,
			}
		}

		pub fn update_tge_rate(&mut self, tge_rate: u32) {
			self.tge_rate = tge_rate;
		}

		pub fn update_lock_duration(&mut self, start_block: BlockNumber, end_block: BlockNumber) {
			self.reward_start_block = start_block;
			self.reward_end_block = end_block;
		}

		pub fn reward_period(&self) -> BlockNumber {
			self.reward_end_block - self.reward_start_block
		}
	}

	#[pallet::pallet]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(0)]
		pub fn initialize_reward(
			origin: OriginFor<T>,
			contributions: Vec<(AccountIdOf<T>, BalanceOf<T>)>,
		) -> DispatchResultWithPostInfo {
			ensure_root(origin)?;

			let relay_block_now = BlockNumberOf::<T>::from(cumulus_pallet_parachain_system::Pallet::<T>::validation_data()
				.expect("set_validation_data inherent needs to be present in every block!")
				.relay_parent_number);
			let setting = Settings::<T>::get();
			ensure!(InitRewardAt::<T>::get().is_zero(), Error::<T>::AlreadyInitReward);

			let total_reward_amount: BalanceOf<T> = contributions
				.iter()
				.fold(0u32.into(), |reward: BalanceOf<T>, (_, amount)| {
				reward + *amount
			});
			ensure!(Self::pot() >= total_reward_amount, Error::<T>::InsufficientFunds);

			for (who, amount) in &contributions {
				let total_reward = *amount;
				let claimed_reward = Perbill::from_percent(setting.tge_rate).mul_floor(total_reward);
				let init_locked = total_reward.saturating_sub(claimed_reward);

				// A part of token are distributed immediately at TGE.
				T::RewardCurrency::transfer(&Self::account_id(), &who, claimed_reward, AllowDeath)
					.map_err(|_| Error::<T>::RewardFailed)?;
				Self::deposit_event(Event::RewardPaid(who.clone(), claimed_reward));

				// The remaining are distributed linearly until end block
				Contributors::<T>::insert(
					who,
					RewardInfo {
						total_reward,
						init_locked,
						claimed_reward,
						last_paid: relay_block_now.clone(),
					},
				);
			}

			InitRewardAt::<T>::put(relay_block_now);
			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn claim(
			origin: OriginFor<T>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			let mut info = Contributors::<T>::get(&who)
				.ok_or(Error::<T>::NotContributedYet)?;
			let setting = Settings::<T>::get();
			let relay_block_now = BlockNumberOf::<T>::from(cumulus_pallet_parachain_system::Pallet::<T>::validation_data()
				.expect("set_validation_data inherent needs to be present in every block!")
				.relay_parent_number);

			ensure!(relay_block_now >= setting.reward_start_block, Error::<T>::ClaimInLockedTime);
			ensure!(
				info.total_reward > info.claimed_reward,
				Error::<T>::AlreadyPaid
			);

			let reward_period = setting.reward_period()
				.saturated_into::<u128>()
				.try_into()
				.ok()
				.ok_or(Error::<T>::WrongConversionU128ToBalance)?;

			ensure!(
				reward_period > Zero::zero(),
				Error::<T>::NotReady,
			);

			let last_paid = if info.last_paid < setting.reward_start_block {
				setting.reward_start_block
			} else {
				info.last_paid
			};

			let reward_per_block = info.init_locked / reward_period;
			let reward_period = relay_block_now.saturating_sub(last_paid);

			let reward_period_as_balance: BalanceOf<T> = reward_period
				.saturated_into::<u128>()
				.try_into()
				.ok()
				.ok_or(Error::<T>::WrongConversionU128ToBalance)?;

			let amount = if reward_per_block.saturating_mul(reward_period_as_balance)
				> info.total_reward - info.claimed_reward {
				info.total_reward - info.claimed_reward
			} else {
				reward_per_block.saturating_mul(reward_period_as_balance)
			};
			info.last_paid = relay_block_now;
			info.claimed_reward = info.claimed_reward.saturating_add(amount);
			Contributors::<T>::insert(&who, info);

			ensure!(
				amount >= T::RewardCurrency::minimum_balance(),
				Error::<T>::ClaimAmountBelowMinimum
			);

			T::RewardCurrency::transfer(&Self::account_id(), &who, amount, AllowDeath)
				.map_err(|_| Error::<T>::RewardFailed)?;

			Self::deposit_event(Event::RewardPaid(who, amount));
			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn config(origin: OriginFor<T>, setting: Setting<BlockNumberOf<T>>) -> DispatchResultWithPostInfo {
			ensure_root(origin)?;
			let mut current_setting = Settings::<T>::get();

			if setting.tge_rate > 0 && setting.tge_rate <= 100 {
				current_setting.update_tge_rate(setting.tge_rate);
			}

			if setting.reward_start_block > Zero::zero() && setting.reward_end_block > setting.reward_start_block {
				current_setting.update_lock_duration(setting.reward_start_block, setting.reward_end_block);
			}

			Settings::<T>::put(current_setting);
			Self::deposit_event(Event::SettingChanged(current_setting.clone()));
			Ok(Default::default())
		}

		#[pallet::weight(0)]
		pub fn distribute_all(origin: OriginFor<T>) -> DispatchResultWithPostInfo {
			ensure_root(origin)?;
			let setting = Settings::<T>::get();
			let relay_block_now = BlockNumberOf::<T>::from(cumulus_pallet_parachain_system::Pallet::<T>::validation_data()
				.expect("set_validation_data inherent needs to be present in every block!")
				.relay_parent_number);
			ensure!(relay_block_now >= setting.reward_end_block, Error::<T>::DistributeNotReady);

			let total_remaining_reward_amount = Contributors::<T>::iter_values()
				.fold(0u32.into(), |reward: BalanceOf<T>, t| {
					let amount = if t.total_reward > t.claimed_reward {
						t.total_reward - t.claimed_reward
					} else {
						0u32.into()
					};

					reward + amount
				});
			ensure!(Self::pot() >= total_remaining_reward_amount, Error::<T>::InsufficientFunds);

			Contributors::<T>::iter().for_each(|(who, mut info)| {
				let amount = if info.total_reward > info.claimed_reward {
					info.total_reward - info.claimed_reward
				} else {
					0u32.into()
				};

				// avoid transfer 0 amount of tokens to users
				if amount < T::RewardCurrency::minimum_balance() { return (); }

				match T::RewardCurrency::transfer(&Self::account_id(), &who, amount, AllowDeath) {
					Ok(_) => {
						info.last_paid = relay_block_now;
						info.claimed_reward = info.claimed_reward.saturating_add(amount);
						Contributors::<T>::insert(&who, info);
						Self::deposit_event(Event::RewardPaid(who.clone(), amount))
					},
					Err(_) => (),
				}
			});

			Ok(Default::default())
		}
	}

	#[pallet::storage]
	#[pallet::getter(fn contributors)]
	pub type Contributors<T: Config> =
	StorageMap<_, Blake2_128Concat, AccountIdOf<T>, RewardInfo<T>>;

	#[pallet::storage]
	#[pallet::getter(fn settings)]
	pub type Settings<T: Config> = StorageValue<_, Setting<BlockNumberOf<T>>, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn init_reward_at)]
	pub type InitRewardAt<T: Config> = StorageValue<_, BlockNumberOf<T>, ValueQuery>;

	#[pallet::error]
	pub enum Error<T> {
		/// The claim is not ready
		NotReady,
		/// Already init a reward
		AlreadyInitReward,
		/// Already paid all reward
		AlreadyPaid,
		/// User not contribute for the crowdloan
		NotContributedYet,
		/// Invalid conversion while calculating payable amount
		WrongConversionU128ToBalance,
		/// User cannot receive a reward
		RewardFailed,
		/// The amount of claim below the minimum balance
		ClaimAmountBelowMinimum,
		/// Cannot claim in locked time
		ClaimInLockedTime,
		/// The total reward amount exceed the pallet's fund
		InsufficientFunds,
		/// Cannot distribute token during vesting period
		DistributeNotReady,
	}

	#[pallet::event]
	#[pallet::generate_deposit(fn deposit_event)]
	pub enum Event<T: Config> {
		RewardPaid(AccountIdOf<T>, BalanceOf<T>),
		SettingChanged(Setting<BlockNumberOf<T>>),
		FundDeposited(BalanceOf<T>),
	}

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub start_block: BlockNumberOf<T>,
		pub end_block: BlockNumberOf<T>,
		pub tge_rate: u32,
	}

	#[cfg(feature = "std")]
	impl <T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			Self {
				start_block: <T as frame_system::Config>::BlockNumber::zero(),
				end_block: <T as frame_system::Config>::BlockNumber::zero(),
				tge_rate: 0u32,
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig<T> {
		fn build(&self) {
			let setting: Setting<BlockNumberOf<T>> = Setting::new(self.tge_rate, self.start_block, self.end_block);
			Settings::<T>::put(setting);
			Pallet::<T>::deposit_event(Event::SettingChanged(setting.clone()));
			Pallet::<T>::deposit_event(Event::FundDeposited(Pallet::<T>::pot()));
		}
	}

	#[pallet::extra_constants]
	impl<T: Config> Pallet<T> {
		/// The account ID of the pallet.
		///
		/// This actually does computation. If you need to keep using it, then make sure you cache the
		/// value and only call this once.
		pub fn account_id() -> AccountIdOf<T> {
			T::PalletId::get().into_account()
		}

		pub fn pot() -> BalanceOf<T> {
			T::RewardCurrency::free_balance(&Self::account_id())
				.checked_sub(&T::RewardCurrency::minimum_balance()).unwrap_or_else(Zero::zero)
		}
	}
}

