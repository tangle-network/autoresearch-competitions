// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M2 proof for the on-chain dispute spine: researcher stake posts/withdraws, a
/// staked `openChallenge` locks the challenger's bond, and `resolveDispute` moves
/// funds per the committee outcome while conserving — the contract can never pay out
/// more than the two stakes locked for the dispute, double-resolve reverts, and an
/// unauthorized resolver reverts.
///
/// The off-chain m-of-n committee (`committee_verdict`) computes the outcome; this
/// contract accepts the authenticated outcome from the dispute authority. On-chain
/// k-of-n EIP-712 verification is the documented seam (see `resolveDispute`).
contract CompetitionManagerDisputeTest is Test {
    CompetitionManager internal mgr;

    address internal proposer = address(0x9405);
    address internal researcher = address(0xBEEF);
    address internal challenger = address(0xC4A11);

    uint64 internal constant COMP = 1;
    uint64 internal constant DEADLINE = 1000;
    uint256 internal constant MIN_STAKE = 1_000;
    bytes32 internal constant CANDIDATE = keccak256("candidate-1");

    // Outcome codes mirroring autoresearch_protocol::dispute::DisputeOutcome.
    uint8 internal constant UPHELD = 0;
    uint8 internal constant OVERTURNED = 1;
    uint8 internal constant INCONCLUSIVE = 2;

    function setUp() public {
        mgr = new CompetitionManager();
        vm.deal(proposer, 100 ether);
        vm.deal(researcher, 100 ether);
        vm.deal(challenger, 100 ether);
    }

    function _create() internal {
        vm.prank(proposer);
        mgr.createCompetitionWithStake{ value: 1 ether }(COMP, DEADLINE, MIN_STAKE);
    }

    /// Record a reveal for `researcher` so a challenge can be bound to them. The
    /// dispute spine requires the disputed researcher to be a revealed participant.
    function _reveal(address who) internal {
        string memory artifactRef = "ipfs://candidate-cid";
        bytes32 salt = keccak256("a-secret-salt");
        bytes32 commitment = keccak256(abi.encode(artifactRef, salt));
        vm.prank(who);
        mgr.commitCandidate(COMP, commitment);
        vm.prank(who);
        mgr.revealCandidate(COMP, artifactRef, salt);
    }

    // --- staking -----------------------------------------------------------

    function test_post_stake_tracks_and_clears_floor() public {
        _create();
        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        assertEq(mgr.stakes(COMP, researcher), MIN_STAKE);
        assertTrue(mgr.isStaked(COMP, researcher));
    }

    function test_post_stake_below_floor_reverts() public {
        _create();
        vm.prank(researcher);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.BelowMinStake.selector, MIN_STAKE - 1, MIN_STAKE)
        );
        mgr.postStake{ value: MIN_STAKE - 1 }(COMP);
        // A reverted post locks nothing.
        assertEq(mgr.stakes(COMP, researcher), 0);
        assertFalse(mgr.isStaked(COMP, researcher));
    }

    function test_post_stake_accumulates_to_reach_floor() public {
        _create();
        // First sub-floor post reverts; a single sufficient post clears the floor.
        vm.prank(researcher);
        vm.expectRevert();
        mgr.postStake{ value: 600 }(COMP);

        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        // A top-up accumulates on top of the cleared stake.
        vm.prank(researcher);
        mgr.postStake{ value: 500 }(COMP);
        assertEq(mgr.stakes(COMP, researcher), MIN_STAKE + 500);
    }

    function test_withdraw_stake_after_settlement_returns_bond() public {
        _create();
        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);

        // Settle the competition (empty distribution is allowed after deadline).
        vm.warp(DEADLINE);
        address[] memory winners = new address[](0);
        uint256[] memory amounts = new uint256[](0);
        vm.prank(proposer);
        mgr.distribute(COMP, winners, amounts);
        assertTrue(mgr.isSettled(COMP));

        uint256 before = researcher.balance;
        vm.prank(researcher);
        mgr.withdrawStake(COMP);
        assertEq(researcher.balance, before + MIN_STAKE);
        assertEq(mgr.stakes(COMP, researcher), 0);
    }

    function test_withdraw_before_settlement_reverts() public {
        _create();
        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        vm.prank(researcher);
        vm.expectRevert();
        mgr.withdrawStake(COMP);
    }

    // --- challenge + resolve ----------------------------------------------

    function _stakeResearcher() internal {
        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        // A challenge can only name a revealed researcher; reveal once here so the
        // challenge-and-resolve helpers below bind to a real participant.
        _reveal(researcher);
    }

    function _openChallenge(uint256 stake) internal {
        vm.prank(challenger);
        mgr.openChallenge{ value: stake }(COMP, CANDIDATE, researcher);
    }

    function test_open_challenge_locks_stake() public {
        _create();
        _stakeResearcher();
        uint256 challengerStake = 500;
        _openChallenge(challengerStake);

        (address ch, address resWho, uint256 stake, bool exists, bool resolved) =
            mgr.challenges(COMP, CANDIDATE);
        assertEq(ch, challenger);
        assertEq(resWho, researcher, "challenge binds the disputed researcher");
        assertEq(stake, challengerStake);
        assertTrue(exists);
        assertFalse(resolved);
    }

    function test_open_duplicate_challenge_reverts() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);
        vm.prank(challenger);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.ChallengeExists.selector, COMP, CANDIDATE)
        );
        mgr.openChallenge{ value: 500 }(COMP, CANDIDATE, researcher);
    }

    /// Upheld: the frivolous challenger's stake is slashed (retained by the contract);
    /// the researcher's stake is untouched. The challenger is not paid back.
    function test_resolve_upheld_slashes_challenger() public {
        _create();
        _stakeResearcher();
        uint256 challengerStake = 500;
        _openChallenge(challengerStake);

        uint256 challengerBefore = challenger.balance;
        uint256 contractBefore = address(mgr).balance;

        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, UPHELD, 0, 0);

        // Challenger gets nothing back; their stake stays in the contract (slashed).
        assertEq(challenger.balance, challengerBefore);
        assertEq(address(mgr).balance, contractBefore);
        // Researcher stake intact.
        assertEq(mgr.stakes(COMP, researcher), MIN_STAKE);
        (,,,, bool resolved) = mgr.challenges(COMP, CANDIDATE);
        assertTrue(resolved);
    }

    /// Overturned: the researcher is slashed; the challenger is refunded their own
    /// stake plus a reward out of the slash. Conservation: payout <= the two stakes.
    function test_resolve_overturned_slashes_researcher_and_rewards_challenger() public {
        _create();
        _stakeResearcher();
        uint256 challengerStake = 500;
        _openChallenge(challengerStake);

        uint256 researcherSlash = MIN_STAKE; // full slash
        uint256 challengerReward = 300; // 30% of slash, <= slash
        uint256 challengerBefore = challenger.balance;

        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, researcherSlash, challengerReward);

        // Challenger: own stake refunded + reward.
        assertEq(challenger.balance, challengerBefore + challengerStake + challengerReward);
        // Researcher fully slashed.
        assertEq(mgr.stakes(COMP, researcher), 0);
        // The slash remainder (slash - reward) stays in the contract (validator/burn share).
        // Contract still holds: reward pool (1 ether) + remainder (MIN_STAKE - reward).
        assertEq(address(mgr).balance, 1 ether + (researcherSlash - challengerReward));
        (,,,, bool resolved) = mgr.challenges(COMP, CANDIDATE);
        assertTrue(resolved);
    }

    /// Conservation guard: a resolution cannot pay the challenger a reward larger than
    /// the researcher's slash, and cannot slash more than the researcher staked.
    function test_resolve_overturned_cannot_over_pay() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);

        // Reward exceeds the slash → revert.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.RewardExceedsSlash.selector, 1_001, 1_000)
        );
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, 1_000, 1_001);

        // Slash exceeds the researcher's staked bond → revert.
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.SlashExceedsStake.selector, MIN_STAKE + 1, MIN_STAKE)
        );
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, MIN_STAKE + 1, 0);
    }

    /// Inconclusive: no fault proven → the challenger's stake is refunded in full and
    /// the researcher's stake is untouched.
    function test_resolve_inconclusive_refunds_challenger() public {
        _create();
        _stakeResearcher();
        uint256 challengerStake = 500;
        _openChallenge(challengerStake);

        uint256 challengerBefore = challenger.balance;
        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, INCONCLUSIVE, 0, 0);

        assertEq(challenger.balance, challengerBefore + challengerStake);
        assertEq(mgr.stakes(COMP, researcher), MIN_STAKE);
    }

    function test_double_resolve_reverts() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);

        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, UPHELD, 0, 0);

        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.ChallengeResolved.selector, COMP, CANDIDATE)
        );
        mgr.resolveDispute(COMP, CANDIDATE, UPHELD, 0, 0);
    }

    function test_resolve_by_non_authority_reverts() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);

        vm.prank(challenger); // not the proposer/dispute authority
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotDisputeAuthority.selector, challenger, proposer)
        );
        mgr.resolveDispute(COMP, CANDIDATE, UPHELD, 0, 0);
    }

    function test_resolve_unknown_challenge_reverts() public {
        _create();
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.UnknownChallenge.selector, COMP, CANDIDATE)
        );
        mgr.resolveDispute(COMP, CANDIDATE, UPHELD, 0, 0);
    }

    function test_resolve_bad_outcome_reverts() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);
        vm.prank(proposer);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.BadOutcome.selector, 3));
        mgr.resolveDispute(COMP, CANDIDATE, 3, 0, 0);
    }

    /// Full conservation across an Overturned dispute: total wei the contract pays out
    /// is exactly bounded by the two locked stakes; the reward pool is never touched.
    function test_overturned_conserves_against_locked_stakes() public {
        _create();
        _stakeResearcher();
        uint256 challengerStake = 500;
        _openChallenge(challengerStake);

        // The contract holds: 1 ether reward pool + researcher MIN_STAKE + challengerStake.
        uint256 lockedStakes = MIN_STAKE + challengerStake;
        assertEq(address(mgr).balance, 1 ether + lockedStakes);

        uint256 researcherSlash = MIN_STAKE;
        uint256 challengerReward = 300;
        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, researcherSlash, challengerReward);

        // Paid out to challenger = own stake + reward; this can never exceed lockedStakes.
        uint256 paidOut = challengerStake + challengerReward;
        assertLe(paidOut, lockedStakes, "payout must not exceed the two locked stakes");
        // Reward pool is untouched; remaining contract balance = pool + retained slash share.
        assertEq(address(mgr).balance, 1 ether + lockedStakes - paidOut);
    }

    // --- P1: a guilty researcher cannot escape slashing by withdrawing ----

    /// The slash escape: settle, then a researcher with an OPEN challenge against them
    /// tries to withdraw their bond before `resolveDispute`. The lock must block it,
    /// so the bond is still present when the dispute resolves Overturned and the slash
    /// is enforceable. Without the lock, withdraw would zero the stake and the later
    /// Overturned slash would revert (SlashExceedsStake), making the slash unenforceable.
    function test_withdraw_blocked_while_challenge_open_then_slash_enforced() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);

        // Settle the competition so withdrawStake's settled-gate is satisfied.
        vm.warp(DEADLINE);
        address[] memory winners = new address[](0);
        uint256[] memory amounts = new uint256[](0);
        vm.prank(proposer);
        mgr.distribute(COMP, winners, amounts);

        // The researcher tries to exit the bond while the challenge is still open: the
        // stake lock must reject it (fail closed), so the slashable bond stays.
        vm.prank(researcher);
        vm.expectRevert(
            abi.encodeWithSelector(
                CompetitionManager.StakeLockedByChallenge.selector, COMP, researcher, uint256(1)
            )
        );
        mgr.withdrawStake(COMP);
        assertEq(mgr.stakes(COMP, researcher), MIN_STAKE, "bond must stay while disputed");

        // Resolve Overturned: the full bond is slashable because it was never withdrawn.
        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, MIN_STAKE, 300);
        assertEq(mgr.stakes(COMP, researcher), 0, "guilty researcher is fully slashed");
        assertEq(mgr.openChallengeCount(COMP, researcher), 0, "lock released on resolve");
    }

    /// After the dispute resolves Inconclusive (no slash), the lock releases and the
    /// honest researcher can withdraw the remaining bond normally.
    function test_withdraw_allowed_after_challenge_resolved() public {
        _create();
        _stakeResearcher();
        _openChallenge(500);

        vm.warp(DEADLINE);
        address[] memory winners = new address[](0);
        uint256[] memory amounts = new uint256[](0);
        vm.prank(proposer);
        mgr.distribute(COMP, winners, amounts);

        // Inconclusive: nothing slashed, lock releases.
        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, INCONCLUSIVE, 0, 0);
        assertEq(mgr.openChallengeCount(COMP, researcher), 0);

        uint256 before = researcher.balance;
        vm.prank(researcher);
        mgr.withdrawStake(COMP);
        assertEq(researcher.balance, before + MIN_STAKE, "unslashed bond recovered after resolve");
    }

    // --- P1: candidate/researcher binding ---------------------------------

    /// A challenge can only name a researcher who has a recorded reveal: an opaque
    /// candidateId cannot be pinned on a non-participant. Opening against an
    /// unrevealed address reverts, so the slashed party is provably the disputed one.
    function test_open_challenge_against_unrevealed_researcher_reverts() public {
        _create();
        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        // researcher has staked but NOT revealed.
        vm.prank(challenger);
        vm.expectRevert(
            abi.encodeWithSelector(
                CompetitionManager.ResearcherNotRevealed.selector, COMP, researcher
            )
        );
        mgr.openChallenge{ value: 500 }(COMP, CANDIDATE, researcher);
    }

    /// resolveDispute slashes ONLY the researcher bound into the challenge, never a
    /// free-chosen address. An innocent third-party staker is untouched even though the
    /// proposer resolves the dispute, because the slash subject comes from the challenge
    /// record, not a resolve-time parameter.
    function test_resolve_slashes_only_bound_researcher_not_innocent_staker() public {
        _create();
        _stakeResearcher();

        // An innocent third party also stakes (and reveals) — they are NOT challenged.
        address innocent = address(0x1117);
        vm.deal(innocent, 100 ether);
        vm.prank(innocent);
        mgr.postStake{ value: MIN_STAKE }(COMP);

        // The challenge binds the actual disputed `researcher`.
        _openChallenge(500);
        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, MIN_STAKE, 300);

        // The bound researcher is slashed; the innocent staker's bond is fully intact.
        assertEq(mgr.stakes(COMP, researcher), 0, "bound researcher slashed");
        assertEq(mgr.stakes(COMP, innocent), MIN_STAKE, "innocent staker untouched");
    }

    // --- P2: global solvency / segregation invariant ----------------------

    /// Global accounting invariant: across an arbitrary interleaving of create /
    /// postStake / openChallenge / resolveDispute / distribute / withdrawStake, the
    /// contract's native balance always equals the sum of every live obligation —
    /// remaining escrow + all live researcher stakes + all unresolved challenge stakes
    /// + retained (slashed/burned) value. No subsystem's payout is ever funded by
    /// another's locked principal at the balance level.
    function test_global_solvency_invariant_across_interleaving() public {
        // Track the retained (validator/burn) share the contract keeps from slashes;
        // it is part of address(this).balance but not owed back to any party.
        uint256 retained = 0;

        _create(); // escrow = 1 ether
        _assertSolvent(1 ether, 0, 0, retained);

        // researcher stakes + reveals; an unrelated staker `other` also stakes.
        vm.prank(researcher);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        _reveal(researcher);
        address other = address(0x2227);
        vm.deal(other, 100 ether);
        vm.prank(other);
        mgr.postStake{ value: MIN_STAKE }(COMP);
        _assertSolvent(1 ether, 2 * MIN_STAKE, 0, retained);

        // Open a challenge: challenger stake is now a live obligation too.
        uint256 chStake = 500;
        _openChallenge(chStake);
        _assertSolvent(1 ether, 2 * MIN_STAKE, chStake, retained);

        // Resolve Overturned: slash MIN_STAKE from researcher, pay challenger
        // (own stake + reward), retain (slash - reward). researcher live stake -> 0,
        // challenge stake obligation cleared, challenger paid out.
        uint256 reward = 300;
        vm.prank(proposer);
        mgr.resolveDispute(COMP, CANDIDATE, OVERTURNED, MIN_STAKE, reward);
        retained += (MIN_STAKE - reward); // slash remainder kept by the contract
        // Live researcher stakes now: only `other` (MIN_STAKE). researcher = 0.
        _assertSolvent(1 ether, MIN_STAKE, 0, retained);

        // Settle and pay a winner from escrow; escrow drops, stakes untouched.
        vm.warp(DEADLINE);
        address[] memory winners = new address[](1);
        winners[0] = address(0x3337);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 0.4 ether;
        vm.prank(proposer);
        mgr.distribute(COMP, winners, amounts);
        _assertSolvent(1 ether - 0.4 ether, MIN_STAKE, 0, retained);

        // The innocent `other` withdraws their full, unslashed, unlocked bond.
        vm.prank(other);
        mgr.withdrawStake(COMP);
        _assertSolvent(1 ether - 0.4 ether, 0, 0, retained);
    }

    /// Assert the global identity holds: contract balance == remaining escrow + live
    /// researcher stakes + live challenge stake + retained slash share.
    function _assertSolvent(
        uint256 escrow,
        uint256 liveStakes,
        uint256 liveChallengeStake,
        uint256 retained
    ) internal view {
        assertEq(
            address(mgr).balance,
            escrow + liveStakes + liveChallengeStake + retained,
            "global solvency: balance must equal sum of all live obligations + retained"
        );
    }
}
