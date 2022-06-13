// This file is part of Substrate.

// Copyright (C) 2017-2022 Parity Technologies (UK) Ltd.
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

//! # Scheduler tests.

use super::*;
use crate::mock::{logger, new_test_ext, root, run_to_block, Call, LoggerCall, Scheduler, Test, *};
use frame_support::{
	assert_err, assert_noop, assert_ok,
	traits::{Contains, GetStorageVersion, OnInitialize, QueryPreimage, StorePreimage},
	Hashable,
};
use sp_runtime::traits::Hash;
use substrate_test_utils::assert_eq_uvec;

#[test]
fn basic_scheduling_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		assert!(!<Test as frame_system::Config>::BaseCallFilter::contains(&call));
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap()
		));
		run_to_block(3);
		assert!(logger::log().is_empty());
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn scheduling_with_preimages_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		let hash = <Test as frame_system::Config>::Hashing::hash_of(&call);
		let len = call.using_encoded(|x| x.len()) as u32;
		let hashed = Preimage::pick(hash.clone(), len);
		assert_ok!(Preimage::note_preimage(Origin::signed(0), call.encode()));
		assert_ok!(Scheduler::do_schedule(DispatchTime::At(4), None, 127, root(), hashed));
		assert!(Preimage::is_requested(&hash));
		run_to_block(3);
		assert!(logger::log().is_empty());
		run_to_block(4);
		assert!(!Preimage::len(&hash).is_some());
		assert!(!Preimage::is_requested(&hash));
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn scheduling_with_preimage_postpones_correctly() {
	new_test_ext().execute_with(|| {
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		let hash = <Test as frame_system::Config>::Hashing::hash_of(&call);
		let len = call.using_encoded(|x| x.len()) as u32;
		let hashed = Preimage::pick(hash.clone(), len);

		assert_ok!(Scheduler::do_schedule(DispatchTime::At(4), None, 127, root(), hashed));
		assert!(Preimage::is_requested(&hash));

		run_to_block(4);
		// #4 empty due to no preimage
		assert!(logger::log().is_empty());

		// Register preimage.
		assert_ok!(Preimage::note_preimage(Origin::signed(0), call.encode()));

		run_to_block(5);
		// #5 empty since postponement is 2 blocks.
		assert!(logger::log().is_empty());

		run_to_block(6);
		// #6 is good.
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		assert!(!Preimage::len(&hash).is_some());
		assert!(!Preimage::is_requested(&hash));

		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn schedule_after_works() {
	new_test_ext().execute_with(|| {
		run_to_block(2);
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		assert!(!<Test as frame_system::Config>::BaseCallFilter::contains(&call));
		// This will schedule the call 3 blocks after the next block... so block 3 + 3 = 6
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::After(3),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap()
		));
		run_to_block(5);
		assert!(logger::log().is_empty());
		run_to_block(6);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn schedule_after_zero_works() {
	new_test_ext().execute_with(|| {
		run_to_block(2);
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		assert!(!<Test as frame_system::Config>::BaseCallFilter::contains(&call));
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::After(0),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap()
		));
		// Will trigger on the next block.
		run_to_block(3);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn periodic_scheduling_works() {
	new_test_ext().execute_with(|| {
		// at #4, every 3 blocks, 3 times.
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			Some((3, 3)),
			127,
			root(),
			Preimage::bound(Call::Logger(logger::Call::log { i: 42, weight: 1000 })).unwrap()
		));
		run_to_block(3);
		assert!(logger::log().is_empty());
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(6);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(7);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32)]);
		run_to_block(9);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32)]);
		run_to_block(10);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32), (root(), 42u32)]);
		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32), (root(), 42u32)]);
	});
}

