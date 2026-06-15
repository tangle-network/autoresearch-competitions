// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M4 proof for the on-chain privacy surface: a competition can DECLARE a privacy
/// tier + required TEE, and the Referee authority can COMMIT a per-candidate
/// attestation hash that is read back for recomputation / dispute.
///
/// HONESTY (docs/PRIVACY.md §12): these tests assert ONLY the hash COMMITMENT and the
/// authority gate. The contract does NOT verify the attestation — on-chain quote
/// signature verification (DCAP/KDS/NSM) + measurement pinning + nonce binding are
/// the unimplemented seam. "Attestation committed" does NOT mean "attestation valid";
/// the cryptographic verification is off-chain / unimplemented, so nothing here
/// asserts a verified quote.
contract CompetitionManagerPrivacyTest is Test {
    CompetitionManager internal mgr;

    address internal proposer = address(0x9405);
    address internal referee = address(0x9405); // the proposer IS the attestation authority (same seam as recordBeat)
    address internal stranger = address(0xBAD);

    uint64 internal constant COMP = 1;
    uint64 internal constant DEADLINE = 1000;

    // Mirror autoresearch_runtime::privacy::PrivacyTier / attestation::TeeType.
    uint8 internal constant TIER_BLACK_BOX = 0;
    uint8 internal constant TIER_REDACTED = 1;
    uint8 internal constant TIER_WHITEBOX = 2;
    uint8 internal constant TIER_ATTESTED = 3;
    uint8 internal constant TEE_NONE = 0;
    uint8 internal constant TEE_PHALA_TDX = 1;

    bytes32 internal constant CANDIDATE = keccak256("candidate-1");
    bytes32 internal constant ATTEST_HASH = keccak256("attestation-report-bytes");

    function setUp() public {
        mgr = new CompetitionManager();
        vm.deal(proposer, 100 ether);
    }

    function _create() internal {
        vm.prank(proposer);
        mgr.createCompetition{ value: 1 ether }(COMP, DEADLINE);
    }

    // --- tier storage ------------------------------------------------------

    function test_privacy_tier_defaults_to_black_box() public {
        _create();
        // Never set => 0 (BlackBox / None), the privacy-easy default.
        assertEq(mgr.privacyTier(COMP), TIER_BLACK_BOX);
        assertEq(mgr.requiredTee(COMP), TEE_NONE);
    }

    function test_set_privacy_stores_tier_and_tee() public {
        _create();
        vm.prank(proposer);
        mgr.setPrivacy(COMP, TIER_WHITEBOX, TEE_PHALA_TDX);
        assertEq(mgr.privacyTier(COMP), TIER_WHITEBOX);
        assertEq(mgr.requiredTee(COMP), TEE_PHALA_TDX);
    }

    function test_set_privacy_emits_event() public {
        _create();
        vm.expectEmit(true, false, false, true);
        emit CompetitionManager.PrivacySet(COMP, TIER_ATTESTED, TEE_PHALA_TDX);
        vm.prank(proposer);
        mgr.setPrivacy(COMP, TIER_ATTESTED, TEE_PHALA_TDX);
    }

    function test_set_privacy_by_non_proposer_reverts() public {
        _create();
        vm.prank(stranger);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotProposer.selector, stranger, proposer)
        );
        mgr.setPrivacy(COMP, TIER_REDACTED, TEE_NONE);
    }

    function test_set_privacy_unknown_competition_reverts() public {
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.UnknownCompetition.selector, COMP)
        );
        mgr.setPrivacy(COMP, TIER_BLACK_BOX, TEE_NONE);
    }

    // --- attestation hash commitment + read-back ---------------------------

    function test_commit_attestation_then_read_back() public {
        _create();

        // The Referee authority (the proposer) commits the structural attestation
        // hash for a scored candidate. This is a COMMITMENT, not a verification.
        vm.prank(referee);
        mgr.commitAttestation(COMP, CANDIDATE, ATTEST_HASH);

        // Read it back for recomputation / dispute via both the mapping and the view.
        assertEq(mgr.attestationHashes(COMP, CANDIDATE), ATTEST_HASH);
        assertEq(mgr.attestationOf(COMP, CANDIDATE), ATTEST_HASH);

        // The stored value is exactly the off-chain keccak commitment — the chain does
        // NOT recompute or verify a quote here (that is the §12 seam). The test only
        // proves the commitment round-trips, never that the attestation is valid.
    }

    function test_commit_attestation_emits_event() public {
        _create();
        vm.expectEmit(true, true, false, true);
        emit CompetitionManager.AttestationCommitted(COMP, CANDIDATE, ATTEST_HASH);
        vm.prank(referee);
        mgr.commitAttestation(COMP, CANDIDATE, ATTEST_HASH);
    }

    function test_unauthorized_commit_attestation_reverts() public {
        _create();
        // A non-authority cannot commit an attestation hash — the off-chain Referee's
        // reports are routed only through the proposer authority (same seam as
        // recordBeat / resolveDispute).
        vm.prank(stranger);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotAttestationAuthority.selector, stranger, proposer)
        );
        mgr.commitAttestation(COMP, CANDIDATE, ATTEST_HASH);
        // Nothing was stored.
        assertEq(mgr.attestationOf(COMP, CANDIDATE), bytes32(0));
    }

    function test_commit_empty_attestation_reverts() public {
        _create();
        vm.prank(referee);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.EmptyAttestationHash.selector, COMP, CANDIDATE)
        );
        mgr.commitAttestation(COMP, CANDIDATE, bytes32(0));
    }

    function test_commit_attestation_unknown_competition_reverts() public {
        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.UnknownCompetition.selector, COMP)
        );
        mgr.commitAttestation(COMP, CANDIDATE, ATTEST_HASH);
    }

    function test_uncommitted_attestation_reads_zero() public {
        _create();
        // No commitment yet => the read-back is the zero hash ("no attestation").
        assertEq(mgr.attestationOf(COMP, CANDIDATE), bytes32(0));
    }

    function test_attestation_commitment_can_be_overwritten_by_authority() public {
        // A re-score (e.g. after a dispute) commits a fresh report hash; the authority
        // may overwrite the prior commitment. The latest committed report is what a
        // dispute recomputes against.
        _create();
        bytes32 second = keccak256("attestation-report-after-rescore");

        vm.prank(referee);
        mgr.commitAttestation(COMP, CANDIDATE, ATTEST_HASH);
        assertEq(mgr.attestationOf(COMP, CANDIDATE), ATTEST_HASH);

        vm.prank(referee);
        mgr.commitAttestation(COMP, CANDIDATE, second);
        assertEq(mgr.attestationOf(COMP, CANDIDATE), second);
    }

    // --- isolation: M4 storage does not disturb M1/M2/M3 -------------------

    function test_privacy_does_not_touch_escrow() public {
        _create();
        vm.prank(proposer);
        mgr.setPrivacy(COMP, TIER_WHITEBOX, TEE_PHALA_TDX);
        vm.prank(referee);
        mgr.commitAttestation(COMP, CANDIDATE, ATTEST_HASH);
        // Escrow and settlement state are untouched by the privacy surface.
        assertEq(mgr.escrowOf(COMP), 1 ether);
        assertFalse(mgr.isSettled(COMP));
    }
}
