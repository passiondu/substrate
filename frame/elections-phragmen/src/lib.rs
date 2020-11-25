// This file is part of Substrate.

// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Phragmén Election Module.
//!
//! An election module based on sequential phragmen.
//!
//! ### Term and Round
//!
//! The election happens in _rounds_: every `N` blocks, all previous members are retired and a new
//! set is elected (which may or may not have an intersection with the previous set). Each round
//! lasts for some number of blocks defined by [`TermDuration`]. The words _term_ and _round_ can be
//! used interchangeably in this context.
//!
//! [`TermDuration`] might change during a round. This can shorten or extend the length of the
//! round. The next election round's block number is never stored but rather always checked on the
//! fly. Based on the current block number and [`TermDuration`], the condition `BlockNumber %
//! TermDuration == 0` being satisfied will always trigger a new election round.
//!
//! ### Bonds and Deposits
//!
//! Both voting and being a candidate requires deposits to be taken, in exchange for the data that
//! needs to be kept on-chain, and the complexity forced to the election. The terms *bond* and
//! *deposit* can be used interchangeably in this context.
//!
//! Bonds will be unreserved only upon adhering to the protocol laws. Failing to do so will cause in
//! the bond to slashed.
//!
//! ### Voting
//!
//! Voters can vote for any number of the candidates by providing a list of account ids. Invalid
//! votes (voting for non-candidates) and duplicate votes are ignored during election. Yet, a voter
//! _might_ vote for a future candidate. Voters reserve a bond as they vote. Each vote defines a
//! `value`. This amount is locked from the account of the voter and indicates the weight of the
//! vote. Voters can update their votes at any time by calling `vote()` again. This keeps the bond
//! untouched but can optionally change the locked `value`. After a round, votes are kept and might
//! still be valid for further rounds. A voter is responsible for calling `remove_voter` once they
//! are done to have their bond back and remove the lock.
//!
//! ### Defunct Voter
//! A voter is defunct once all of the candidates that they have voted for are not a valid candidate
//! (as seen further below, members and runners-up are also always candidates). Defunct voters can
//! be removed via a root call. Upon being removed, their bond is returned. This is an
//! administrative operation and can be called only by the root origin in the case of state blaot.
//!
//! ### Candidacy and Members
//!
//! Candidates also reserve a bond as they submit candidacy. A candidate can end up in one of the
//! below situations:
//!   - **Members**: A winner is kept as a _member_. They must still have a bond in reserve and they
//!     are automatically counted as a candidate for the next election.
//!   - **Runner-up**: Runners-up are the best candidates immediately after the winners. The number
//!     of runners_up to keep is configurable. Runners-up are used, in order that they are elected,
//!     as replacements when a candidate is kicked by [`remove_member`], or when an active member
//!     renounces their candidacy. Runners are automatically counted as a candidate for the next
//!     election.
//!   - **Loser**: Any of the candidate who are not a winner are left as losers. A loser might be an
//!     _outgoing member or runner_, meaning that they are an active member who failed to keep their
//!     spot. **An outgoing will always lose their bond**.
//!
//! ##### Renouncing candidacy.
//!
//! All candidates, elected or not, can renounce their candidacy. A call to [`renounce_candidacy`]
//! will always cause the candidacy bond to be refunded.
//!
//! Note that with the members being the default candidates for the next round and votes persisting
//! in storage, the election system is entirely stable given no further input. This means that if
//! the system has a particular set of candidates `C` and voters `V` that lead to a set of members
//! `M` being elected, as long as `V` and `C` don't remove their candidacy and votes, `M` will keep
//! being re-elected at the end of each round.
//!
//! ### Module Information
//!
//! - [`election_sp_phragmen::Trait`](./trait.Trait.html)
//! - [`Call`](./enum.Call.html)
//! - [`Module`](./struct.Module.html)

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use frame_support::{
	decl_error, decl_event, decl_module, decl_storage,
	dispatch::{DispatchResultWithPostInfo, WithPostDispatchInfo},
	ensure,
	storage::{IterableStorageMap, StorageMap},
	traits::{
		ChangeMembers, Contains, ContainsLengthBound, Currency, CurrencyToVote, Get,
		InitializeMembers, LockIdentifier, LockableCurrency, OnUnbalanced, ReservableCurrency,
		WithdrawReasons,
	},
	weights::Weight,
};
use frame_system::{ensure_root, ensure_signed};
use sp_npos_elections::{ElectionResult, ExtendedBalance};
use sp_runtime::{
	traits::{Saturating, StaticLookup, Zero},
	DispatchError, Perbill, RuntimeDebug,
};
use sp_std::{prelude::*, cmp::Ordering};

mod benchmarking;
pub mod weights;
pub use weights::WeightInfo;

/// The maximum votes allowed per voter.
pub const MAXIMUM_VOTE: usize = 16;

type BalanceOf<T> =
	<<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;
type NegativeImbalanceOf<T> =
	<<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::NegativeImbalance;

/// Helper functions for migrations of this module.
pub mod migrations {
	use super::*;
	use frame_support::{migration::StorageKeyIterator, Twox64Concat};

	/// Migrate from the old legacy voting bond (fixed) to the new one (per-vote dynamic).
	///
	/// Will only be triggered if storage version is V1.
	pub fn migrate_voters_to_recorded_deposit<T: Trait>(old_deposit: BalanceOf<T>) -> Weight {
		if <Module<T>>::pallet_storage_version() == StorageVersion::V1 {
			let mut count = 0;
			<StorageKeyIterator<T::AccountId, (BalanceOf<T>, Vec<T::AccountId>), Twox64Concat>>::new(
				<Voting<T>>::module_prefix(),
				b"Voting",
			)
			.for_each(|(who, (stake, votes))| {
				// Insert a new value into the same location, thus no need to do `.drain()`.
				let deposit = old_deposit;
				let voter = Voter { votes, stake, deposit };
				<Voting<T>>::insert(who, voter);
				count += 1;
			});

			PalletStorageVersion::put(StorageVersion::V2RecordedDeposit);
			frame_support::debug::info!(
				"🏛 pallet-elections-phragmen: {} voters migrated to V2PerVoterDeposit.",
				count,
			);
			Weight::max_value()
		} else {
			frame_support::debug::warn!(
				"🏛 pallet-elections-phragmen: Tried to run migration but PalletStorageVersion is \
				updated to V2. This code probably needs to be removed now.",
			);
			0
		}
	}

	/// Migrate all candidates to recorded deposit.
	///
	/// Will only be triggered if storage version is V1.
	pub fn migrate_candidates_to_recorded_deposit<T: Trait>(old_deposit: BalanceOf<T>) -> Weight {
		if <Module<T>>::pallet_storage_version() == StorageVersion::V1 {
			let old_candidates = frame_support::migration::take_storage_value::<Vec<T::AccountId>>(
				<Voting<T>>::module_prefix(),
				b"Candidates",
				&[],
			)
			.unwrap_or_default();
			let new_candidates = old_candidates
				.into_iter()
				.map(|c| (c, old_deposit))
				.collect::<Vec<_>>();
			<Candidates<T>>::put(new_candidates);
			Weight::max_value()
		} else {
			frame_support::debug::warn!(
				"🏛 pallet-elections-phragmen: Tried to run migration but PalletStorageVersion is \
				updated. This code probably needs to be removed now.",
			);
			0
		}
	}

	pub fn migrate_members_to_recorded_deposit<T: Trait>(deposit: BalanceOf<T>) -> Weight {
		if <Module<T>>::pallet_storage_version() == StorageVersion::V1 {
			let _ = <Members<T>>::translate::<Vec<(T::AccountId, BalanceOf<T>)>, _>(
				|maybe_old_members| {
					maybe_old_members.map(|old_members| {
						frame_support::debug::info!(
							"migrated {} member accounts.",
							old_members.len()
						);
						old_members
							.into_iter()
							.map(|(who, stake)| SeatHolder {
								who,
								stake,
								deposit,
							})
							.collect::<Vec<_>>()
					})
				},
			);
			Weight::max_value()
		} else {
			frame_support::debug::warn!(
				"🏛 pallet-elections-phragmen: Tried to run migration but PalletStorageVersion is \
				updated. This code probably needs to be removed now.",
			);
			0
		}
	}

	pub fn migrate_runners_up_to_recorded_deposit<T: Trait>(deposit: BalanceOf<T>) -> Weight {
		if <Module<T>>::pallet_storage_version() == StorageVersion::V1 {
			let _ = <RunnersUp<T>>::translate::<Vec<(T::AccountId, BalanceOf<T>)>, _>(
				|maybe_old_runners_up| {
					maybe_old_runners_up.map(|old_runners_up| {
						frame_support::debug::info!(
							"migrated {} runner-up accounts.",
							old_runners_up.len()
						);
						old_runners_up
							.into_iter()
							.map(|(who, stake)| SeatHolder {
								who,
								stake,
								deposit,
							})
							.collect::<Vec<_>>()
					})
				},
			);
			Weight::max_value()
		} else {
			frame_support::debug::warn!(
				"🏛 pallet-elections-phragmen: Tried to run migration but PalletStorageVersion is \
				updated. This code probably needs to be removed now.",
			);
			0
		}
	}
}

/// An indication that the renouncing account currently has which of the below roles.
#[derive(Encode, Decode, Clone, PartialEq, RuntimeDebug)]
pub enum Renouncing {
	/// A member is renouncing.
	Member,
	/// A runner-up is renouncing.
	RunnerUp,
	/// A candidate is renouncing, while the given total number of candidates exists.
	Candidate(#[codec(compact)] u32),
}

/// An active voter.
#[derive(Encode, Decode, Clone, Default, RuntimeDebug, PartialEq)]
pub struct Voter<AccountId, Balance> {
	/// The members being backed.
	pub votes: Vec<AccountId>,
	/// The amount of stake placed on this vote.
	pub stake: Balance,
	/// The amount of deposit reserved for this vote.
	///
	/// To be unreserved upon removal.
	pub deposit: Balance,
}

/// A holder of a seat as either a member or a runner-up.
#[derive(Encode, Decode, Clone, Default, RuntimeDebug, PartialEq)]
pub struct SeatHolder<AccountId, Balance> {
	/// The holder.
	who: AccountId,
	/// The total backing stake.
	stake: Balance,
	/// The amount of deposit held.s
	deposit: Balance,
}

/// Storage version.
#[derive(Encode, Decode, Eq, PartialEq)]
pub enum StorageVersion {
	/// Initial version.
	V1,
	/// After moving to per-vote deposit.
	V2RecordedDeposit,
}

pub trait Trait: frame_system::Trait {
	/// The overarching event type.c
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;

	/// Identifier for the elections-phragmen pallet's lock
	type ModuleId: Get<LockIdentifier>;

	/// The currency that people are electing with.
	type Currency:
		LockableCurrency<Self::AccountId, Moment=Self::BlockNumber> +
		ReservableCurrency<Self::AccountId>;

	/// What to do when the members change.
	type ChangeMembers: ChangeMembers<Self::AccountId>;

	/// What to do with genesis members
	type InitializeMembers: InitializeMembers<Self::AccountId>;

	/// Convert a balance into a number used for election calculation.
	/// This must fit into a `u64` but is allowed to be sensibly lossy.
	type CurrencyToVote: CurrencyToVote<BalanceOf<Self>>;

	/// How much should be locked up in order to submit one's candidacy.
	type CandidacyBond: Get<BalanceOf<Self>>;

	/// Base deposit associated with voting.
	///
	/// This should be sensibly high to economically ensure the pallet cannot be attacked by
	/// creating a gigantic number of votes.
	type VotingBondBase: Get<BalanceOf<Self>>;