#[test]
fn reschedule_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		assert!(!<Test as frame_system::Config>::BaseCallFilter::contains(&call));
		assert_eq!(
			Scheduler::do_schedule(
				DispatchTime::At(4),
				None,
				127,
				root(),
				Preimage::bound(call).unwrap()
			)
			.unwrap(),
			(4, 0)
		);

		run_to_block(3);
		assert!(logger::log().is_empty());

		assert_eq!(Scheduler::do_reschedule((4, 0), DispatchTime::At(6)).unwrap(), (6, 0));

		assert_noop!(
			Scheduler::do_reschedule((6, 0), DispatchTime::At(6)),
			Error::<Test>::RescheduleNoChange
		);

		run_to_block(4);
		assert!(logger::log().is_empty());

		run_to_block(6);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);

		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn reschedule_named_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		assert!(!<Test as frame_system::Config>::BaseCallFilter::contains(&call));
		assert_eq!(
			Scheduler::do_schedule_named(
				[1u8; 32],
				DispatchTime::At(4),
				None,
				127,
				root(),
				Preimage::bound(call).unwrap(),
			)
			.unwrap(),
			(4, 0)
		);

		run_to_block(3);
		assert!(logger::log().is_empty());

		assert_eq!(Scheduler::do_reschedule_named([1u8; 32], DispatchTime::At(6)).unwrap(), (6, 0));

		assert_noop!(
			Scheduler::do_reschedule_named([1u8; 32], DispatchTime::At(6)),
			Error::<Test>::RescheduleNoChange
		);

		run_to_block(4);
		assert!(logger::log().is_empty());

		run_to_block(6);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);

		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
	});
}

#[test]
fn reschedule_named_perodic_works() {
	new_test_ext().execute_with(|| {
		let call = Call::Logger(LoggerCall::log { i: 42, weight: 1000 });
		assert!(!<Test as frame_system::Config>::BaseCallFilter::contains(&call));
		assert_eq!(
			Scheduler::do_schedule_named(
				[1u8; 32],
				DispatchTime::At(4),
				Some((3, 3)),
				127,
				root(),
				Preimage::bound(call).unwrap(),
			)
			.unwrap(),
			(4, 0)
		);

		run_to_block(3);
		assert!(logger::log().is_empty());

		assert_eq!(Scheduler::do_reschedule_named([1u8; 32], DispatchTime::At(5)).unwrap(), (5, 0));
		assert_eq!(Scheduler::do_reschedule_named([1u8; 32], DispatchTime::At(6)).unwrap(), (6, 0));

		run_to_block(5);
		assert!(logger::log().is_empty());

		run_to_block(6);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);

		assert_eq!(
			Scheduler::do_reschedule_named([1u8; 32], DispatchTime::At(10)).unwrap(),
			(10, 0)
		);

		run_to_block(9);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);

		run_to_block(10);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32)]);

		run_to_block(13);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32), (root(), 42u32)]);

		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 42u32), (root(), 42u32)]);
	});
}

#[test]
fn cancel_named_scheduling_works_with_normal_cancel() {
	new_test_ext().execute_with(|| {
		// at #4.
		Scheduler::do_schedule_named(
			[1u8; 32],
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(Call::Logger(LoggerCall::log { i: 69, weight: 1000 })).unwrap(),
		)
		.unwrap();
		let i = Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(Call::Logger(LoggerCall::log { i: 42, weight: 1000 })).unwrap(),
		)
		.unwrap();
		run_to_block(3);
		assert!(logger::log().is_empty());
		assert_ok!(Scheduler::do_cancel_named(None, [1u8; 32]));
		assert_ok!(Scheduler::do_cancel(None, i));
		run_to_block(100);
		assert!(logger::log().is_empty());
	});
}

