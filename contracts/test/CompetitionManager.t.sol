// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M1 proof for the on-chain spine: commit-reveal is cryptographically enforced,
/// escrow conserves the pool through settlement, and a competition pays out at most
/// once after its deadline.
contract CompetitionManagerTest is Test {
    CompetitionManager internal mgr;

    address internal proposer = address(0x9405);
    address internal researcher = address(0xBEEF);
    address internal alice = address(0xA11CE);
    address internal bob = address(0xB0B);
    address internal carol = address(0xCA401);

    uint64 internal constant COMP = 1;
    uint64 internal constant DEADLINE = 1000;

    function setUp() public {
        mgr = new CompetitionManager();
        vm.deal(proposer, 100 ether);
        vm.deal(address(this), 100 ether);
    }

    // --- commit-reveal -----------------------------------------------------

    function test_commit_then_correct_reveal_passes() public {
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);

        string memory artifactRef = "ipfs://candidate-cid";
        bytes32 salt = keccak256("a-secret-salt");
        bytes32 commitment = keccak256(abi.encode(artifactRef, salt));

        vm.prank(researcher);
        mgr.commitCandidate(COMP, commitment);
        assertEq(mgr.commitments(COMP, researcher), commitment);

        // Pre-validation view agrees before the state-changing reveal.
        assertTrue(mgr.verifyReveal(COMP, researcher, artifactRef, salt));

        vm.prank(researcher);
        mgr.revealCandidate(COMP, artifactRef, salt);
        assertTrue(mgr.revealed(COMP, researcher));
    }

    function test_reveal_with_wrong_ref_reverts() public {
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);

        string memory artifactRef = "ipfs://candidate-cid";
        bytes32 salt = keccak256("a-secret-salt");
        bytes32 commitment = keccak256(abi.encode(artifactRef, salt));

        vm.prank(researcher);
        mgr.commitCandidate(COMP, commitment);

        // Reveal a DIFFERENT artifact than was committed.
        vm.prank(researcher);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.RevealMismatch.selector, COMP, researcher)
        );
        mgr.revealCandidate(COMP, "ipfs://a-different-cid", salt);
        assertFalse(mgr.revealed(COMP, researcher));
    }

    function test_reveal_with_wrong_salt_reverts() public {
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);

        string memory artifactRef = "ipfs://candidate-cid";
        bytes32 commitment = keccak256(abi.encode(artifactRef, keccak256("real-salt")));

        vm.prank(researcher);
        mgr.commitCandidate(COMP, commitment);

        vm.prank(researcher);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.RevealMismatch.selector, COMP, researcher)
        );
        mgr.revealCandidate(COMP, artifactRef, keccak256("wrong-salt"));
    }

    function test_reveal_without_commitment_reverts() public {
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);
        vm.prank(researcher);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NoCommitment.selector, COMP, researcher)
        );
        mgr.revealCandidate(COMP, "ipfs://x", bytes32(0));
    }

    /// The commitment must bind the artifact reference unambiguously. Under the old
    /// `abi.encodePacked(artifactRef, salt)` scheme, two adjacent variable-length
    /// values have no length delimiter, so a researcher who committed to "ipfs://AAAA"
    /// with salt-bytes "Ax" could reveal a DIFFERENT artifact "ipfs://AAAAA" with
    /// salt-bytes "x" and pass the check (the canonical encodePacked collision
    /// keccak("ab"+"c") == keccak("a"+"bc")). `abi.encode` is length-prefixed, so the
    /// two encodings differ and the shifted reveal must revert.
    function test_encode_is_length_unambiguous_packed_was_not() public pure {
        // The packed boundary can be shifted: identical bytes, different split.
        assertEq(
            keccak256(abi.encodePacked("ipfs://AAAA", "Ax")),
            keccak256(abi.encodePacked("ipfs://AAAAA", "x")),
            "packed encoding collides across the artifactRef/salt boundary"
        );
        // The contract's encoding is length-prefixed, so the same two pairs differ.
        assertTrue(
            keccak256(abi.encode("ipfs://AAAA", "Ax")) != keccak256(abi.encode("ipfs://AAAAA", "x")),
            "abi.encode must not collide across the boundary"
        );
    }

    /// End-to-end on the on-chain reveal binding: committing to one artifact under the
    /// length-prefixed scheme makes the boundary-shifted artifact unrevealable. With
    /// the old packed scheme this reveal would have SUCCEEDED, defeating anti-copy.
    function test_boundary_shift_reveal_reverts_under_abi_encode() public {
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);

        // Commit to "ipfs://AAAA" with a salt; record the length-prefixed commitment.
        string memory committedRef = "ipfs://AAAA";
        bytes32 committedSalt = "Ax";
        bytes32 commitment = keccak256(abi.encode(committedRef, committedSalt));

        vm.prank(researcher);
        mgr.commitCandidate(COMP, commitment);

        // Attempt to reveal the boundary-shifted artifact "ipfs://AAAAA" with salt "x":
        // a genuinely different artifact. Under abi.encode the commitment does not match.
        bytes32 shiftedSalt = "x";
        vm.prank(researcher);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.RevealMismatch.selector, COMP, researcher)
        );
        mgr.revealCandidate(COMP, "ipfs://AAAAA", shiftedSalt);
        assertFalse(mgr.revealed(COMP, researcher));

        // The honestly-committed pair still reveals cleanly.
        vm.prank(researcher);
        mgr.revealCandidate(COMP, committedRef, committedSalt);
        assertTrue(mgr.revealed(COMP, researcher));
    }

    // --- escrow + distribute ----------------------------------------------

    function test_create_locks_escrow() public {
        mgr.createCompetition{ value: 5 ether }(COMP, DEADLINE);
        assertEq(mgr.escrowOf(COMP), 5 ether);
        assertEq(address(mgr).balance, 5 ether);
    }

    function test_create_with_zero_pool_reverts() public {
        vm.expectRevert(CompetitionManager.EmptyPool.selector);
        mgr.createCompetition{ value: 0 }(COMP, DEADLINE);
    }

    function test_create_duplicate_reverts() public {
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.CompetitionExists.selector, COMP)
        );
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);
    }

    function test_distribute_pays_winners_and_conserves() public {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1_000_000 }(COMP, DEADLINE);

        vm.warp(DEADLINE); // settlement allowed on/after deadline

        address[] memory winners = new address[](3);
        winners[0] = alice;
        winners[1] = bob;
        winners[2] = carol;
        uint256[] memory amounts = new uint256[](3);
        amounts[0] = 500_000;
        amounts[1] = 300_000;
        amounts[2] = 200_000;

        vm.prank(proposer);
        mgr.distribute(COMP, winners, amounts);

        // Conservation: every wei accounted for, nothing minted, nothing stranded.
        assertEq(alice.balance, 500_000);
        assertEq(bob.balance, 300_000);
        assertEq(carol.balance, 200_000);
        assertEq(mgr.escrowOf(COMP), 0);
        assertEq(address(mgr).balance, 0);
        assertTrue(mgr.isSettled(COMP));
    }

    function test_distribute_cannot_over_distribute() public {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1_000_000 }(COMP, DEADLINE);
        vm.warp(DEADLINE);

        address[] memory winners = new address[](1);
        winners[0] = alice;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1_000_001; // one wei over the pool

        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.Overdistribution.selector, 1_000_001, 1_000_000)
        );
        mgr.distribute(COMP, winners, amounts);
        // Failed settlement leaves escrow intact.
        assertEq(mgr.escrowOf(COMP), 1_000_000);
        assertFalse(mgr.isSettled(COMP));
    }

    function test_distribute_before_deadline_reverts() public {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1_000_000 }(COMP, DEADLINE);

        vm.warp(DEADLINE - 1);
        address[] memory winners = new address[](1);
        winners[0] = alice;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1;

        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.BeforeDeadline.selector, uint64(DEADLINE - 1), DEADLINE)
        );
        mgr.distribute(COMP, winners, amounts);
    }

    function test_distribute_by_non_proposer_reverts() public {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1_000_000 }(COMP, DEADLINE);
        vm.warp(DEADLINE);

        address[] memory winners = new address[](1);
        winners[0] = alice;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1;

        vm.prank(bob); // not the proposer
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotProposer.selector, bob, proposer)
        );
        mgr.distribute(COMP, winners, amounts);
    }

    function test_double_settle_reverts() public {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1_000_000 }(COMP, DEADLINE);
        vm.warp(DEADLINE);

        address[] memory winners = new address[](1);
        winners[0] = alice;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 400_000;

        vm.prank(proposer);
        mgr.distribute(COMP, winners, amounts);
        assertTrue(mgr.isSettled(COMP));

        // Second settlement attempt — even within budget — must revert.
        uint256[] memory amounts2 = new uint256[](1);
        amounts2[0] = 100_000;
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.AlreadySettled.selector, COMP)
        );
        mgr.distribute(COMP, winners, amounts2);
    }

    function test_length_mismatch_reverts() public {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1_000_000 }(COMP, DEADLINE);
        vm.warp(DEADLINE);

        address[] memory winners = new address[](2);
        winners[0] = alice;
        winners[1] = bob;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1;

        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.LengthMismatch.selector, 2, 1)
        );
        mgr.distribute(COMP, winners, amounts);
    }
}