	/// The amount of bond that need to be locked for each vote (32 bytes).
	type VotingBondFactor: Get<BalanceOf<Self>>;

	/// Handler for the unbalanced reduction when a candidate has lost (and is not a runner-up)
	type LoserCandidate: OnUnbalanced<NegativeImbalanceOf<Self>>;

	/// Handler for the unbalanced reduction when a member has been kicked.
	type KickedMember: OnUnbalanced<NegativeImbalanceOf<Self>>;

	/// Number of members to elect.
	type DesiredMembers: Get<u32>;

	/// Number of runners_up to keep.
	type DesiredRunnersUp: Get<u32>;

	/// How long each seat is kept. This defines the next block number at which an election
	/// round will happen. If set to zero, no elections are ever triggered and the module will
	/// be in passive mode.
	type TermDuration: Get<Self::BlockNumber>;

	/// Weight information for extrinsics in this pallet.
	type WeightInfo: WeightInfo;
}

decl_storage! {
	trait Store for Module<T: Trait> as PhragmenElection {
		// ---- State
		/// The current elected members.
		///
		/// Invariant: Always sorted based on account id.
		pub Members get(fn members): Vec<SeatHolder<T::AccountId, BalanceOf<T>>>;

		/// The current reserved runners-up.
		///
		/// Invariant: Always sorted based on rank (worse to best). Upon removal of a member, the
		/// last (i.e. _best_) runner-up will be replaced.
		pub RunnersUp get(fn runners_up): Vec<SeatHolder<T::AccountId, BalanceOf<T>>>;

		/// The total number of vote rounds that have happened, excluding the upcoming one.
		pub ElectionRounds get(fn election_rounds): u32 = Zero::zero();

		/// Votes and locked stake of a particular voter.
		///
		/// TWOX-NOTE: SAFE as `AccountId` is a crypto hash.
		pub Voting get(fn voting): map hasher(twox_64_concat) T::AccountId => Voter<T::AccountId, BalanceOf<T>>;

		/// The present candidate list. A current member or runner-up
		/// can never enter this vector and is always implicitly assumed to be a candidate.
		///
		/// Second element is the deposit.
		///
		/// Invariant: Always sorted based on account id.
		pub Candidates get(fn candidates): Vec<(T::AccountId, BalanceOf<T>)>;

		/// Should be used in conjunction with `on_runtime_upgrade` to ensure an upgrade is executed
		/// once, even if the code is not removed in time.
		/// TODO: use basti's new version stuff?
		pub PalletStorageVersion get(fn pallet_storage_version)
			build(|_| StorageVersion::V2RecordedDeposit): StorageVersion = StorageVersion::V1;

	} add_extra_genesis {
		config(members): Vec<(T::AccountId, BalanceOf<T>)>;
		build(|config: &GenesisConfig<T>| {
			let members = config.members.iter().map(|(ref member, ref stake)| {
				// make sure they have enough stake.
				assert!(
					T::Currency::free_balance(member) >= *stake,
					"Genesis member does not have enough stake.",
				);

				// reserve candidacy bond and set as members.
				T::Currency::reserve(&member, T::CandidacyBond::get())
					.expect("Genesis member does not have enough balance to be a candidate");

				// Note: all members will only vote for themselves, hence they must be given exactly
				// their own stake as total backing. Any sane election should behave as such.
				// Nonetheless, stakes will be updated for term 1 onwards according to the election.
				Members::<T>::mutate(|members| {
					match members.binary_search_by(|m| m.who.cmp(member)) {
						Ok(_) => panic!("Duplicate member in elections-phragmen genesis: {}", member),
						Err(pos) => members.insert(
							pos,
							SeatHolder { who: member.clone(), stake: *stake, deposit: T::CandidacyBond::get() },
						),
					}
				});

				// set self-votes to make persistent.
				<Module<T>>::vote(
					T::Origin::from(Some(member.clone()).into()),
					vec![member.clone()],
					*stake,
				).expect("Genesis member could not vote.");

				member.clone()
			}).collect::<Vec<T::AccountId>>();

			// report genesis members to upstream, if any.
			T::InitializeMembers::initialize_members(&members);
		})
	}
}

decl_error! {
	pub enum Error for Module<T: Trait> {
		/// Cannot vote when no candidates or members exist.
		UnableToVote,
		/// Must vote for at least one candidate.
		NoVotes,
		/// Cannot vote more than candidates.
		TooManyVotes,
		/// Cannot vote more than maximum allowed.
		MaximumVotesExceeded,
		/// Cannot vote with stake less than minimum balance.
		LowBalance,
		/// Voter can not pay voting bond.
		UnableToPayBond,
		/// Must be a voter.
		MustBeVoter,
		/// Cannot report self.
		ReportSelf,
		/// Duplicated candidate submission.
		DuplicatedCandidate,
		/// Member cannot re-submit candidacy.
		MemberSubmit,
		/// Runner cannot re-submit candidacy.
		RunnerUpSubmit,
		/// Candidate does not have enough funds.
		InsufficientCandidateFunds,
		/// Not a member.
		NotMember,
		/// The provided count of number of candidates is incorrect.
		InvalidCandidateCount,
		/// The provided count of number of votes is incorrect.
		InvalidVoteCount,
		/// The renouncing origin presented a wrong `Renouncing` parameter.
		InvalidRenouncing,
		/// Prediction regarding replacement after member removal is wrong.
		InvalidReplacement,
	}
}

decl_event!(
	pub enum Event<T> where
		Balance = BalanceOf<T>,
		<T as frame_system::Trait>::AccountId,
	{
		/// A new term with \[new_members\]. This indicates that enough candidates existed to run the
		/// election, not that enough have has been elected. The inner value must be examined for
		/// this purpose. A `NewTerm(\[\])` indicates that some candidates got their bond slashed and
		/// none were elected, whilst `EmptyTerm` means that no candidates existed to begin with.
		NewTerm(Vec<(AccountId, Balance)>),
		/// No (or not enough) candidates existed for this round. This is different from
		/// `NewTerm(\[\])`. See the description of `NewTerm`.
		EmptyTerm,
		/// Internal error happened while trying to perform election.
		ElectionError,
		/// A \[member\] has been removed. This should always be followed by either `NewTerm` or
		/// `EmptyTerm`.
		MemberKicked(AccountId),
		/// A \[member\] has renounced their candidacy.
		MemberRenounced(AccountId),
		/// A \[candidate\] was slashed by \[amount\] due to failing to obtain a seat as member or
		/// runner-up.
		///
		/// Note that old members and runners-up are also candidates.
		CandidateSlashed(AccountId, Balance),
		/// A \[seat holder\] was slashed by \[amount\] by being forcefully removed from the set.
		SeatHolderSlashed(AccountId, Balance),
	}
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		type Error = Error<T>;
		fn deposit_event() = default;

		const CandidacyBond: BalanceOf<T> = T::CandidacyBond::get();
		const VotingBondBase: BalanceOf<T> = T::VotingBondBase::get();
		const VotingBondFactor: BalanceOf<T> = T::VotingBondFactor::get();
		const DesiredMembers: u32 = T::DesiredMembers::get();
		const DesiredRunnersUp: u32 = T::DesiredRunnersUp::get();
		const TermDuration: T::BlockNumber = T::TermDuration::get();
		const ModuleId: LockIdentifier = T::ModuleId::get();

		/// Vote for a set of candidates for the upcoming round of election. This can be called to
		/// set the initial votes, or update already existing votes.
		///
		/// Upon initial voting, `value` units of `who`'s balance is locked and a deposit amount is
		/// reserved. The deposit is based on the number of votes and can be updated over time.
		///
		/// The `votes` should:
		///   - not be empty.
		///   - be less than the number of possible candidates. Note that all current members and
		///     runners-up are also automatically candidates for the next round.
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ### Warning
		///
		/// It is the responsibility of the caller to **NOT** place all of their balance into the
		/// lock and keep some for further transactions.
		///
		/// # <weight>
		/// we consider the common case of placing more votes. In other two case, we refund.
		/// # </weight>
		#[weight = T::WeightInfo::vote_more(votes.len() as u32)]
		fn vote(
			origin,
			votes: Vec<T::AccountId>,
			#[compact] value: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;

			// votes should not be empty and more than `MAXIMUM_VOTE` in any case.
			ensure!(votes.len() <= MAXIMUM_VOTE, Error::<T>::MaximumVotesExceeded);
			ensure!(!votes.is_empty(), Error::<T>::NoVotes);

			let candidates_count = <Candidates<T>>::decode_len().unwrap_or(0);
			let members_count = <Members<T>>::decode_len().unwrap_or(0);
			let runners_up_count = <RunnersUp<T>>::decode_len().unwrap_or(0);

			// can never submit a vote of there are no members, and cannot submit more votes than
			// all potential vote targets.
			// addition is valid: candidates, members and runners-up will never overlap.
			let allowed_votes = candidates_count + members_count + runners_up_count;
			ensure!(!allowed_votes.is_zero(), Error::<T>::UnableToVote);
			ensure!(votes.len() <= allowed_votes, Error::<T>::TooManyVotes);

			ensure!(value > T::Currency::minimum_balance(), Error::<T>::LowBalance);

			// Reserve bond.
			let new_deposit = Self::deposit_of(votes.len());
			let Voter { deposit: old_deposit, .. } = <Voting<T>>::get(&who);
			let maybe_refund = match new_deposit.cmp(&old_deposit) {
				Ordering::Greater => {
					// Must reserve a bit more.
					let to_reserve = new_deposit - old_deposit;
					T::Currency::reserve(&who, to_reserve)
						.map_err(|_| Error::<T>::UnableToPayBond)?;
					None
				},
				Ordering::Equal => {
					Some(T::WeightInfo::vote_equal(votes.len() as u32))
				},
				Ordering::Less => {
					// Must unreserve a bit.
					let to_unreserve = old_deposit - new_deposit;
					let _remainder = T::Currency::unreserve(&who, to_unreserve);
					debug_assert!(_remainder.is_zero());
					Some(T::WeightInfo::vote_less(votes.len() as u32))
				},
			};

			// Amount to be locked up.
			let locked_stake = value.min(T::Currency::total_balance(&who));

			// lock
			T::Currency::set_lock(
				T::ModuleId::get(),
				&who,
				locked_stake,
				WithdrawReasons::except(WithdrawReasons::TRANSACTION_PAYMENT),
			);

			Voting::<T>::insert(&who, Voter { votes, deposit: new_deposit, stake: locked_stake });
			Ok(maybe_refund.into())
		}

		/// Remove `origin` as a voter. This removes the lock and returns the deposit.
		///
		/// The dispatch origin of this call must be signed and be a voter.
		#[weight = T::WeightInfo::remove_voter()]
		fn remove_voter(origin) {
			let who = ensure_signed(origin)?;
			ensure!(Self::is_voter(&who), Error::<T>::MustBeVoter);

			Self::do_remove_voter(&who);
		}

		/// Submit oneself for candidacy. A fixed amount of deposit is recorded.
		///
		/// All candidates are wiped at the end of the term. They either become a member/runner-up,
		/// or leave the system while their deposit is slashed.
		///
		/// The dispatch origin of this call must be signed.
		///
		/// ### Warning
		///
		/// Even if a candidate ends up being a member, they must call renounce_candidacy to get
		/// their deposit back. Losing the spot in an election will also lead to a slash.
		///
		/// # <weight>
		/// The number of current candidates must be provided as witness data.
		/// # </weight>
		#[weight = T::WeightInfo::submit_candidacy(*candidate_count)]
		fn submit_candidacy(origin, #[compact] candidate_count: u32) {
			let who = ensure_signed(origin)?;

			let actual_count = <Candidates<T>>::decode_len().unwrap_or(0);
			ensure!(
				actual_count as u32 <= candidate_count,
				Error::<T>::InvalidCandidateCount,
			);

			let is_candidate = Self::is_candidate(&who);
			ensure!(is_candidate.is_err(), Error::<T>::DuplicatedCandidate);

			// assured to be an error, error always contains the index.
			let index = is_candidate.unwrap_err();

			ensure!(!Self::is_member(&who), Error::<T>::MemberSubmit);
			ensure!(!Self::is_runner_up(&who), Error::<T>::RunnerUpSubmit);

			T::Currency::reserve(&who, T::CandidacyBond::get())
				.map_err(|_| Error::<T>::InsufficientCandidateFunds)?;

			<Candidates<T>>::mutate(|c| c.insert(index, (who, T::CandidacyBond::get())));
		}

		/// Renounce one's intention to be a candidate for the next election round. 3 potential
		/// outcomes exist:
		///
		/// - `origin` is a candidate and not elected in any set. In this case, the deposit is
		///   unreserved, returned and origin is removed as a candidate.
		/// - `origin` is a current runner-up. In this case, the deposit is unreserved, returned and
		///   origin is removed as a runner-up.
		/// - `origin` is a current member. In this case, the deposit is unreserved and origin is
		///   removed as a member, consequently not being a candidate for the next round anymore.
		///   Similar to [`remove_voter`], if replacement runners exists, they are immediately used.
		///   If the prime is renouncing, then no prime will exist until the next round.
		///
		/// The dispatch origin of this call must be signed, and have one of the above roles.
		///
		/// # <weight>
		/// The type of renouncing must be provided as witness data.
		/// # </weight>
		#[weight =  match *renouncing {
			Renouncing::Candidate(count) => T::WeightInfo::renounce_candidacy_candidate(count),
			Renouncing::Member => T::WeightInfo::renounce_candidacy_members(),
			Renouncing::RunnerUp => T::WeightInfo::renounce_candidacy_runners_up(),
		}]
		fn renounce_candidacy(origin, renouncing: Renouncing) {
			let who = ensure_signed(origin)?;
			match renouncing {
				Renouncing::Member => {
					let _ = Self::remove_and_replace_member(&who, false)?;
					Self::deposit_event(RawEvent::MemberRenounced(who));
				},
				Renouncing::RunnerUp => {
					let mut runners_up = Self::runners_up();
					if let Some(index) = runners_up
						.iter()
						.position(|SeatHolder { who: r, .. }| r == &who)
					{
						let SeatHolder { deposit, .. } = runners_up.remove(index);
						T::Currency::unreserve(&who, deposit);
						<RunnersUp<T>>::put(runners_up);
					} else {
						Err(Error::<T>::InvalidRenouncing)?;
					}
				}
				Renouncing::Candidate(count) => {
					let mut candidates = Self::candidates();
					ensure!(count >= candidates.len() as u32, Error::<T>::InvalidRenouncing);
					// Note: candidates list is always sorted.
					if let Ok(index) = candidates.binary_search_by(|(c, _)| c.cmp(&who)) {
						let (_removed, deposit) = candidates.remove(index);
						T::Currency::unreserve(&who, deposit);
						<Candidates<T>>::put(candidates);
					} else {
						Err(Error::<T>::InvalidRenouncing)?;
					}
				}
			};
		}

		/// Remove a particular member from the set. This is effective immediately and the bond of
		/// the outgoing member is slashed.
		///
		/// If a runner-up is available, then the best runner-up will be removed and replaces the
		/// outgoing member. Otherwise, a new phragmen election is started.
		///
		/// The dispatch origin of this call must be root.
		///
		/// Note that this does not affect the designated block number of the next election.
		///
		/// # <weight>
		/// If we have a replacement, we use a small weight. Else, since this is a root call and
		/// will go into phragmen, we assume full block for now.
		/// # </weight>
		#[weight = if *has_replacement {
			T::WeightInfo::remove_member_with_replacement()
		} else {
			T::MaximumBlockWeight::get()
		}]
		fn remove_member(
			origin,
			who: <T::Lookup as StaticLookup>::Source,
			has_replacement: bool,
		) -> DispatchResultWithPostInfo {
			ensure_root(origin)?;
			let who = T::Lookup::lookup(who)?;

			let will_have_replacement = <RunnersUp<T>>::decode_len().unwrap_or(0) > 0;
			if will_have_replacement != has_replacement {
				// In both cases, we will change more weight than need. Refund and abort.
				return Err(Error::<T>::InvalidReplacement.with_weight(
					// refund. The weight value comes from a benchmark which is special to this.
					//  5.751 µs
					T::WeightInfo::remove_member_wrong_refund()
				));
			} // else, prediction was correct.

			let had_replacement = Self::remove_and_replace_member(&who, true)?;
			Self::deposit_event(RawEvent::MemberKicked(who.clone()));

			if !had_replacement {
				// if we end up here, we will charge a full block weight.
				Self::do_phragmen();
			}

			// no refund needed.
			Ok(None.into())
		}

		/// Clean all voters who are defunct (i.e. the do not serve any purpose at all). The deposit
		/// of the removed voters are returned.
		///
		/// This is an root function to be used only for cleaning the state.
		///
		/// The dispatch origin of this call must be root.
		///
		/// # <weight>
		/// The total number of voters and those that are defunct must be provided as witness data.
		/// # </weight>
		#[weight = 0]
		fn clean_defunct_voters(origin, num_voters: u32, num_defunct: u32) {
			let _ = ensure_root(origin)?;
			<Voting<T>>::iter()
				.filter(|(_, x)| Self::is_defunct_voter(&x.votes))
				.for_each(|(dv, _)| {
					Self::do_remove_voter(&dv)
				})
		}

		/// What to do at the end of each block.
		///
		/// Checks if an election needs to happen or not.
		fn on_initialize(n: T::BlockNumber) -> Weight {
			if !Self::term_duration().is_zero() && (n % Self::term_duration()).is_zero() {
				Self::do_phragmen()
			} else {
				0
			}
		}
	}
}