#[test]
fn cancel_named_periodic_scheduling_works() {
	new_test_ext().execute_with(|| {
		// at #4, every 3 blocks, 3 times.
		Scheduler::do_schedule_named(
			[1u8; 32],
			DispatchTime::At(4),
			Some((3, 3)),
			127,
			root(),
			Preimage::bound(Call::Logger(LoggerCall::log { i: 42, weight: 1000 })).unwrap(),
		)
		.unwrap();
		// same id results in error.
		assert!(Scheduler::do_schedule_named(
			[1u8; 32],
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(Call::Logger(LoggerCall::log { i: 69, weight: 1000 })).unwrap(),
		)
		.is_err());
		// different id is ok.
		Scheduler::do_schedule_named(
			[2u8; 32],
			DispatchTime::At(8),
			None,
			127,
			root(),
			Preimage::bound(Call::Logger(LoggerCall::log { i: 69, weight: 1000 })).unwrap(),
		)
		.unwrap();
		run_to_block(3);
		assert!(logger::log().is_empty());
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(6);
		assert_ok!(Scheduler::do_cancel_named(None, [1u8; 32]));
		run_to_block(100);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 69u32)]);
	});
}

#[test]
fn scheduler_respects_weight_limits() {
	new_test_ext().execute_with(|| {
		let call =
			Call::Logger(LoggerCall::log { i: 42, weight: MaximumSchedulerWeight::get() / 2 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call =
			Call::Logger(LoggerCall::log { i: 69, weight: MaximumSchedulerWeight::get() / 2 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap(),
		));
		// 69 and 42 do not fit together
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 42u32)]);
		run_to_block(5);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 69u32)]);
	});
}

#[test]
fn scheduler_respects_hard_deadlines_more() {
	new_test_ext().execute_with(|| {
		let call =
			Call::Logger(LoggerCall::log { i: 42, weight: MaximumSchedulerWeight::get() / 2 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			0,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call =
			Call::Logger(LoggerCall::log { i: 69, weight: MaximumSchedulerWeight::get() / 2 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			0,
			root(),
			Preimage::bound(call).unwrap(),
		));
		// With base weights, 69 and 42 should not fit together, but do because of hard
		// deadlines
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 42u32), (root(), 69u32)]);
	});
}

#[test]
fn scheduler_respects_priority_ordering() {
	new_test_ext().execute_with(|| {
		let call =
			Call::Logger(LoggerCall::log { i: 42, weight: MaximumSchedulerWeight::get() / 2 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			1,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call =
			Call::Logger(LoggerCall::log { i: 69, weight: MaximumSchedulerWeight::get() / 2 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			0,
			root(),
			Preimage::bound(call).unwrap(),
		));
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 69u32), (root(), 42u32)]);
	});
}

#[test]
fn scheduler_respects_priority_ordering_with_soft_deadlines() {
	new_test_ext().execute_with(|| {
		let max_weight = MaximumSchedulerWeight::get() - <() as WeightInfo>::on_initialize(0);
		let item_weight =
			<() as WeightInfo>::on_initialize(1) - <() as WeightInfo>::on_initialize(0);
		let call = Call::Logger(LoggerCall::log { i: 42, weight: max_weight / 2 - item_weight });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			255,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call = Call::Logger(LoggerCall::log { i: 69, weight: max_weight / 2 - item_weight });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call =
			Call::Logger(LoggerCall::log { i: 2600, weight: max_weight / 2 - item_weight + 1 });
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(4),
			None,
			126,
			root(),
			Preimage::bound(call).unwrap(),
		));

		// 2600 does not fit with 69 or 42, but has higher priority, so will go through
		run_to_block(4);
		assert_eq!(logger::log(), vec![(root(), 2600u32)]);
		// 69 and 42 fit together
		run_to_block(5);
		assert_eq!(logger::log(), vec![(root(), 2600u32), (root(), 69u32), (root(), 42u32)]);
	});
}

