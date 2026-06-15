// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M3 proof for the on-chain continuous (king-of-the-hill) arena. A `RecordBeat`
/// pays its MARGINAL improvement and conserves the pool; sub-epsilon beats and
/// regressions revert; across a monotone sequence the total paid equals
/// `weiPerMicro * (finalBest - baseline)` (the frontier is bought exactly once);
/// pool exhaustion blocks further payout; the `RecordBeat` events ARE the verifiable
/// leaderboard (anyone replays them to recompute ranks + payouts); and an
/// unauthorized `recordBeat` reverts.
///
/// `recordBeat` is authority-gated to the proposer (the record authority / referee).
/// On-chain k-of-n EIP-712 verification of the certified score is the documented
/// seam (see `recordBeat`), identical to the dispute spine.
contract CompetitionManagerContinuousTest is Test {
    CompetitionManager internal mgr;

    address internal proposer = address(0x9405);
    address internal alice = address(0xA11CE);
    address internal bob = address(0xB0B);
    address internal carol = address(0xCA401);

    uint64 internal constant COMP = 1;
    uint64 internal constant DEADLINE = 1000;
    // 1 gwei per micro-point; epsilon = 0.01 point (10_000 micros).
    uint256 internal constant WEI_PER_MICRO = 1_000_000_000;
    int256 internal constant EPSILON = 10_000;
    int256 internal constant BASELINE = 0;

    bytes32 internal constant CAND_A = keccak256("cand-a");
    bytes32 internal constant CAND_B = keccak256("cand-b");
    bytes32 internal constant CAND_C = keccak256("cand-c");

    function setUp() public {
        mgr = new CompetitionManager();
        vm.deal(proposer, 100 ether);
    }

    /// Open a RecordBounty arena with a pool large enough to cover the test lifts.
    function _createRecordBounty(uint256 pool) internal {
        vm.prank(proposer);
        mgr.createContinuousCompetition{ value: pool }(COMP, DEADLINE, EPSILON, WEI_PER_MICRO, BASELINE);
    }

    function _beat(address researcher, bytes32 cand, int256 newBest, uint256 marginalWei) internal {
        vm.prank(proposer);
        mgr.recordBeat(COMP, researcher, cand, newBest, marginalWei);
    }

    // --- record beat pays marginal + conserves -----------------------------

    function test_record_beat_pays_marginal_and_conserves() public {
        uint256 pool = 1 ether;
        _createRecordBounty(pool);

        // alice: 0 -> 100_000 micros, marginal 100_000 * 1 gwei.
        uint256 m1 = uint256(100_000) * WEI_PER_MICRO;
        _beat(alice, CAND_A, 100_000, m1);
        assertEq(alice.balance, m1);
        assertEq(mgr.continuousBest(COMP), 100_000);
        assertEq(mgr.continuousSpent(COMP), m1);
        assertEq(mgr.continuousTopHolder(COMP), alice);
        // Escrow drew down by exactly the marginal; conservation holds.
        assertEq(mgr.escrowOf(COMP), pool - m1);
        assertEq(address(mgr).balance, pool - m1);

        // bob raises to 160_000: pays only the 60_000 marginal over alice.
        uint256 m2 = uint256(60_000) * WEI_PER_MICRO;
        _beat(bob, CAND_B, 160_000, m2);
        assertEq(bob.balance, m2);
        assertEq(mgr.continuousBest(COMP), 160_000);
        assertEq(mgr.continuousSpent(COMP), m1 + m2);
        assertEq(mgr.continuousTopHolder(COMP), bob);
        assertEq(mgr.escrowOf(COMP), pool - m1 - m2);
    }

    // --- sub-epsilon + regression revert -----------------------------------

    function test_sub_epsilon_beat_reverts_and_does_not_move_bar() public {
        _createRecordBounty(1 ether);
        _beat(alice, CAND_A, 100_000, uint256(100_000) * WEI_PER_MICRO);

        // +5_000 over alice: below epsilon (10_000) => revert, bar unchanged.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.SubEpsilonBeat.selector, int256(105_000), int256(100_000), EPSILON)
        );
        mgr.recordBeat(COMP, bob, CAND_B, 105_000, uint256(5_000) * WEI_PER_MICRO);
        assertEq(mgr.continuousBest(COMP), 100_000);
        assertEq(mgr.continuousTopHolder(COMP), alice);
    }

    function test_regression_reverts() public {
        _createRecordBounty(1 ether);
        _beat(alice, CAND_A, 200_000, uint256(200_000) * WEI_PER_MICRO);

        // A worse score than the standing best => negative marginal => revert.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.SubEpsilonBeat.selector, int256(120_000), int256(200_000), EPSILON)
        );
        mgr.recordBeat(COMP, bob, CAND_B, 120_000, 0);
        assertEq(mgr.continuousBest(COMP), 200_000);
        assertEq(mgr.continuousTopHolder(COMP), alice);
    }

    // --- exact marginal arithmetic is enforced -----------------------------

    function test_wrong_marginal_wei_reverts() public {
        _createRecordBounty(1 ether);
        // Claim a marginal that does not match weiPerMicro * (newBest - best).
        uint256 expected = uint256(100_000) * WEI_PER_MICRO;
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.MarginalMismatch.selector, expected + 1, expected)
        );
        mgr.recordBeat(COMP, alice, CAND_A, 100_000, expected + 1);
    }

    // --- total over a monotone sequence == weiPerMicro*(final - baseline) ---

    function test_total_over_monotone_sequence_equals_full_lift() public {
        uint256 pool = 1 ether;
        _createRecordBounty(pool);

        // A strengthening sequence; each beat pays its own marginal.
        int256[3] memory bests = [int256(100_000), int256(160_000), int256(399_000)];
        address[3] memory who = [alice, bob, alice];
        bytes32[3] memory cands = [CAND_A, CAND_B, CAND_C];
        int256 prev = BASELINE;
        for (uint256 i = 0; i < 3; i++) {
            uint256 marginal = uint256(bests[i] - prev) * WEI_PER_MICRO;
            _beat(who[i], cands[i], bests[i], marginal);
            prev = bests[i];
        }

        int256 finalBest = bests[2];
        uint256 expectedTotal = uint256(finalBest - BASELINE) * WEI_PER_MICRO;
        // The frontier is bought exactly once: total spent == full lift over baseline.
        assertEq(mgr.continuousSpent(COMP), expectedTotal);
        assertEq(alice.balance + bob.balance, expectedTotal);
        assertEq(mgr.escrowOf(COMP), pool - expectedTotal);
        assertEq(mgr.continuousBest(COMP), finalBest);
    }

    // --- pool exhaustion blocks further payout -----------------------------

    function test_pool_exhaustion_blocks_further_payout() public {
        // Pool covers only part of the first record's marginal.
        uint256 pool = uint256(30_000) * WEI_PER_MICRO; // 3e13 wei
        _createRecordBounty(pool);

        // First record owes 100_000 * 1 gwei = 1e14 but the pool is 3e13 => the
        // marginal exceeds the remaining pool and the over-pay reverts (conservation).
        uint256 owed = uint256(100_000) * WEI_PER_MICRO;
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.Overdistribution.selector, owed, pool)
        );
        mgr.recordBeat(COMP, alice, CAND_A, 100_000, owed);
        // Nothing paid, bar unmoved, escrow intact.
        assertEq(alice.balance, 0);
        assertEq(mgr.continuousBest(COMP), 0);
        assertEq(mgr.escrowOf(COMP), pool);

        // A beat sized to exactly the pool succeeds and exhausts it; the next reverts.
        _beat(alice, CAND_A, 30_000, pool);
        assertEq(alice.balance, pool);
        assertEq(mgr.escrowOf(COMP), 0);

        uint256 nextOwed = uint256(20_000) * WEI_PER_MICRO;
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.Overdistribution.selector, nextOwed, 0)
        );
        mgr.recordBeat(COMP, bob, CAND_B, 50_000, nextOwed);
    }

    // --- events emitted for recomputation ----------------------------------

    function test_record_beat_emits_verifiable_leaderboard_event() public {
        _createRecordBounty(1 ether);
        uint256 marginal = uint256(100_000) * WEI_PER_MICRO;
        vm.expectEmit(true, true, true, true);
        emit CompetitionManager.RecordBeat(COMP, alice, CAND_A, 100_000, marginal, 0);
        vm.prank(proposer);
        mgr.recordBeat(COMP, alice, CAND_A, 100_000, marginal);
    }

    // --- unauthorized recordBeat reverts -----------------------------------

    function test_unauthorized_record_beat_reverts() public {
        _createRecordBounty(1 ether);
        vm.prank(bob); // not the proposer/record authority
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotRecordAuthority.selector, bob, proposer)
        );
        mgr.recordBeat(COMP, alice, CAND_A, 100_000, uint256(100_000) * WEI_PER_MICRO);
    }

    function test_record_beat_on_non_continuous_reverts() public {
        // A plain (non-continuous) competition has no continuous state.
        vm.prank(proposer);
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotContinuous.selector, COMP)
        );
        mgr.recordBeat(COMP, alice, CAND_A, 100_000, uint256(100_000) * WEI_PER_MICRO);
    }

    // --- TimeAtTopStreaming -----------------------------------------------

    function test_time_at_top_credits_holder_per_epoch_and_conserves() public {
        uint256 weiPerEpoch = 1_000;
        uint256 pool = 2_500; // covers 2.5 epochs
        vm.prank(proposer);
        mgr.createContinuousStreaming{ value: pool }(COMP, DEADLINE, EPSILON, weiPerEpoch, BASELINE);

        // No top holder yet => ticking reverts (nothing to credit).
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NoTopHolder.selector, COMP)
        );
        mgr.tickEpoch(COMP);

        // alice seizes the top spot; a streaming record pays 0 on the beat.
        _beat(alice, CAND_A, 100_000, 0);
        assertEq(alice.balance, 0);
        assertEq(mgr.continuousTopHolder(COMP), alice);

        // Two full epochs credited, then a clamped partial, then nothing.
        vm.prank(proposer);
        assertEq(mgr.tickEpoch(COMP), 1_000);
        vm.prank(proposer);
        assertEq(mgr.tickEpoch(COMP), 1_000);
        vm.prank(proposer);
        assertEq(mgr.tickEpoch(COMP), 500); // only 500 left
        vm.prank(proposer);
        assertEq(mgr.tickEpoch(COMP), 0); // pool exhausted, no panic

        assertEq(alice.balance, 2_500);
        assertEq(mgr.continuousSpent(COMP), 2_500);
        assertEq(mgr.escrowOf(COMP), 0);
    }

    function test_streaming_record_with_nonzero_marginal_reverts() public {
        vm.prank(proposer);
        mgr.createContinuousStreaming{ value: 1 ether }(COMP, DEADLINE, EPSILON, 1_000, BASELINE);
        // Under streaming the beat must carry marginalWei == 0.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.MarginalMismatch.selector, uint256(1), uint256(0))
        );
        mgr.recordBeat(COMP, alice, CAND_A, 100_000, 1);
    }

    function test_streaming_handoff_credits_new_holder() public {
        uint256 weiPerEpoch = 100;
        vm.prank(proposer);
        mgr.createContinuousStreaming{ value: 1 ether }(COMP, DEADLINE, EPSILON, weiPerEpoch, BASELINE);

        _beat(alice, CAND_A, 100_000, 0);
        vm.prank(proposer);
        mgr.tickEpoch(COMP); // credits alice
        _beat(bob, CAND_B, 200_000, 0);
        vm.prank(proposer);
        mgr.tickEpoch(COMP); // credits bob (new holder)

        assertEq(alice.balance, weiPerEpoch);
        assertEq(bob.balance, weiPerEpoch);
        assertEq(mgr.continuousTopHolder(COMP), bob);
    }

    // --- creation guards ---------------------------------------------------

    function test_create_continuous_with_zero_pool_reverts() public {
        vm.prank(proposer);
        vm.expectRevert(CompetitionManager.EmptyPool.selector);
        mgr.createContinuousCompetition{ value: 0 }(COMP, DEADLINE, EPSILON, WEI_PER_MICRO, BASELINE);
    }

    function test_create_continuous_duplicate_reverts() public {
        _createRecordBounty(1 ether);
        // The base competition id already exists; a second create reverts there.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.CompetitionExists.selector, COMP)
        );
        mgr.createContinuousCompetition{ value: 1 ether }(COMP, DEADLINE, EPSILON, WEI_PER_MICRO, BASELINE);
    }

    // --- lifecycle: distribute() closes the continuous window --------------

    /// `distribute()` is terminal. After settlement, neither `recordBeat` nor
    /// `tickEpoch` may pay out the residual escrow (`ContinuousClosed`). This makes
    /// "settled" actually terminal for continuous competitions and closes the
    /// unbounded post-settlement payout window.
    function test_record_beat_reverts_after_settle() public {
        uint256 pool = 1 ether;
        _createRecordBounty(pool);
        uint256 m1 = uint256(100_000) * WEI_PER_MICRO;
        _beat(alice, CAND_A, 100_000, m1);

        // Settle the residual escrow at/after the deadline (the dust stays escrowed).
        vm.warp(DEADLINE);
        vm.prank(proposer);
        mgr.distribute(COMP, new address[](0), new uint256[](0));
        assertTrue(mgr.isSettled(COMP));

        // A further recordBeat against the settled competition reverts.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.ContinuousClosed.selector, COMP)
        );
        mgr.recordBeat(COMP, bob, CAND_B, 200_000, uint256(40_000) * WEI_PER_MICRO);
        // The best did not move and no further escrow was spent.
        assertEq(mgr.continuousBest(COMP), 100_000);
        assertEq(mgr.continuousSpent(COMP), m1);
    }

    function test_tick_epoch_reverts_after_settle() public {
        uint256 weiPerEpoch = 1_000;
        uint256 pool = 1 ether;
        vm.prank(proposer);
        mgr.createContinuousStreaming{ value: pool }(COMP, DEADLINE, EPSILON, weiPerEpoch, BASELINE);
        _beat(alice, CAND_A, 100_000, 0);

        vm.warp(DEADLINE);
        vm.prank(proposer);
        mgr.distribute(COMP, new address[](0), new uint256[](0));
        assertTrue(mgr.isSettled(COMP));

        // tickEpoch can no longer credit the holder out of the residual escrow.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.ContinuousClosed.selector, COMP)
        );
        mgr.tickEpoch(COMP);
    }
}