impl<T: Trait> Module<T> {
	/// The deposit value of `count` votes.
	fn deposit_of(count: usize) -> BalanceOf<T> {
		T::VotingBondBase::get().saturating_add(
			T::VotingBondFactor::get().saturating_mul((count as u32).into())
		)
	}

	/// Attempts to remove a member `who`. If a runner-up exists, it is used as the replacement.
	///
	/// Returns:
	///
	/// - `Ok(true)` if the member was removed and a replacement was found.
	/// - `Ok(false)` if the member was removed and but no replacement was found.
	/// - `Err(_)` if the member was no found.
	///
	/// Both `Members` and `RunnersUp` storage is updated accordingly. `T::ChangeMember` is called
	/// if needed. If `slash` is true, the deposit of the potentially removed member is slashed,
	/// else it is unreserved.
	///
	/// ### Note
	///
	/// This function attempts to preserve the prime. If the removed members is not the prime, it is
	/// set again via [`Trait::ChangeMembers` ] to prevent it being wiped.
	fn remove_and_replace_member(who: &T::AccountId, slash: bool) -> Result<bool, DispatchError> {
		// closure will return:
		// - `Ok(Option(replacement))` if member was removed and replacement was replaced.
		// - `Ok(None)` if member was removed but no replacement was found
		// - `Err(_)` if who is not a member.
		<Members<T>>::try_mutate(|members| {
			// we remove the member anyhow, regardless of having a runner-up or not.
			// TODO: a way to make sure all tests operate in a way that accounts are not sorted.
			let remove_index = members
				// note: members are always sorted.
				.binary_search_by(|m| m.who.cmp(who))
				.map_err(|_| Error::<T>::NotMember)?;
			let removed = members.remove(remove_index);
			if slash {
				let (imbalance, _remaining) = T::Currency::slash_reserved(who, removed.deposit);
				debug_assert_eq!(_remaining, 0u32.into());
				T::LoserCandidate::on_unbalanced(imbalance);
				Self::deposit_event(RawEvent::SeatHolderSlashed(who.clone(), removed.deposit));
			} else {
				T::Currency::unreserve(who, removed.deposit);
			}

			Ok(<RunnersUp<T>>::mutate(|r| r.pop()).map(|next_best| {
				// invariant: Members and runners-up are disjoint. This will always be err and give
				// us an index to insert.
				members
					.binary_search_by(|m| m.who.cmp(&next_best.who))
					.err()
					.map(|index| {
						members.insert(index, next_best.clone());
					});
				next_best
			}))
		})
		.map(|maybe_replacement| {
			let member_ids = Self::members()
				.into_iter()
				.map(|x| x.who.clone())
				.collect::<Vec<_>>();
			let outgoing = &[who.clone()];
			let maybe_current_prime = T::ChangeMembers::get_prime();
			let return_value = match maybe_replacement {
				// member ids are already sorted, other two elements have one item.
				Some(incoming) => {
					T::ChangeMembers::change_members_sorted(
						&[incoming.who],
						outgoing,
						&member_ids[..],
					);
					true
				}
				None => {
					T::ChangeMembers::change_members_sorted(&[], outgoing, &member_ids[..]);
					false
				}
			};

			// if there was a prime before and they are not the one being removed, then set them
			// again.
			if let Some(current_prime) = maybe_current_prime {
				if &current_prime != who {
					T::ChangeMembers::set_prime(Some(current_prime));
				}
			}

			return_value
		})
	}

	/// Check if `who` is a candidate. It returns the insert index if the element does not exists as
	/// an error.
	///
	/// O(LogN) given N candidates.
	fn is_candidate(who: &T::AccountId) -> Result<(), usize> {
		Self::candidates()
			.binary_search_by(|c| c.0.cmp(who))
			.map(|_| ())
	}

	/// Check if `who` is a voter. It may or may not be a _current_ one.
	///
	/// State: O(1).
	fn is_voter(who: &T::AccountId) -> bool {
		Voting::<T>::contains_key(who)
	}

	/// Check if `who` is currently an active member.
	///
	/// O(LogN) given N members. Since members are limited, O(1).
	fn is_member(who: &T::AccountId) -> bool {
		Self::members().binary_search_by(|m| m.who.cmp(who)).is_ok()
	}

	/// Check if `who` is currently an active runner-up.
	///
	/// O(N) given N runners-up. Since runners-up are limited, O(1).
	fn is_runner_up(who: &T::AccountId) -> bool {
		Self::runners_up()
			.iter()
			.position(|r| &r.who == who)
			.is_some()
	}

	/// Returns number of desired members.
	fn desired_members() -> u32 {
		T::DesiredMembers::get()
	}

	/// Returns number of desired runners up.
	fn desired_runners_up() -> u32 {
		T::DesiredRunnersUp::get()
	}

	/// Returns the term duration
	fn term_duration() -> T::BlockNumber {
		T::TermDuration::get()
	}

	/// Get the members' account ids.
	fn members_ids() -> Vec<T::AccountId> {
		Self::members()
			.into_iter()
			.map(|m| m.who)
			.collect::<Vec<T::AccountId>>()
	}

	/// Get a concatenation of previous members and runners-up and their deposits.
	///
	/// These accounts are essentially treated as candidates.
	fn implicit_candidates() -> Vec<(T::AccountId, BalanceOf<T>)> {
		// invariant: these two are always without duplicates.
		Self::members()
			.into_iter()
			.map(|m| (m.who, m.deposit))
			.chain(Self::runners_up().into_iter().map(|r| (r.who, r.deposit)))
			.collect::<Vec<_>>()
	}