#[test]
fn on_initialize_weight_is_correct() {
	new_test_ext().execute_with(|| {
		let base_weight = <() as WeightInfo>::on_initialize(0);
		let call_weight = MaximumSchedulerWeight::get() / 4;

		// Named
		let call = Call::Logger(LoggerCall::log { i: 3, weight: call_weight + 1 });
		assert_ok!(Scheduler::do_schedule_named(
			[1u8; 32],
			DispatchTime::At(3),
			None,
			255,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call = Call::Logger(LoggerCall::log { i: 42, weight: call_weight + 2 });
		// Anon Periodic
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(2),
			Some((1000, 3)),
			128,
			root(),
			Preimage::bound(call).unwrap(),
		));
		let call = Call::Logger(LoggerCall::log { i: 69, weight: call_weight + 3 });
		// Anon
		assert_ok!(Scheduler::do_schedule(
			DispatchTime::At(2),
			None,
			127,
			root(),
			Preimage::bound(call).unwrap(),
		));
		// Named Periodic
		let call = Call::Logger(LoggerCall::log { i: 2600, weight: call_weight + 4 });
		assert_ok!(Scheduler::do_schedule_named(
			[2u8; 32],
			DispatchTime::At(1),
			Some((1000, 3)),
			126,
			root(),
			Preimage::bound(call).unwrap(),
		));

		// Will include the named periodic only
		let actual_weight = Scheduler::on_initialize(1);
		assert_eq!(
			actual_weight,
			base_weight +
				call_weight + 4 + <() as MarginalWeightInfo>::item(true, true, Some(false))
		);
		assert_eq!(logger::log(), vec![(root(), 2600u32)]);

		// Will include anon and anon periodic
		let actual_weight = Scheduler::on_initialize(2);
		assert_eq!(
			actual_weight,
			base_weight +
				call_weight + 2 + <() as MarginalWeightInfo>::item(false, false, Some(false)) +
				call_weight + 3 + <() as MarginalWeightInfo>::item(true, false, Some(false))
		);
		assert_eq!(logger::log(), vec![(root(), 2600u32), (root(), 69u32), (root(), 42u32)]);

		// Will include named only
		let actual_weight = Scheduler::on_initialize(3);
		assert_eq!(
			actual_weight,
			base_weight +
				call_weight + 1 + <() as MarginalWeightInfo>::item(false, true, Some(false))
		);
		assert_eq!(
			logger::log(),
			vec![(root(), 2600u32), (root(), 69u32), (root(), 42u32), (root(), 3u32)]
		);

		// Will contain none
		let actual_weight = Scheduler::on_initialize(4);
		assert_eq!(actual_weight, base_weight);
	});
}

#[test]
fn root_calls_works() {
	new_test_ext().execute_with(|| {
		let call = Box::new(Call::Logger(LoggerCall::log { i: 69, weight: 1000 }));
		let call2 = Box::new(Call::Logger(LoggerCall::log { i: 42, weight: 1000 }));
		assert_ok!(Scheduler::schedule_named(Origin::root(), [1u8; 32], 4, None, 127, call,));
		assert_ok!(Scheduler::schedule(Origin::root(), 4, None, 127, call2));
		run_to_block(3);
		// Scheduled calls are in the agenda.
		assert_eq!(Agenda::<Test>::get(4).len(), 2);
		assert!(logger::log().is_empty());
		assert_ok!(Scheduler::cancel_named(Origin::root(), [1u8; 32]));
		assert_ok!(Scheduler::cancel(Origin::root(), 4, 1));
		// Scheduled calls are made NONE, so should not effect state
		run_to_block(100);
		assert!(logger::log().is_empty());
	});
}

#[test]
fn fails_to_schedule_task_in_the_past() {
	new_test_ext().execute_with(|| {
		run_to_block(3);

		let call1 = Box::new(Call::Logger(LoggerCall::log { i: 69, weight: 1000 }));
		let call2 = Box::new(Call::Logger(LoggerCall::log { i: 42, weight: 1000 }));
		let call3 = Box::new(Call::Logger(LoggerCall::log { i: 42, weight: 1000 }));

		assert_err!(
			Scheduler::schedule_named(Origin::root(), [1u8; 32], 2, None, 127, call1),
			Error::<Test>::TargetBlockNumberInPast,
		);

		assert_err!(
			Scheduler::schedule(Origin::root(), 2, None, 127, call2),
			Error::<Test>::TargetBlockNumberInPast,
		);

		assert_err!(
			Scheduler::schedule(Origin::root(), 3, None, 127, call3),
			Error::<Test>::TargetBlockNumberInPast,
		);
	});
}

