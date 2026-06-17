// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M5 proof for the on-chain scorer-kind record: a competition can DECLARE which
/// scorer class adjudicated it (held-out eval / private oracle / privileged hardware /
/// human panel), so the verifiable leaderboard carries the referee class behind each
/// payout. The scoring itself is OFF-CHAIN by design — the chain never runs an oracle,
/// a privileged device, or a human panel; it only records the declared kind.
contract CompetitionManagerScorerKindTest is Test {
    CompetitionManager internal mgr;

    address internal proposer = address(0x9405);
    address internal stranger = address(0xBAD);

    uint64 internal constant COMP = 1;
    uint64 internal constant DEADLINE = 1000;

    // Mirror autoresearch_runtime::types::ScorerKind.
    uint8 internal constant KIND_HELD_OUT_EVAL = 0;
    uint8 internal constant KIND_PRIVATE_ORACLE = 1;
    uint8 internal constant KIND_PRIVILEGED_HARDWARE = 2;
    uint8 internal constant KIND_HUMAN_PANEL = 3;

    function setUp() public {
        mgr = new CompetitionManager();
        vm.deal(proposer, 100 ether);
    }

    function _create() internal {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);
    }

    // --- default value -----------------------------------------------------

    function test_scorer_kind_defaults_to_held_out_eval() public {
        _create();
        // Never set => 0 (HeldOutEval), the agent-profile stand-in default.
        assertEq(mgr.scorerKind(COMP), KIND_HELD_OUT_EVAL);
        assertEq(mgr.scorerKindOf(COMP), KIND_HELD_OUT_EVAL);
    }

    // --- store + read each kind -------------------------------------------

    function test_set_scorer_kind_stores_and_reads_back() public {
        _create();
        vm.prank(proposer);
        mgr.setScorerKind(COMP, KIND_PRIVATE_ORACLE);
        // Read back via both the mapping and the view.
        assertEq(mgr.scorerKind(COMP), KIND_PRIVATE_ORACLE);
        assertEq(mgr.scorerKindOf(COMP), KIND_PRIVATE_ORACLE);
    }

    function test_set_each_valid_scorer_kind() public {
        _create();
        uint8[4] memory kinds =
            [KIND_HELD_OUT_EVAL, KIND_PRIVATE_ORACLE, KIND_PRIVILEGED_HARDWARE, KIND_HUMAN_PANEL];
        for (uint256 i = 0; i < kinds.length; i++) {
            vm.prank(proposer);
            mgr.setScorerKind(COMP, kinds[i]);
            assertEq(mgr.scorerKindOf(COMP), kinds[i]);
        }
    }

    function test_set_scorer_kind_emits_event() public {
        _create();
        vm.expectEmit(true, false, false, true);
        emit CompetitionManager.ScorerKindSet(COMP, KIND_HUMAN_PANEL);
        vm.prank(proposer);
        mgr.setScorerKind(COMP, KIND_HUMAN_PANEL);
    }

    // --- authority gate ----------------------------------------------------

    function test_set_scorer_kind_by_non_proposer_reverts() public {
        _create();
        vm.prank(stranger);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotScorerAuthority.selector, stranger, proposer)
        );
        mgr.setScorerKind(COMP, KIND_PRIVATE_ORACLE);
        // Nothing was changed.
        assertEq(mgr.scorerKindOf(COMP), KIND_HELD_OUT_EVAL);
    }

    function test_set_scorer_kind_unknown_competition_reverts() public {
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.UnknownCompetition.selector, COMP)
        );
        mgr.setScorerKind(COMP, KIND_PRIVATE_ORACLE);
    }

    // --- range check -------------------------------------------------------

    function test_set_out_of_range_scorer_kind_reverts() public {
        _create();
        vm.prank(proposer);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.BadScorerKind.selector, uint8(4)));
        mgr.setScorerKind(COMP, 4);
        // Unchanged.
        assertEq(mgr.scorerKindOf(COMP), KIND_HELD_OUT_EVAL);
    }

    // --- isolation: M5 storage does not disturb M1/M4 ----------------------

    function test_scorer_kind_does_not_touch_escrow_or_privacy() public {
        _create();
        vm.prank(proposer);
        mgr.setScorerKind(COMP, KIND_PRIVILEGED_HARDWARE);
        // Escrow, settlement, and the privacy surface are untouched by the scorer-kind
        // record.
        assertEq(mgr.escrowOf(COMP), 1 ether);
        assertFalse(mgr.isSettled(COMP));
        assertEq(mgr.privacyTier(COMP), 0);
        assertEq(mgr.requiredTee(COMP), 0);
    }
}