	/// Check if `votes` will correspond to a defunct voter. As no origin is part of the inputs,
	/// this function does not check the origin at all.
	///
	/// O(NLogM) with M candidates and `who` having voted for `N` of them.
	/// Reads Members, RunnersUp, Candidates and Voting(who) from database.
	fn is_defunct_voter(votes: &[T::AccountId]) -> bool {
		votes.iter().all(|v|
			!Self::is_member(v) &&
			!Self::is_runner_up(v) &&
			!Self::is_candidate(v).is_ok()
		)
	}

	/// Remove a certain someone as a voter.
	fn do_remove_voter(who: &T::AccountId) {
		let Voter { deposit, .. } = <Voting<T>>::take(who);

		// remove storage, lock and unreserve.
		T::Currency::remove_lock(T::ModuleId::get(), who);

		let _remainder = T::Currency::unreserve(who, deposit);
		debug_assert!(_remainder.is_zero());
	}

	/// Run the phragmen election with all required side processes and state updates, if election
	/// succeeds. Else, it will emit an `ElectionError` event.
	///
	/// Calls the appropriate [`ChangeMembers`] function variant internally.
	///
	/// Reads: O(C + V*E) where C = candidates, V voters and E votes per voter exits.
	/// Writes: O(M + R) with M desired members and R runners_up.
	fn do_phragmen() -> Weight {
		let desired_seats = Self::desired_members() as usize;
		let desired_runners_up = Self::desired_runners_up() as usize;
		let num_to_elect = desired_runners_up + desired_seats;

		let mut candidates_and_deposit = Self::candidates();
		// add all the previous members and runners-up as candidates as well.
		candidates_and_deposit.append(&mut Self::implicit_candidates());

		if candidates_and_deposit.len().is_zero() {
			Self::deposit_event(RawEvent::EmptyTerm);
			return T::DbWeight::get().reads(5);
		}

		// All of the new winners that come out of phragmen will thus have a deposit recorded.
		let candidate_ids = candidates_and_deposit
			.iter()
			.map(|(x, _)| x)
			.cloned()
			.collect::<Vec<_>>();

		// helper closures to deal with balance/stake.
		let total_issuance = T::Currency::total_issuance();
		let to_votes = |b: BalanceOf<T>| T::CurrencyToVote::to_vote(b, total_issuance);
		let to_balance = |e: ExtendedBalance| T::CurrencyToVote::to_currency(e, total_issuance);

		let mut num_edges: u32 = 0;
		// used for prime election.
		let voters_and_stakes = Voting::<T>::iter()
			.map(|(voter, Voter { stake, votes, .. })| { (voter, stake, votes) })
			.collect::<Vec<_>>();
		// used for phragmen.
		let voters_and_votes = voters_and_stakes.iter()
			.cloned()
			.map(|(voter, stake, votes)| {
				num_edges = num_edges.saturating_add(votes.len() as u32);
				(voter, to_votes(stake), votes)
			})
			.collect::<Vec<_>>();

		let weight_candidates = candidates_and_deposit.len() as u32;
		let weight_voters = voters_and_votes.len() as u32;
		let weight_edges = num_edges;
		let _ = sp_npos_elections::seq_phragmen::<T::AccountId, Perbill>(
			num_to_elect,
			candidate_ids,
			voters_and_votes.clone(),
			None,
		)
		.map(
			|ElectionResult {
			     winners,
			     assignments: _,
			 }| {
				// this is already sorted by id.
				let old_members_ids_sorted = <Members<T>>::take()
					.into_iter()
					.map(|m| m.who)
					.collect::<Vec<T::AccountId>>();
				// this one needs a sort by id.
				let mut old_runners_up_ids_sorted = <RunnersUp<T>>::take()
					.into_iter()
					.map(|r| r.who)
					.collect::<Vec<T::AccountId>>();
				old_runners_up_ids_sorted.sort();

				// filter out those who end up with no backing stake. This is a logic specific to this
				// module.
				let new_set_with_stake = winners
					.into_iter()
					.filter_map(|(m, b)| {
						if b.is_zero() {
							None
						} else {
							Some((m, to_balance(b)))
						}
					})
					.collect::<Vec<(T::AccountId, BalanceOf<T>)>>();

				// OPTIMISATION NOTE: we could bail out here if `new_set.len() == 0`. There isn't much
				// left to do. Yet, re-arranging the code would require duplicating the slashing of
				// exposed candidates, cleaning any previous members, and so on. For now, in favour of
				// readability and veracity, we keep it simple.

				// split new set into winners and runners up.
				let split_point = desired_seats.min(new_set_with_stake.len());

				let mut new_members_sorted_by_id = (&new_set_with_stake[..split_point]).to_vec();
				new_members_sorted_by_id.sort_by(|i, j| i.0.cmp(&j.0));

				let new_runners_up_sorted_by_rank = (&new_set_with_stake[split_point..])
					.to_vec()
					.into_iter()
					.rev()
					.collect::<Vec<(T::AccountId, BalanceOf<T>)>>();
				let mut new_runners_up_ids_sorted = new_runners_up_sorted_by_rank
					.iter()
					.map(|(r, _)| r.clone())
					.collect::<Vec<T::AccountId>>();
				new_runners_up_ids_sorted.sort();

				// Now we select a prime member using a [Borda count](https://en.wikipedia.org/wiki/Borda_count). We
				// weigh everyone's vote for that new member by a multiplier based on the order of the votes. i.e. the
				// first person a voter votes for gets a 16x multiplier, the next person gets a 15x multiplier, an so
				// on... (assuming `MAXIMUM_VOTE` = 16)
				let mut prime_votes = new_members_sorted_by_id
					.iter()
					.map(|c| (&c.0, BalanceOf::<T>::zero()))
					.collect::<Vec<_>>();
				for (_, stake, votes) in voters_and_stakes.into_iter() {
					for (vote_multiplier, who) in votes
						.iter()
						.enumerate()
						.map(|(vote_position, who)| ((MAXIMUM_VOTE - vote_position) as u32, who))
					{
						if let Ok(i) = prime_votes.binary_search_by_key(&who, |k| k.0) {
							prime_votes[i].1 = prime_votes[i]
								.1
								.saturating_add(stake.saturating_mul(vote_multiplier.into()));
						}
					}
				}

				// We then select the new member with the highest weighted stake. In the case of
				// a tie, the last person in the list with the tied score is selected. This is
				// the person with the "highest" account id based on the sort above.
				let prime = prime_votes
					.into_iter()
					.max_by_key(|x| x.1)
					.map(|x| x.0.clone());

				// new_members_sorted_by_id is sorted by account id.
				let new_members_ids_sorted = new_members_sorted_by_id
					.iter()
					.map(|(m, _)| m.clone())
					.collect::<Vec<T::AccountId>>();

				// report member changes. We compute diff because we need the outgoing list.
				let (incoming, outgoing) = T::ChangeMembers::compute_members_diff_sorted(
					&new_members_ids_sorted,
					&old_members_ids_sorted,
				);
				T::ChangeMembers::change_members_sorted(
					&incoming,
					&outgoing,
					&new_members_ids_sorted,
				);
				T::ChangeMembers::set_prime(prime);

				// All candidates/members/runners-up who are no longer retaining a position as a seat
				// holder will lose their bond.
				candidates_and_deposit.iter().for_each(|(c, d)| {
					// any candidate who is not a member and not a runner up.
					if
						new_members_ids_sorted.binary_search(c).is_err() &&
						new_runners_up_ids_sorted.binary_search(c).is_err()
					{
						let (imbalance, _) = T::Currency::slash_reserved(c, *d);
						T::LoserCandidate::on_unbalanced(imbalance);
						Self::deposit_event(RawEvent::CandidateSlashed(c.clone(), *d));
					}
				});

				// write final values to storage.
				let deposit_of_candidate = |x: &T::AccountId| -> BalanceOf<T> {
					// defensive-only. This closure is used against the new members and new runners-up,
					// both of which are phragmen winners and thus must have deposit.
					candidates_and_deposit
						.iter()
						.find_map(|(c, d)| if c == x { Some(*d) } else { None })
						.unwrap_or_default()
				};
				// fetch deposits from the one recorded one. This will make sure that a candidate who
				// submitted candidacy before a change to candidacy deposit will have the correct amount
				// recorded.
				<Members<T>>::put(
					new_members_sorted_by_id
						.iter()
						.map(|(who, stake)| SeatHolder {
							deposit: deposit_of_candidate(&who),
							who: who.clone(),
							stake: stake.clone(),
						})
						.collect::<Vec<_>>(),
				);
				<RunnersUp<T>>::put(
					new_runners_up_sorted_by_rank
						.into_iter()
						.map(|(who, stake)| SeatHolder {
							deposit: deposit_of_candidate(&who),
							who,
							stake,
						})
						.collect::<Vec<_>>(),
				);

				// clean candidates.
				<Candidates<T>>::kill();

				Self::deposit_event(RawEvent::NewTerm(new_members_sorted_by_id));
				ElectionRounds::mutate(|v| *v += 1);
			},
		)
		.map_err(|e| {
			frame_support::debug::error!("elections-phragmen: failed to run election [{:?}].", e);
			Self::deposit_event(RawEvent::ElectionError);
		});

		T::WeightInfo::election_phragmen(weight_candidates, weight_voters, weight_edges)
	}
}

impl<T: Trait> Contains<T::AccountId> for Module<T> {
	fn contains(who: &T::AccountId) -> bool {
		Self::is_member(who)
	}
	fn sorted_members() -> Vec<T::AccountId> { Self::members_ids() }

	// A special function to populate members in this pallet for passing Origin
	// checks in runtime benchmarking.
	#[cfg(feature = "runtime-benchmarks")]
	fn add(who: &T::AccountId) {
		Members::<T>::mutate(|members| {
			match members.binary_search_by(|m| m.who.cmp(who)) {
				Ok(_) => (),
				Err(pos) => members.insert(pos, SeatHolder { who: who.clone(), ..Default::default() }),
			}
		})
	}
}

impl<T: Trait> ContainsLengthBound for Module<T> {
	fn min_len() -> usize { 0 }