#[test]
fn should_use_orign() {
	new_test_ext().execute_with(|| {
		let call = Box::new(Call::Logger(LoggerCall::log { i: 69, weight: 1000 }));
		let call2 = Box::new(Call::Logger(LoggerCall::log { i: 42, weight: 1000 }));
		assert_ok!(Scheduler::schedule_named(
			system::RawOrigin::Signed(1).into(),
			[1u8; 32],
			4,
			None,
			127,
			call,
		));
		assert_ok!(Scheduler::schedule(system::RawOrigin::Signed(1).into(), 4, None, 127, call2,));
		run_to_block(3);
		// Scheduled calls are in the agenda.
		assert_eq!(Agenda::<Test>::get(4).len(), 2);
		assert!(logger::log().is_empty());
		assert_ok!(Scheduler::cancel_named(system::RawOrigin::Signed(1).into(), [1u8; 32]));
		assert_ok!(Scheduler::cancel(system::RawOrigin::Signed(1).into(), 4, 1));
		// Scheduled calls are made NONE, so should not effect state
		run_to_block(100);
		assert!(logger::log().is_empty());
	});
}

#[test]
fn should_check_orign() {
	new_test_ext().execute_with(|| {
		let call = Box::new(Call::Logger(LoggerCall::log { i: 69, weight: 1000 }));
		let call2 = Box::new(Call::Logger(LoggerCall::log { i: 42, weight: 1000 }));
		assert_noop!(
			Scheduler::schedule_named(
				system::RawOrigin::Signed(2).into(),
				[1u8; 32],
				4,
				None,
				127,
				call
			),
			BadOrigin
		);
		assert_noop!(
			Scheduler::schedule(system::RawOrigin::Signed(2).into(), 4, None, 127, call2),
			BadOrigin
		);
	});
}

#[test]
fn should_check_orign_for_cancel() {
	new_test_ext().execute_with(|| {
		let call = Box::new(Call::Logger(LoggerCall::log_without_filter { i: 69, weight: 1000 }));
		let call2 = Box::new(Call::Logger(LoggerCall::log_without_filter { i: 42, weight: 1000 }));
		assert_ok!(Scheduler::schedule_named(
			system::RawOrigin::Signed(1).into(),
			[1u8; 32],
			4,
			None,
			127,
			call,
		));
		assert_ok!(Scheduler::schedule(system::RawOrigin::Signed(1).into(), 4, None, 127, call2,));
		run_to_block(3);
		// Scheduled calls are in the agenda.
		assert_eq!(Agenda::<Test>::get(4).len(), 2);
		assert!(logger::log().is_empty());
		assert_noop!(
			Scheduler::cancel_named(system::RawOrigin::Signed(2).into(), [1u8; 32]),
			BadOrigin
		);
		assert_noop!(Scheduler::cancel(system::RawOrigin::Signed(2).into(), 4, 1), BadOrigin);
		assert_noop!(Scheduler::cancel_named(system::RawOrigin::Root.into(), [1u8; 32]), BadOrigin);
		assert_noop!(Scheduler::cancel(system::RawOrigin::Root.into(), 4, 1), BadOrigin);
		run_to_block(5);
		assert_eq!(
			logger::log(),
			vec![
				(system::RawOrigin::Signed(1).into(), 69u32),
				(system::RawOrigin::Signed(1).into(), 42u32)
			]
		);
	});
}

#[test]
fn migration_to_v4_works() {
	new_test_ext().execute_with(|| {
		for i in 0..3u64 {
			let k = i.twox_64_concat();
			let old = vec![
				Some(ScheduledV1 {
					maybe_id: None,
					priority: i as u8 + 10,
					call: Call::Logger(LoggerCall::log { i: 96, weight: 100 }),
					maybe_periodic: None,
				}),
				None,
				Some(ScheduledV1 {
					maybe_id: Some(b"test".to_vec()),
					priority: 123,
					call: Call::Logger(LoggerCall::log { i: 69, weight: 1000 }),
					maybe_periodic: Some((456u64, 10)),
				}),
			];
			frame_support::migration::put_storage_value(b"Scheduler", b"Agenda", &k, old);
		}

		Scheduler::migrate_v1_to_v4();

		let mut x = Agenda::<Test>::iter().collect::<Vec<_>>();
		x.sort_by_key(|x| x.0);
		let expected = vec![
			(
				0,
				vec![
					Some(ScheduledV4Of::<Test> {
						maybe_id: None,
						priority: 10,
						call: Preimage::bound(Call::Logger(LoggerCall::log { i: 96, weight: 100 }))
							.unwrap(),
						maybe_periodic: None,
						origin: root(),
						_phantom: PhantomData::<u64>::default(),
					}),
					None,
					Some(ScheduledV4Of::<Test> {
						maybe_id: Some(blake2_256(&b"test"[..])),
						priority: 123,
						call: Preimage::bound(Call::Logger(LoggerCall::log {
							i: 69,
							weight: 1000,
						}))
						.unwrap(),
						maybe_periodic: Some((456u64, 10)),
						origin: root(),
						_phantom: PhantomData::<u64>::default(),
					}),
				],
			),
			(
				1,
				vec![
					Some(ScheduledV4Of::<Test> {
						maybe_id: None,
						priority: 11,
						call: Preimage::bound(Call::Logger(LoggerCall::log { i: 96, weight: 100 }))
							.unwrap(),
						maybe_periodic: None,
						origin: root(),
						_phantom: PhantomData::<u64>::default(),
					}),
					None,
					Some(ScheduledV4Of::<Test> {
						maybe_id: Some(blake2_256(&b"test"[..])),
						priority: 123,
						call: Preimage::bound(Call::Logger(LoggerCall::log {
							i: 69,
							weight: 1000,
						}))
						.unwrap(),
						maybe_periodic: Some((456u64, 10)),
						origin: root(),
						_phantom: PhantomData::<u64>::default(),
					}),
				],
			),
			(
				2,
				vec![
					Some(ScheduledV4Of::<Test> {
						maybe_id: None,
						priority: 12,
						call: Preimage::bound(Call::Logger(LoggerCall::log { i: 96, weight: 100 }))
							.unwrap(),
						maybe_periodic: None,
						origin: root(),
						_phantom: PhantomData::<u64>::default(),
					}),
					None,
					Some(ScheduledV4Of::<Test> {
						maybe_id: Some(blake2_256(&b"test"[..])),
						priority: 123,
						call: Preimage::bound(Call::Logger(LoggerCall::log {
							i: 69,
							weight: 1000,
						}))
						.unwrap(),
						maybe_periodic: Some((456u64, 10)),
						origin: root(),
						_phantom: PhantomData::<u64>::default(),
					}),
				],
			),
		];
		for (i, j) in x.iter().zip(expected.iter()) {
			assert_eq!(i.0, j.0);
			for (x, y) in i.1.iter().zip(j.1.iter()) {
				assert_eq!(x, y);
			}
		}
		assert_eq_uvec!(x, expected);

		assert_eq!(Scheduler::current_storage_version(), 3);
	});
}