	/// Implementation uses a parameter type so calling is cost-free.
	fn max_len() -> usize {
		Self::desired_members() as usize
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_support::{assert_ok, assert_noop, assert_err_with_weight, parameter_types,
		traits::OnInitialize,
		weights::Weight,
	};
	use substrate_test_utils::assert_eq_uvec;
	use sp_core::H256;
	use sp_runtime::{
		Perbill, testing::Header, BuildStorage, DispatchResult,
		traits::{BlakeTwo256, IdentityLookup, Block as BlockT},
	};
	use crate as elections_phragmen;

	parameter_types! {
		pub const BlockHashCount: u64 = 250;
		pub const MaximumBlockWeight: Weight = 1024;
		pub const MaximumBlockLength: u32 = 2 * 1024;
		pub const AvailableBlockRatio: Perbill = Perbill::one();
	}

	impl frame_system::Trait for Test {
		type BaseCallFilter = ();
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Call = Call;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = Event;
		type BlockHashCount = BlockHashCount;
		type MaximumBlockWeight = MaximumBlockWeight;
		type DbWeight = ();
		type BlockExecutionWeight = ();
		type ExtrinsicBaseWeight = ();
		type MaximumExtrinsicWeight = MaximumBlockWeight;
		type MaximumBlockLength = MaximumBlockLength;
		type AvailableBlockRatio = AvailableBlockRatio;
		type Version = ();
		type PalletInfo = ();
		type AccountData = pallet_balances::AccountData<u64>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
	}

	parameter_types! {
		pub const ExistentialDeposit: u64 = 1;
	}

	impl pallet_balances::Trait for Test {
		type Balance = u64;
		type Event = Event;
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = frame_system::Module<Test>;
		type MaxLocks = ();
		type WeightInfo = ();
	}

	frame_support::parameter_types! {
		pub static VotingBondBase: u64 = 2;
		pub static VotingBondFactor: u64 = 0;
		pub static CandidacyBond: u64 = 3;
		pub static DesiredMembers: u32 = 2;
		pub static DesiredRunnersUp: u32 = 0;
		pub static TermDuration: u64 = 5;
		pub static Members: Vec<u64> = vec![];
		pub static Prime: Option<u64> = None;
	}

	pub struct TestChangeMembers;
	impl ChangeMembers<u64> for TestChangeMembers {
		fn change_members_sorted(incoming: &[u64], outgoing: &[u64], new: &[u64]) {
			// new, incoming, outgoing must be sorted.
			let mut new_sorted = new.to_vec();
			new_sorted.sort();
			assert_eq!(new, &new_sorted[..]);

			let mut incoming_sorted = incoming.to_vec();
			incoming_sorted.sort();
			assert_eq!(incoming, &incoming_sorted[..]);

			let mut outgoing_sorted = outgoing.to_vec();
			outgoing_sorted.sort();
			assert_eq!(outgoing, &outgoing_sorted[..]);

			// incoming and outgoing must be disjoint
			for x in incoming.iter() {
				assert!(outgoing.binary_search(x).is_err());
			}

			let mut old_plus_incoming = MEMBERS.with(|m| m.borrow().to_vec());
			old_plus_incoming.extend_from_slice(incoming);
			old_plus_incoming.sort();

			let mut new_plus_outgoing = new.to_vec();
			new_plus_outgoing.extend_from_slice(outgoing);
			new_plus_outgoing.sort();

			assert_eq!(old_plus_incoming, new_plus_outgoing, "change members call is incorrect!");

			MEMBERS.with(|m| *m.borrow_mut() = new.to_vec());
			PRIME.with(|p| *p.borrow_mut() = None);
		}

		fn set_prime(who: Option<u64>) {
			PRIME.with(|p| *p.borrow_mut() = who);
		}

		fn get_prime() -> Option<u64> {
			PRIME.with(|p| *p.borrow())
		}
	}

	parameter_types! {
		pub const ElectionsPhragmenModuleId: LockIdentifier = *b"phrelect";
	}

	impl Trait for Test {
		type ModuleId = ElectionsPhragmenModuleId;
		type Event = Event;
		type Currency = Balances;
		type CurrencyToVote = frame_support::traits::SaturatingCurrencyToVote;
		type ChangeMembers = TestChangeMembers;
		type InitializeMembers = ();
		type CandidacyBond = CandidacyBond;
		type VotingBondBase = VotingBondBase;
		type VotingBondFactor = VotingBondFactor;
		type TermDuration = TermDuration;
		type DesiredMembers = DesiredMembers;
		type DesiredRunnersUp = DesiredRunnersUp;
		type LoserCandidate = ();
		type KickedMember = ();
		type WeightInfo = ();
	}

	pub type Block = sp_runtime::generic::Block<Header, UncheckedExtrinsic>;
	pub type UncheckedExtrinsic = sp_runtime::generic::UncheckedExtrinsic<u32, u64, Call, ()>;

	frame_support::construct_runtime!(
		pub enum Test where
			Block = Block,
			NodeBlock = Block,
			UncheckedExtrinsic = UncheckedExtrinsic
		{
			System: frame_system::{Module, Call, Event<T>},
			Balances: pallet_balances::{Module, Call, Event<T>, Config<T>},
			Elections: elections_phragmen::{Module, Call, Event<T>, Config<T>},
		}
	);

	pub struct ExtBuilder {
		balance_factor: u64,
		genesis_members: Vec<(u64, u64)>,
	}

	impl Default for ExtBuilder {
		fn default() -> Self {
			Self {
				balance_factor: 1,
				genesis_members: vec![],
			}
		}
	}

	impl ExtBuilder {
		pub fn voter_bond(self, bond: u64) -> Self {
			VOTING_BOND_BASE.with(|v| *v.borrow_mut() = bond);
			self
		}
		pub fn voter_bond_factor(self, bond: u64) -> Self {
			VOTING_BOND_FACTOR.with(|v| *v.borrow_mut() = bond);
			self
		}
		pub fn desired_runners_up(self, count: u32) -> Self {
			DESIRED_RUNNERS_UP.with(|v| *v.borrow_mut() = count);
			self
		}
		pub fn term_duration(self, duration: u64) -> Self {
			TERM_DURATION.with(|v| *v.borrow_mut() = duration);
			self
		}
		pub fn genesis_members(mut self, members: Vec<(u64, u64)>) -> Self {
			MEMBERS.with(|m| {
				*m.borrow_mut() = members
					.iter()
					.map(|(m, _)| m.clone())
					.collect::<Vec<_>>()
			});
			self.genesis_members = members;
			self
		}
		pub fn desired_members(self, count: u32) -> Self {
			DESIRED_MEMBERS.with(|m| *m.borrow_mut() = count);
			self
		}
		pub fn balance_factor(mut self, factor: u64) -> Self {
			self.balance_factor = factor;
			self
		}
		pub fn build_and_execute(self, test: impl FnOnce() -> ()) {
			MEMBERS.with(|m| *m.borrow_mut() = self.genesis_members.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>());
			let mut ext: sp_io::TestExternalities = GenesisConfig {
				pallet_balances: Some(pallet_balances::GenesisConfig::<Test>{
					balances: vec![
						(1, 10 * self.balance_factor),
						(2, 20 * self.balance_factor),
						(3, 30 * self.balance_factor),
						(4, 40 * self.balance_factor),
						(5, 50 * self.balance_factor),
						(6, 60 * self.balance_factor)
					],
				}),
				elections_phragmen: Some(elections_phragmen::GenesisConfig::<Test> {
					members: self.genesis_members
				}),
			}.build_storage().unwrap().into();
			ext.execute_with(pre_conditions);
			ext.execute_with(test);
			ext.execute_with(post_conditions)
		}
	}

	fn candidate_ids() -> Vec<u64> {
		Elections::candidates()
			.into_iter()
			.map(|(c, _)| c)
			.collect::<Vec<_>>()
	}

	fn candidate_deposit(who: &u64) -> u64 {
		Elections::candidates().into_iter().find_map(|(c, d)| if c == *who { Some(d) } else { None }).unwrap_or_default()
	}

	fn voter_deposit(who: &u64) -> u64 {
		Elections::voting(who).deposit
	}

	fn runners_up_ids() -> Vec<u64> {
		Elections::runners_up()
			.into_iter()
			.map(|r| r.who)
			.collect::<Vec<_>>()
	}

	fn members_and_stake() -> Vec<(u64, u64)> {
		Elections::members()
			.into_iter()
			.map(|m| (m.who, m.stake))
			.collect::<Vec<_>>()
	}

	fn runners_up_and_stake() -> Vec<(u64, u64)> {
		Elections::runners_up()
			.into_iter()
			.map(|r| (r.who, r.stake))
			.collect::<Vec<_>>()
	}

	fn all_voters() -> Vec<u64> {
		Voting::<Test>::iter().map(|(v, _)| v).collect::<Vec<u64>>()
	}

	fn balances(who: &u64) -> (u64, u64) {
		(Balances::free_balance(who), Balances::reserved_balance(who))
	}

	fn has_lock(who: &u64) -> u64 {
		let lock = Balances::locks(who)[0].clone();
		assert_eq!(lock.id, ElectionsPhragmenModuleId::get());
		lock.amount
	}

	fn intersects<T: PartialEq>(a: &[T], b: &[T]) -> bool {
		a.iter().any(|e| b.contains(e))
	}

	fn ensure_members_sorted() {
		let mut members = Elections::members().clone();
		members.sort_by_key(|m| m.who);
		assert_eq!(Elections::members(), members);
	}

	fn ensure_candidates_sorted() {
		let mut candidates = Elections::candidates().clone();
		candidates.sort_by_key(|(c, _)| *c);
		assert_eq!(Elections::candidates(), candidates);
	}

	fn locked_stake_of(who: &u64) -> u64 {
		Voting::<Test>::get(who).stake
	}

	fn ensure_members_has_approval_stake() {
		// we filter members that have no approval state. This means that even we have more seats
		// than candidates, we will never ever chose a member with no votes.
		assert!(Elections::members()
			.iter()
			.chain(Elections::runners_up().iter())
			.all(|s| s.stake != u64::zero()));
	}

	fn ensure_member_candidates_runners_up_disjoint() {
		// members, candidates and runners-up must always be disjoint sets.
		assert!(!intersects(&Elections::members_ids(), &candidate_ids()));
		assert!(!intersects(&Elections::members_ids(), &runners_up_ids()));
		assert!(!intersects(&candidate_ids(), &runners_up_ids()));
	}

	fn pre_conditions() {
		System::set_block_number(1);
		ensure_members_sorted();
		ensure_candidates_sorted();
	}

	fn post_conditions() {
		ensure_members_sorted();
		ensure_candidates_sorted();
		ensure_member_candidates_runners_up_disjoint();
		ensure_members_has_approval_stake();
	}

	fn submit_candidacy(origin: Origin) -> DispatchResult {
		Elections::submit_candidacy(origin, Elections::candidates().len() as u32)
	}

	fn vote(origin: Origin, votes: Vec<u64>, stake: u64) -> DispatchResultWithPostInfo {
		// historical note: helper function was created in a period of time in which the API of vote
		// call was changing. Currently it is a wrapper for the original call and does not do much.
		// Nonetheless, totally harmless.
		ensure_signed(origin.clone()).expect("vote origin must be signed");
		Elections::vote(origin, votes, stake)
	}

	fn votes_of(who: &u64) -> Vec<u64> {
		Voting::<Test>::get(who).votes
	}

	#[test]
	fn params_should_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(Elections::desired_members(), 2);
			assert_eq!(Elections::desired_runners_up(), 0);
			assert_eq!(<Test as Trait>::VotingBondBase::get(), 2);
			assert_eq!(<Test as Trait>::VotingBondFactor::get(), 0);
			assert_eq!(<Test as Trait>::CandidacyBond::get(), 3);
			assert_eq!(Elections::term_duration(), 5);
			assert_eq!(Elections::election_rounds(), 0);

			assert!(Elections::members().is_empty());
			assert!(Elections::runners_up().is_empty());

			assert!(candidate_ids().is_empty());
			assert_eq!(<Candidates<Test>>::decode_len(), None);
			assert!(Elections::is_candidate(&1).is_err());

			assert!(all_voters().is_empty());
			assert!(votes_of(&1).is_empty());
		});
	}

	#[test]
	fn genesis_members_should_work() {
		ExtBuilder::default()
			.genesis_members(vec![(1, 10), (2, 20)])
			.build_and_execute(|| {
				System::set_block_number(1);
				assert_eq!(
					Elections::members(),
					vec![
						SeatHolder {
							who: 1,
							stake: 10,
							deposit: 3,
						},
						SeatHolder {
							who: 2,
							stake: 20,
							deposit: 3,
						}
					]
				);

				assert_eq!(
					Elections::voting(1),
					Voter {
						stake: 10u64,
						votes: vec![1],
						deposit: 2,
					}
				);
				assert_eq!(
					Elections::voting(2),
					Voter {
						stake: 20u64,
						votes: vec![2],
						deposit: 2,
					}
				);

				// they will persist since they have self vote.
				System::set_block_number(5);
				Elections::on_initialize(System::block_number());

				assert_eq!(Elections::members_ids(), vec![1, 2]);
			})
	}

	#[test]
	fn genesis_members_unsorted_should_work() {
		ExtBuilder::default()
			.genesis_members(vec![(2, 20), (1, 10)])
			.build_and_execute(|| {
				System::set_block_number(1);
				assert_eq!(
					Elections::members(),
					vec![
						SeatHolder {
							who: 1,
							stake: 10,
							deposit: 3,
						},
						SeatHolder {
							who: 2,
							stake: 20,
							deposit: 3,
						}
					]
				);

				assert_eq!(
					Elections::voting(1),
					Voter {
						stake: 10u64,
						votes: vec![1],
						deposit: 2
					}
				);
				assert_eq!(
					Elections::voting(2),
					Voter {
						stake: 20u64,
						votes: vec![2],
						deposit: 2
					}
				);

				// they will persist since they have self vote.
				System::set_block_number(5);
				Elections::on_initialize(System::block_number());

				assert_eq!(Elections::members_ids(), vec![1, 2]);
			})
	}

	#[test]
	#[should_panic = "Genesis member does not have enough stake"]
	fn genesis_members_cannot_over_stake_0() {
		// 10 cannot lock 20 as their stake and extra genesis will panic.
		ExtBuilder::default()
			.genesis_members(vec![(1, 20), (2, 20)])
			.build_and_execute(|| {});
	}

	#[test]
	#[should_panic]
	fn genesis_members_cannot_over_stake_1() {
		// 20 cannot reserve 20 as voting bond and extra genesis will panic.
		ExtBuilder::default()
			.voter_bond(20)
			.genesis_members(vec![(1, 10), (2, 20)])
			.build_and_execute(|| {});
	}

	#[test]
	#[should_panic = "Duplicate member in elections-phragmen genesis: 2"]
	fn genesis_members_cannot_be_duplicate() {
		ExtBuilder::default()
			.genesis_members(vec![(1, 10), (2, 10), (2, 12)])
			.build_and_execute(|| {});
	}

	#[test]
	fn term_duration_zero_is_passive() {
		ExtBuilder::default()
			.term_duration(0)
			.build_and_execute(||
		{
			assert_eq!(Elections::term_duration(), 0);
			assert_eq!(Elections::desired_members(), 2);
			assert_eq!(Elections::election_rounds(), 0);

			assert!(Elections::members_ids().is_empty());
			assert!(Elections::runners_up().is_empty());
			assert!(candidate_ids().is_empty());

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert!(Elections::members_ids().is_empty());
			assert!(Elections::runners_up().is_empty());
			assert!(candidate_ids().is_empty());
		});
	}

	#[test]
	fn simple_candidate_submission_should_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(candidate_ids(), Vec::<u64>::new());
			assert!(Elections::is_candidate(&1).is_err());
			assert!(Elections::is_candidate(&2).is_err());

			assert_eq!(balances(&1), (10, 0));
			assert_ok!(submit_candidacy(Origin::signed(1)));
			assert_eq!(balances(&1), (7, 3));

			assert_eq!(candidate_ids(), vec![1]);

			assert!(Elections::is_candidate(&1).is_ok());
			assert!(Elections::is_candidate(&2).is_err());

			assert_eq!(balances(&2), (20, 0));
			assert_ok!(submit_candidacy(Origin::signed(2)));
			assert_eq!(balances(&2), (17, 3));

			assert_eq!(candidate_ids(), vec![1, 2]);

			assert!(Elections::is_candidate(&1).is_ok());
			assert!(Elections::is_candidate(&2).is_ok());

			assert_eq!(candidate_deposit(&1), 3);
			assert_eq!(candidate_deposit(&2), 3);
			assert_eq!(candidate_deposit(&3), 0);
		});
	}

	#[test]
	fn updating_candidacy_bond_works() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_eq!(Elections::candidates(), vec![(5, 3)]);

			// a runtime upgrade changes the bond.
			CANDIDACY_BOND.with(|v| *v.borrow_mut() = 4);

			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_eq!(Elections::candidates(), vec![(4, 4), (5, 3)]);

			// once elected, they each hold their candidacy bond, no more.
			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(
				Elections::members(),
				vec![
					SeatHolder {
						who: 4,
						stake: 40,
						deposit: 4
					},
					SeatHolder {
						who: 5,
						stake: 50,
						deposit: 3
					}
				]
			);
		})
	}

	#[test]
	fn simple_candidate_submission_with_no_votes_should_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(candidate_ids(), Vec::<u64>::new());

			assert_ok!(submit_candidacy(Origin::signed(1)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert!(Elections::is_candidate(&1).is_ok());
			assert!(Elections::is_candidate(&2).is_ok());
			assert_eq!(candidate_ids(), vec![1, 2]);

			assert!(Elections::members_ids().is_empty());
			assert!(Elections::runners_up().is_empty());

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert!(Elections::is_candidate(&1).is_err());
			assert!(Elections::is_candidate(&2).is_err());
			assert!(candidate_ids().is_empty());

			assert!(Elections::members_ids().is_empty());
			assert!(Elections::runners_up().is_empty());
		});
	}

	#[test]
	fn dupe_candidate_submission_should_not_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(candidate_ids(), Vec::<u64>::new());
			assert_ok!(submit_candidacy(Origin::signed(1)));
			assert_eq!(candidate_ids(), vec![1]);
			assert_noop!(
				submit_candidacy(Origin::signed(1)),
				Error::<Test>::DuplicatedCandidate,
			);
		});
	}

	#[test]
	fn member_candidacy_submission_should_not_work() {
		// critically important to make sure that outgoing candidates and losers are not mixed up.
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(vote(Origin::signed(2), vec![5], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![5]);
			assert!(Elections::runners_up().is_empty());
			assert!(candidate_ids().is_empty());

			assert_noop!(
				submit_candidacy(Origin::signed(5)),
				Error::<Test>::MemberSubmit,
			);
		});
	}

	#[test]
	fn runner_candidate_submission_should_not_work() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(2), vec![5, 4], 20));
			assert_ok!(vote(Origin::signed(1), vec![3], 10));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![3]);

			assert_noop!(
				submit_candidacy(Origin::signed(3)),
				Error::<Test>::RunnerUpSubmit,
			);
		});
	}

	#[test]
	fn poor_candidate_submission_should_not_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(candidate_ids(), Vec::<u64>::new());
			assert_noop!(
				submit_candidacy(Origin::signed(7)),
				Error::<Test>::InsufficientCandidateFunds,
			);
		});
	}

	#[test]
	fn simple_voting_should_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(candidate_ids(), Vec::<u64>::new());
			assert_eq!(balances(&2), (20, 0));

			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(vote(Origin::signed(2), vec![5], 20));

			assert_eq!(balances(&2), (18, 2));
			assert_eq!(has_lock(&2), 20);
		});
	}

	#[test]
	fn can_vote_with_custom_stake() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(candidate_ids(), Vec::<u64>::new());
			assert_eq!(balances(&2), (20, 0));

			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(vote(Origin::signed(2), vec![5], 12));

			assert_eq!(balances(&2), (18, 2));
			assert_eq!(has_lock(&2), 12);
		});
	}

	#[test]
	fn can_update_votes_and_stake() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(balances(&2), (20, 0));

			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(vote(Origin::signed(2), vec![5], 20));

			assert_eq!(balances(&2), (18, 2));
			assert_eq!(has_lock(&2), 20);
			assert_eq!(locked_stake_of(&2), 20);

			// can update; different stake; different lock and reserve.
			assert_ok!(vote(Origin::signed(2), vec![5, 4], 15));
			assert_eq!(balances(&2), (18, 2));
			assert_eq!(has_lock(&2), 15);
			assert_eq!(locked_stake_of(&2), 15);
		});
	}

	#[test]
	fn updated_voting_bond_works() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_eq!(balances(&2), (20, 0));
			assert_ok!(vote(Origin::signed(2), vec![5], 5));
			assert_eq!(balances(&2), (18, 2));
			assert_eq!(voter_deposit(&2), 2);

			// a runtime upgrade lowers the voting bond to 1. This guy still un-reserves 2 when
			// leaving.
			VOTING_BOND_BASE.with(|v| *v.borrow_mut() = 1);

			// proof that bond changed.
			assert_eq!(balances(&1), (10, 0));
			assert_ok!(vote(Origin::signed(1), vec![5], 5));
			assert_eq!(balances(&1), (9, 1));
			assert_eq!(voter_deposit(&1), 1);

			assert_ok!(Elections::remove_voter(Origin::signed(2)));
			assert_eq!(balances(&2), (20, 0));
		})
	}

	#[test]
	fn voting_reserves_bond_per_vote() {
		ExtBuilder::default().voter_bond_factor(1).build_and_execute(|| {
			assert_eq!(balances(&2), (20, 0));

			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			// initial vote.
			assert_ok!(vote(Origin::signed(2), vec![5], 10));

			// 2 + 1
			assert_eq!(balances(&2), (17, 3));
			assert_eq!(Elections::voting(&2).deposit, 3);
			assert_eq!(has_lock(&2), 10);
			assert_eq!(locked_stake_of(&2), 10);

			// can update; different stake; different lock and reserve.
			assert_ok!(vote(Origin::signed(2), vec![5, 4], 15));
			// 2 + 2
			assert_eq!(balances(&2), (16, 4));
			assert_eq!(Elections::voting(&2).deposit, 4);
			assert_eq!(has_lock(&2), 15);
			assert_eq!(locked_stake_of(&2), 15);

			// stay at two votes with different stake.
			assert_ok!(vote(Origin::signed(2), vec![5, 3], 18));
			// 2 + 2
			assert_eq!(balances(&2), (16, 4));
			assert_eq!(Elections::voting(&2).deposit, 4);
			assert_eq!(has_lock(&2), 18);
			assert_eq!(locked_stake_of(&2), 18);

			// back to 1 vote.
			assert_ok!(vote(Origin::signed(2), vec![4], 12));
			// 2 + 1
			assert_eq!(balances(&2), (17, 3));
			assert_eq!(Elections::voting(&2).deposit, 3);
			assert_eq!(has_lock(&2), 12);
			assert_eq!(locked_stake_of(&2), 12);
		});
	}

	#[test]
	fn cannot_vote_for_no_candidate() {
		ExtBuilder::default().build_and_execute(|| {
			assert_noop!(
				vote(Origin::signed(2), vec![], 20),
				Error::<Test>::NoVotes,
			);
		});
	}

	#[test]
	fn can_vote_for_old_members_even_when_no_new_candidates() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(2), vec![4, 5], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert!(candidate_ids().is_empty());

			assert_ok!(vote(Origin::signed(3), vec![4, 5], 10));
		});
	}

	#[test]
	fn prime_works() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_ok!(vote(Origin::signed(1), vec![4, 3], 10));
			assert_ok!(vote(Origin::signed(2), vec![4], 20));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert!(candidate_ids().is_empty());

			assert_ok!(vote(Origin::signed(3), vec![4, 5], 10));
			assert_eq!(PRIME.with(|p| *p.borrow()), Some(4));
		});
	}

	#[test]
	fn prime_votes_for_exiting_members_are_removed() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_ok!(vote(Origin::signed(1), vec![4, 3], 10));
			assert_ok!(vote(Origin::signed(2), vec![4], 20));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			assert_ok!(Elections::renounce_candidacy(Origin::signed(4), Renouncing::Candidate(3)));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![3, 5]);
			assert!(candidate_ids().is_empty());

			assert_eq!(PRIME.with(|p| *p.borrow()), Some(5));
		});
	}

	#[test]
	fn prime_is_kept_if_other_members_leave() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(PRIME.with(|p| *p.borrow()), Some(5));
			assert_ok!(Elections::renounce_candidacy(
				Origin::signed(4),
				Renouncing::Member
			));

			assert_eq!(Elections::members_ids(), vec![5]);
			assert_eq!(PRIME.with(|p| *p.borrow()), Some(5));
		})
	}

	#[test]
	fn prime_is_gone_if_renouncing() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(PRIME.with(|p| *p.borrow()), Some(5));
			assert_ok!(Elections::renounce_candidacy(Origin::signed(5), Renouncing::Member));

			assert_eq!(Elections::members_ids(), vec![4]);
			assert_eq!(PRIME.with(|p| *p.borrow()), None);
		})
	}

	#[test]
	fn cannot_vote_for_more_than_candidates_and_members_and_runners() {
		ExtBuilder::default()
			.desired_runners_up(1)
			.balance_factor(10)
			.build_and_execute(
		|| {
			// when we have only candidates
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_noop!(
				// content of the vote is irrelevant.
				vote(Origin::signed(1), vec![9, 99, 999, 9999], 5),
				Error::<Test>::TooManyVotes,
			);

			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			// now we have 2 members, 1 runner-up, and 1 new candidate
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(1), vec![9, 99, 999, 9999], 5));
			assert_noop!(
				vote(Origin::signed(1), vec![9, 99, 999, 9_999, 99_999], 5),
				Error::<Test>::TooManyVotes,
			);
		});
	}

	#[test]
	fn cannot_vote_for_less_than_ed() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_noop!(
				vote(Origin::signed(2), vec![4], 1),
				Error::<Test>::LowBalance,
			);
		})
	}

	#[test]
	fn can_vote_for_more_than_total_balance_but_moot() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(2), vec![4, 5], 30));
			// you can lie but won't get away with it.
			assert_eq!(locked_stake_of(&2), 20);
			assert_eq!(has_lock(&2), 20);
		});
	}

	#[test]
	fn remove_voter_should_work() {
		ExtBuilder::default().voter_bond(8).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_ok!(vote(Origin::signed(2), vec![5], 20));
			assert_ok!(vote(Origin::signed(3), vec![5], 30));

			assert_eq_uvec!(all_voters(), vec![2, 3]);
			assert_eq!(locked_stake_of(&2), 20);
			assert_eq!(locked_stake_of(&3), 30);
			assert_eq!(votes_of(&2), vec![5]);
			assert_eq!(votes_of(&3), vec![5]);

			assert_ok!(Elections::remove_voter(Origin::signed(2)));

			assert_eq_uvec!(all_voters(), vec![3]);
			assert!(votes_of(&2).is_empty());
			assert_eq!(locked_stake_of(&2), 0);

			assert_eq!(balances(&2), (20, 0));
			assert_eq!(Balances::locks(&2).len(), 0);
		});
	}

	#[test]
	fn non_voter_remove_should_not_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_noop!(Elections::remove_voter(Origin::signed(3)), Error::<Test>::MustBeVoter);
		});
	}

	#[test]
	fn dupe_remove_should_fail() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(vote(Origin::signed(2), vec![5], 20));

			assert_ok!(Elections::remove_voter(Origin::signed(2)));
			assert!(all_voters().is_empty());

			assert_noop!(Elections::remove_voter(Origin::signed(2)), Error::<Test>::MustBeVoter);
		});
	}

	#[test]
	fn removed_voter_should_not_be_counted() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			assert_ok!(Elections::remove_voter(Origin::signed(4)));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![3, 5]);
		});
	}

	#[test]
	fn simple_voting_rounds_should_work() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(2), vec![5], 20));
			assert_ok!(vote(Origin::signed(4), vec![4], 15));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			assert_eq_uvec!(all_voters(), vec![2, 3, 4]);

			assert_eq!(votes_of(&2), vec![5]);
			assert_eq!(votes_of(&3), vec![3]);
			assert_eq!(votes_of(&4), vec![4]);

			assert_eq!(candidate_ids(), vec![3, 4, 5]);
			assert_eq!(<Candidates<Test>>::decode_len().unwrap(), 3);

			assert_eq!(Elections::election_rounds(), 0);

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(members_and_stake(), vec![(3, 30), (5, 20)]);
			assert!(Elections::runners_up().is_empty());

			assert_eq_uvec!(all_voters(), vec![2, 3, 4]);
			assert!(candidate_ids().is_empty());
			assert_eq!(<Candidates<Test>>::decode_len(), None);

			assert_eq!(Elections::election_rounds(), 1);
		});
	}

	#[test]
	fn empty_term() {
		ExtBuilder::default().build_and_execute(|| {
			// no candidates, no nothing.
			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(
				System::events().iter().last().unwrap().event,
				Event::elections_phragmen(RawEvent::EmptyTerm),
			)
		})
	}

	#[test]
	fn all_outgoing() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(
				System::events().iter().last().unwrap().event,
				Event::elections_phragmen(RawEvent::NewTerm(vec![(4, 40), (5, 50)])),
			);

			assert_eq!(members_and_stake(), vec![(4, 40), (5, 50)]);
			assert_eq!(runners_up_and_stake(), vec![]);

			assert_ok!(Elections::remove_voter(Origin::signed(5)));
			assert_ok!(Elections::remove_voter(Origin::signed(4)));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			assert_eq!(
				System::events().iter().last().unwrap().event,
				Event::elections_phragmen(RawEvent::NewTerm(vec![])),
			);

			// outgoing have lost their bond.
			assert_eq!(balances(&4), (37, 0));
			assert_eq!(balances(&5), (47, 0));
		});
	}

	#[test]
	fn defunct_voter_will_be_counted() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));

			// This guy's vote is pointless for this round.
			assert_ok!(vote(Origin::signed(3), vec![4], 30));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(members_and_stake(), vec![(5, 50)]);
			assert_eq!(Elections::election_rounds(), 1);

			// but now it has a valid target.
			assert_ok!(submit_candidacy(Origin::signed(4)));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			// candidate 4 is affected by an old vote.
			assert_eq!(members_and_stake(), vec![(4, 30), (5, 50)]);
			assert_eq!(Elections::election_rounds(), 2);
			assert_eq_uvec!(all_voters(), vec![3, 5]);
		});
	}

	#[test]
	fn only_desired_seats_are_chosen() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![2], 20));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::election_rounds(), 1);
			assert_eq!(Elections::members_ids(), vec![4, 5]);
		});
	}

	#[test]
	fn phragmen_should_not_self_vote() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert!(candidate_ids().is_empty());
			assert_eq!(Elections::election_rounds(), 1);
			assert!(Elections::members_ids().is_empty());

			assert_eq!(
				System::events().iter().last().unwrap().event,
				Event::elections_phragmen(RawEvent::NewTerm(vec![])),
			)
		});
	}

	#[test]
	fn runners_up_should_be_kept() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![3], 20));
			assert_ok!(vote(Origin::signed(3), vec![2], 30));
			assert_ok!(vote(Origin::signed(4), vec![5], 40));
			assert_ok!(vote(Origin::signed(5), vec![4], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			// sorted based on account id.
			assert_eq!(Elections::members_ids(), vec![4, 5]);
			// sorted based on merit (least -> most)
			assert_eq!(runners_up_ids(), vec![3, 2]);

			// runner ups are still locked.
			assert_eq!(balances(&4), (35, 5));
			assert_eq!(balances(&5), (45, 5));
			assert_eq!(balances(&3), (25, 5));
		});
	}

	#[test]
	fn runners_up_should_be_next_candidates() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![2], 20));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(members_and_stake(), vec![(4, 40), (5, 50)]);
			assert_eq!(runners_up_and_stake(), vec![(2, 20), (3, 30)]);

			assert_ok!(vote(Origin::signed(5), vec![5], 15));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());
			assert_eq!(members_and_stake(), vec![(3, 30), (4, 40)]);
			assert_eq!(runners_up_and_stake(), vec![(5, 15), (2, 20)]);
		});
	}

	#[test]
	fn runners_up_lose_bond_once_outgoing() {
		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![2], 20));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![2]);
			assert_eq!(balances(&2), (15, 5));

			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			assert_eq!(runners_up_ids(), vec![3]);
			assert_eq!(balances(&2), (15, 2));
		});
	}

	#[test]
	fn members_lose_bond_once_outgoing() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(balances(&5), (50, 0));

			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_eq!(balances(&5), (47, 3));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_eq!(balances(&5), (45, 5));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![5]);

			assert_ok!(Elections::remove_voter(Origin::signed(5)));
			assert_eq!(balances(&5), (47, 3));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());
			assert!(Elections::members_ids().is_empty());

			assert_eq!(balances(&5), (47, 0));
		});
	}

	#[test]
	fn losers_will_lose_the_bond() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(4), vec![5], 40));

			assert_eq!(balances(&5), (47, 3));
			assert_eq!(balances(&3), (27, 3));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![5]);

			// winner
			assert_eq!(balances(&5), (47, 3));
			// loser
			assert_eq!(balances(&3), (27, 0));
		});
	}

	#[test]
	fn current_members_are_always_next_candidate() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(Elections::election_rounds(), 1);

			assert_ok!(submit_candidacy(Origin::signed(2)));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			assert_ok!(Elections::remove_voter(Origin::signed(4)));

			// 5 will persist as candidates despite not being in the list.
			assert_eq!(candidate_ids(), vec![2, 3]);

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			// 4 removed; 5 and 3 are the new best.
			assert_eq!(Elections::members_ids(), vec![3, 5]);
		});
	}

	#[test]
	fn election_state_is_uninterrupted() {
		// what I mean by uninterrupted:
		// given no input or stimulants the same members are re-elected.
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			let check_at_block = |b: u32| {
				System::set_block_number(b.into());
				Elections::on_initialize(System::block_number());
				// we keep re-electing the same folks.
				assert_eq!(members_and_stake(), vec![(4, 40), (5, 50)]);
				assert_eq!(runners_up_and_stake(), vec![(2, 20), (3, 30)]);
				// no new candidates but old members and runners-up are always added.
				assert!(candidate_ids().is_empty());
				assert_eq!(Elections::election_rounds(), b / 5);
				assert_eq_uvec!(all_voters(), vec![2, 3, 4, 5]);
			};

			// this state will always persist when no further input is given.
			check_at_block(5);
			check_at_block(10);
			check_at_block(15);
			check_at_block(20);
		});
	}

	#[test]
	fn remove_members_triggers_election() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(Elections::election_rounds(), 1);

			// a new candidate
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			assert_ok!(Elections::remove_member(Origin::root(), 4, false));

			assert_eq!(balances(&4), (35, 2)); // slashed
			assert_eq!(Elections::election_rounds(), 2); // new election round
			assert_eq!(Elections::members_ids(), vec![3, 5]); // new members
		});
	}

	#[test]
	fn remove_member_should_indicate_replacement() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![4, 5]);

			// no replacement yet.
			assert_err_with_weight!(
				Elections::remove_member(Origin::root(), 4, true),
				Error::<Test>::InvalidReplacement,
				Some(34042000), // only thing that matters for now is that it is NOT the full block.
			);
		});

		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![3]);

			// there is a replacement! and this one needs a weight refund.
			assert_err_with_weight!(
				Elections::remove_member(Origin::root(), 4, false),
				Error::<Test>::InvalidReplacement,
				Some(34042000) // only thing that matters for now is that it is NOT the full block.
			);
		});
	}

	#[test]
	fn seats_should_be_released_when_no_vote() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(2), vec![3], 20));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			assert_eq!(<Candidates<Test>>::decode_len().unwrap(), 3);

			assert_eq!(Elections::election_rounds(), 0);

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![3, 5]);
			assert_eq!(Elections::election_rounds(), 1);

			assert_ok!(Elections::remove_voter(Origin::signed(2)));
			assert_ok!(Elections::remove_voter(Origin::signed(3)));
			assert_ok!(Elections::remove_voter(Origin::signed(4)));
			assert_ok!(Elections::remove_voter(Origin::signed(5)));

			// meanwhile, no one cares to become a candidate again.
			System::set_block_number(10);
			Elections::on_initialize(System::block_number());
			assert!(Elections::members_ids().is_empty());
			assert_eq!(Elections::election_rounds(), 2);
		});
	}

	#[test]
	fn incoming_outgoing_are_reported() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(5)));

			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			assert_eq!(Elections::members_ids(), vec![4, 5]);

			assert_ok!(submit_candidacy(Origin::signed(1)));
			assert_ok!(submit_candidacy(Origin::signed(2)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			// 5 will change their vote and becomes an `outgoing`
			assert_ok!(vote(Origin::signed(5), vec![4], 8));
			// 4 will stay in the set
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			// 3 will become a winner
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			// these two are losers.
			assert_ok!(vote(Origin::signed(2), vec![2], 20));
			assert_ok!(vote(Origin::signed(1), vec![1], 10));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			// 3, 4 are new members, must still be bonded, nothing slashed.
			assert_eq!(members_and_stake(), vec![(3, 30), (4, 48)]);
			assert_eq!(balances(&3), (25, 5));
			assert_eq!(balances(&4), (35, 5));

			// 1 is a loser, slashed by 3.
			assert_eq!(balances(&1), (5, 2));

			// 5 is an outgoing loser. will also get slashed.
			assert_eq!(balances(&5), (45, 2));

			assert!(System::events().iter().any(|event| {
				event.event == Event::elections_phragmen(RawEvent::NewTerm(vec![(4, 40), (5, 50)]))
			}));
		})
	}

	#[test]
	fn invalid_votes_are_moot() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![10], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq_uvec!(Elections::members_ids(), vec![3, 4]);
			assert_eq!(Elections::election_rounds(), 1);
		});
	}

	#[test]
	fn members_are_sorted_based_on_id_runners_on_merit() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![3], 20));
			assert_ok!(vote(Origin::signed(3), vec![2], 30));
			assert_ok!(vote(Origin::signed(4), vec![5], 40));
			assert_ok!(vote(Origin::signed(5), vec![4], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());
			// id: low -> high.
			assert_eq!(members_and_stake(), vec![(4, 50), (5, 40)]);
			// merit: low -> high.
			assert_eq!(runners_up_and_stake(), vec![(3, 20), (2, 30)]);
		});
	}

	#[test]
	fn candidates_are_sorted() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_eq!(candidate_ids(), vec![3, 5]);

			assert_ok!(submit_candidacy(Origin::signed(2)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(Elections::renounce_candidacy(Origin::signed(3), Renouncing::Candidate(4)));

			assert_eq!(candidate_ids(), vec![2, 4, 5]);
		})
	}

	#[test]
	fn runner_up_replacement_maintains_members_order() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![5], 20));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![2], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![2, 4]);
			assert_ok!(Elections::remove_member(Origin::root(), 2, true));
			assert_eq!(Elections::members_ids(), vec![4, 5]);
		});
	}

	#[test]
	fn can_renounce_candidacy_member_with_runners_bond_is_refunded() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![2, 3]);

			assert_ok!(Elections::renounce_candidacy(Origin::signed(4), Renouncing::Member));
			assert_eq!(balances(&4), (38, 2)); // 2 is voting bond.

			assert_eq!(Elections::members_ids(), vec![3, 5]);
			assert_eq!(runners_up_ids(), vec![2]);
		})
	}

	#[test]
	fn can_renounce_candidacy_member_without_runners_bond_is_refunded() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert!(runners_up_ids().is_empty());

			assert_ok!(Elections::renounce_candidacy(Origin::signed(4), Renouncing::Member));
			assert_eq!(balances(&4), (38, 2)); // 2 is voting bond.

			// no replacement
			assert_eq!(Elections::members_ids(), vec![5]);
			assert!(runners_up_ids().is_empty());
		})
	}

	#[test]
	fn can_renounce_candidacy_runner() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(5), vec![4], 50));
			assert_ok!(vote(Origin::signed(4), vec![5], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![2, 3]);

			assert_ok!(Elections::renounce_candidacy(Origin::signed(3), Renouncing::RunnerUp));
			assert_eq!(balances(&3), (28, 2)); // 2 is voting bond.

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![2]);
		})
	}

	#[test]
	fn runner_up_replacement_works_when_out_of_order() {
		ExtBuilder::default().desired_runners_up(2).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));

			assert_ok!(vote(Origin::signed(2), vec![5], 20));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(5), vec![2], 50));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![2, 4]);
			assert_eq!(runners_up_ids(), vec![5, 3]);
			assert_ok!(Elections::renounce_candidacy(Origin::signed(3), Renouncing::RunnerUp));
			assert_eq!(Elections::members_ids(), vec![2, 4]);
			assert_eq!(runners_up_ids(), vec![5]);
		});
	}

	#[test]
	fn can_renounce_candidacy_candidate() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_eq!(balances(&5), (47, 3));
			assert_eq!(candidate_ids(), vec![5]);

			assert_ok!(Elections::renounce_candidacy(Origin::signed(5), Renouncing::Candidate(1)));
			assert_eq!(balances(&5), (50, 0));
			assert!(candidate_ids().is_empty());
		})
	}

	#[test]
	fn wrong_renounce_candidacy_should_fail() {
		ExtBuilder::default().build_and_execute(|| {
			assert_noop!(
				Elections::renounce_candidacy(Origin::signed(5), Renouncing::Candidate(0)),
				Error::<Test>::InvalidRenouncing,
			);
			assert_noop!(
				Elections::renounce_candidacy(Origin::signed(5), Renouncing::Member),
				Error::<Test>::NotMember,
			);
			assert_noop!(
				Elections::renounce_candidacy(Origin::signed(5), Renouncing::RunnerUp),
				Error::<Test>::InvalidRenouncing,
			);
		})
	}

	#[test]
	fn non_member_renounce_member_should_fail() {
		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![3]);

			assert_noop!(
				Elections::renounce_candidacy(Origin::signed(3), Renouncing::Member),
				Error::<Test>::NotMember,
			);
		})
	}

	#[test]
	fn non_runner_up_renounce_runner_up_should_fail() {
		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![3]);

			assert_noop!(
				Elections::renounce_candidacy(Origin::signed(4), Renouncing::RunnerUp),
				Error::<Test>::InvalidRenouncing,
			);
		})
	}

	#[test]
	fn wrong_candidate_count_renounce_should_fail() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_noop!(
				Elections::renounce_candidacy(Origin::signed(4), Renouncing::Candidate(2)),
				Error::<Test>::InvalidRenouncing,
			);

			assert_ok!(Elections::renounce_candidacy(Origin::signed(4), Renouncing::Candidate(3)));
		})
	}

	#[test]
	fn renounce_candidacy_count_can_overestimate() {
		ExtBuilder::default().build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			// while we have only 3 candidates.
			assert_ok!(Elections::renounce_candidacy(Origin::signed(4), Renouncing::Candidate(4)));
		})
	}

	#[test]
	fn behavior_with_dupe_candidate() {
		ExtBuilder::default()
			.desired_runners_up(2)
			.build_and_execute(|| {
				<Candidates<Test>>::put(vec![(1, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);

				assert_ok!(vote(Origin::signed(5), vec![1], 50));
				assert_ok!(vote(Origin::signed(4), vec![4], 40));
				assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

				assert_eq!(Elections::members_ids(), vec![1, 4]);
				assert_eq!(runners_up_ids(), vec![2, 3]);
				assert!(candidate_ids().is_empty());
			})
	}

	#[test]
	fn unsorted_runners_up_are_detected() {
		ExtBuilder::default().desired_runners_up(2).desired_members(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 5));
			assert_ok!(vote(Origin::signed(3), vec![3], 15));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![5]);
			assert_eq!(runners_up_ids(), vec![4, 3]);

			assert_ok!(submit_candidacy(Origin::signed(2)));
			assert_ok!(vote(Origin::signed(2), vec![2], 10));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![5]);
			assert_eq!(runners_up_ids(), vec![2, 3]);

			// 4 is outgoing runner-up. Slash candidacy bond.
			assert_eq!(balances(&4), (35, 2));
			// 3 stays.
			assert_eq!(balances(&3), (25, 5));
		})
	}

	#[test]
	fn member_to_runner_up_wont_slash() {
		ExtBuilder::default().desired_runners_up(2).desired_members(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));


			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4]);
			assert_eq!(runners_up_ids(), vec![2, 3]);

			assert_eq!(balances(&4), (35, 5));
			assert_eq!(balances(&3), (25, 5));
			assert_eq!(balances(&2), (15, 5));

			// this guy will shift everyone down.
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(vote(Origin::signed(5), vec![5], 50));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![5]);
			assert_eq!(runners_up_ids(), vec![3, 4]);

			// 4 went from member to runner-up -- don't slash.
			assert_eq!(balances(&4), (35, 5));
			// 3 stayed runner-up -- don't slash.
			assert_eq!(balances(&3), (25, 5));
			// 2 was removed -- slash.
			assert_eq!(balances(&2), (15, 2));
		});
	}

	#[test]
	fn runner_up_to_member_wont_slash() {
		ExtBuilder::default().desired_runners_up(2).desired_members(1).build_and_execute(|| {
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));
			assert_ok!(submit_candidacy(Origin::signed(2)));


			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));
			assert_ok!(vote(Origin::signed(2), vec![2], 20));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4]);
			assert_eq!(runners_up_ids(), vec![2, 3]);

			assert_eq!(balances(&4), (35, 5));
			assert_eq!(balances(&3), (25, 5));
			assert_eq!(balances(&2), (15, 5));

			// swap some votes.
			assert_ok!(vote(Origin::signed(4), vec![2], 40));
			assert_ok!(vote(Origin::signed(2), vec![4], 20));

			System::set_block_number(10);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![2]);
			assert_eq!(runners_up_ids(), vec![4, 3]);

			// 2 went from runner to member, don't slash
			assert_eq!(balances(&2), (15, 5));
			// 4 went from member to runner, don't slash
			assert_eq!(balances(&4), (35, 5));
			// 3 stayed the same
			assert_eq!(balances(&3), (25, 5));
		});
	}

	#[test]
	fn dupe_voter_is_moot() {}

	#[test]
	fn remove_and_replace_member_works() {
		let setup = || {
			assert_ok!(submit_candidacy(Origin::signed(5)));
			assert_ok!(submit_candidacy(Origin::signed(4)));
			assert_ok!(submit_candidacy(Origin::signed(3)));

			assert_ok!(vote(Origin::signed(5), vec![5], 50));
			assert_ok!(vote(Origin::signed(4), vec![4], 40));
			assert_ok!(vote(Origin::signed(3), vec![3], 30));

			System::set_block_number(5);
			Elections::on_initialize(System::block_number());

			assert_eq!(Elections::members_ids(), vec![4, 5]);
			assert_eq!(runners_up_ids(), vec![3]);
		};

		// member removed, replacement found.
		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			setup();
			assert_eq!(Elections::remove_and_replace_member(&4, false), Ok(true));

			assert_eq!(Elections::members_ids(), vec![3, 5]);
			assert_eq!(runners_up_ids().len(), 0);
		});

		// member removed, no replacement found.
		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			setup();
			assert_ok!(Elections::renounce_candidacy(Origin::signed(3), Renouncing::RunnerUp));
			assert_eq!(Elections::remove_and_replace_member(&4, false), Ok(false));

			assert_eq!(Elections::members_ids(), vec![5]);
			assert_eq!(runners_up_ids().len(), 0);
		});

		// wrong member to remove.
		ExtBuilder::default().desired_runners_up(1).build_and_execute(|| {
			setup();
			assert!(matches!(Elections::remove_and_replace_member(&2, false), Err(_)));
		});
	}
}