#[test]
fn test_migrate_origin() {
	new_test_ext().execute_with(|| {
		for i in 0..3u64 {
			let k = i.twox_64_concat();
			let old: Vec<Option<Scheduled<Bounded<Call>, u64, u32, u64>>> = vec![
				Some(Scheduled {
					maybe_id: None,
					priority: i as u8 + 10,
					call: Preimage::bound(Call::Logger(LoggerCall::log { i: 96, weight: 100 }))
						.unwrap(),
					origin: 3u32,
					maybe_periodic: None,
					_phantom: Default::default(),
				}),
				None,
				Some(Scheduled {
					maybe_id: Some(blake2_256(&b"test"[..])),
					priority: 123,
					origin: 2u32,
					call: Preimage::bound(Call::Logger(LoggerCall::log { i: 69, weight: 1000 }))
						.unwrap(),
					maybe_periodic: Some((456u64, 10)),
					_phantom: Default::default(),
				}),
			];
			frame_support::migration::put_storage_value(b"Scheduler", b"Agenda", &k, old);
		}

		impl Into<OriginCaller> for u32 {
			fn into(self) -> OriginCaller {
				match self {
					3u32 => system::RawOrigin::Root.into(),
					2u32 => system::RawOrigin::None.into(),
					_ => unreachable!("test make no use of it"),
				}
			}
		}

		Scheduler::migrate_origin::<u32>();

		assert_eq_uvec!(
			Agenda::<Test>::iter().collect::<Vec<_>>(),
			vec![
				(
					0,
					vec![
						Some(ScheduledOf::<Test> {
							maybe_id: None,
							priority: 10,
							call: Preimage::bound(Call::Logger(LoggerCall::log {
								i: 96,
								weight: 100
							}))
							.unwrap(),
							maybe_periodic: None,
							origin: system::RawOrigin::Root.into(),
							_phantom: PhantomData::<u64>::default(),
						}),
						None,
						Some(Scheduled {
							maybe_id: Some(blake2_256(&b"test"[..])),
							priority: 123,
							call: Preimage::bound(Call::Logger(LoggerCall::log {
								i: 69,
								weight: 1000
							}))
							.unwrap(),
							maybe_periodic: Some((456u64, 10)),
							origin: system::RawOrigin::None.into(),
							_phantom: PhantomData::<u64>::default(),
						}),
					]
				),
				(
					1,
					vec![
						Some(Scheduled {
							maybe_id: None,
							priority: 11,
							call: Preimage::bound(Call::Logger(LoggerCall::log {
								i: 96,
								weight: 100
							}))
							.unwrap(),
							maybe_periodic: None,
							origin: system::RawOrigin::Root.into(),
							_phantom: PhantomData::<u64>::default(),
						}),
						None,
						Some(Scheduled {
							maybe_id: Some(blake2_256(&b"test"[..])),
							priority: 123,
							call: Preimage::bound(Call::Logger(LoggerCall::log {
								i: 69,
								weight: 1000
							}))
							.unwrap(),
							maybe_periodic: Some((456u64, 10)),
							origin: system::RawOrigin::None.into(),
							_phantom: PhantomData::<u64>::default(),
						}),
					]
				),
				(
					2,
					vec![
						Some(Scheduled {
							maybe_id: None,
							priority: 12,
							call: Preimage::bound(Call::Logger(LoggerCall::log {
								i: 96,
								weight: 100
							}))
							.unwrap(),
							maybe_periodic: None,
							origin: system::RawOrigin::Root.into(),
							_phantom: PhantomData::<u64>::default(),
						}),
						None,
						Some(Scheduled {
							maybe_id: Some(blake2_256(&b"test"[..])),
							priority: 123,
							call: Preimage::bound(Call::Logger(LoggerCall::log {
								i: 69,
								weight: 1000
							}))
							.unwrap(),
							maybe_periodic: Some((456u64, 10)),
							origin: system::RawOrigin::None.into(),
							_phantom: PhantomData::<u64>::default(),
						}),
					]
				)
			]
		);
	});
}
