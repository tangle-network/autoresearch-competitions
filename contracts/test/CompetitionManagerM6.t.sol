// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M6 proof for the on-chain spine: the COLLABORATIVE contribution settlement
/// (`distributeShares`) and the certified-artifact MARKETPLACE (`listArtifact` /
/// `buyArtifact`).
///
/// Collaborative: `distributeShares` pays contributors by share, conserves the pool
/// (sum(amounts) <= escrow), pays a zero-share free-rider nothing, settles at most once,
/// and is proposer-gated. The off-chain runner computes the held-out-gated
/// single-permutation marginal shares; the chain conserves + pays (docs/MECHANISM.md §6).
///
/// Marketplace: list + buy happy path transfers price to the seller, an exclusive
/// listing cannot be double-bought, the wrong `msg.value` reverts, and the pay-to-seller
/// settlement conserves (docs/MECHANISM.md §10).
contract CompetitionManagerM6Test is Test {
    CompetitionManager internal mgr;

    address internal proposer = address(0x9405);
    address internal stranger = address(0xBAD);
    // Collaborative contributors (GPU pools, in the production framing).
    address internal poolA = address(0xA11CE);
    address internal poolB = address(0xB0B);
    address internal poolC = address(0xCA401);
    address internal freeRider = address(0xF9EE);
    // Marketplace actors.
    address internal seller = address(0x5E11E5);
    address internal buyer = address(0xB04E5);

    uint64 internal constant COMP = 1;
    uint64 internal constant DEADLINE = 1000;

    bytes32 internal constant ARTIFACT = keccak256("collab-checkpoint-v1");
    uint64 internal constant PROVENANCE = COMP;

    function setUp() public {
        mgr = new CompetitionManager();
        vm.deal(proposer, 100 ether);
        vm.deal(buyer, 100 ether);
        vm.deal(stranger, 100 ether);
    }

    /// Tracks whether `COMP` (the provenance competition) has been opened in this test,
    /// so `_ensureProvenance` stays idempotent (a duplicate `createCompetition` reverts).
    bool internal compCreated;

    function _create(uint256 pool) internal {
        vm.prank(proposer);
        mgr.createCompetition{ value: pool }(COMP, DEADLINE);
        compCreated = true;
    }

    /// Ensure the provenance competition (`COMP`) exists. `listArtifact` now gates
    /// `provenanceCompetitionId` through `_requireExists`, so a listing's provenance must
    /// reference a real competition. Idempotent: a no-op if `COMP` is already open.
    function _ensureProvenance() internal {
        if (!compCreated) {
            // The pool is irrelevant to the marketplace path (it is escrowless); a
            // nominal pool just makes `COMP` a real competition the listing can name.
            _create(1);
        }
    }

    // =======================================================================
    // Collaborative settlement — distributeShares
    // =======================================================================

    /// Shares pay by contribution, conserve the pool, and pay a zero-share free-rider
    /// nothing. The off-chain attribution gave A 5000 / B 3000 / C 2000 bps and the
    /// free-rider 0 — the free-rider is simply omitted (or carries a 0 amount).
    function test_distribute_shares_pays_by_contribution_and_conserves() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](4);
        contributors[0] = poolA;
        contributors[1] = poolB;
        contributors[2] = poolC;
        contributors[3] = freeRider;
        uint256[] memory amounts = new uint256[](4);
        amounts[0] = 500_000; // 5000 bps
        amounts[1] = 300_000; // 3000 bps
        amounts[2] = 200_000; // 2000 bps
        amounts[3] = 0; // free-rider: zero-marginal => zero share

        vm.prank(proposer);
        mgr.distributeShares(COMP, contributors, amounts);

        // Conservation: every wei accounted for, nothing minted, nothing stranded.
        assertEq(poolA.balance, 500_000);
        assertEq(poolB.balance, 300_000);
        assertEq(poolC.balance, 200_000);
        // The free-rider with a zero share receives nothing.
        assertEq(freeRider.balance, 0);
        assertEq(mgr.escrowOf(COMP), 0);
        assertEq(address(mgr).balance, 0);
        assertTrue(mgr.isSettled(COMP));
    }

    /// A free-rider's zero-marginal share pays nothing AND the conservation holds even
    /// when the productive contributors take only part of the pool (dust stays escrowed,
    /// not minted, not paid to the free-rider).
    function test_distribute_shares_free_rider_gets_zero_dust_stays_escrowed() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](2);
        contributors[0] = poolA;
        contributors[1] = freeRider;
        uint256[] memory amounts = new uint256[](2);
        amounts[0] = 999_900; // productive share (flooring left 100 wei dust)
        amounts[1] = 0; // free-rider

        vm.prank(proposer);
        mgr.distributeShares(COMP, contributors, amounts);

        assertEq(poolA.balance, 999_900);
        assertEq(freeRider.balance, 0, "a zero-share free-rider is paid nothing");
        // The 100 wei of flooring dust stays escrowed — never minted, never paid out.
        assertEq(mgr.escrowOf(COMP), 100);
        assertTrue(mgr.isSettled(COMP));
    }

    /// distributeShares cannot pay out more than the escrowed pool.
    function test_distribute_shares_cannot_over_distribute() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](1);
        contributors[0] = poolA;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1_000_001; // one wei over the pool

        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.Overdistribution.selector, 1_000_001, 1_000_000)
        );
        mgr.distributeShares(COMP, contributors, amounts);
        // Failed settlement leaves escrow intact and the competition unsettled.
        assertEq(mgr.escrowOf(COMP), 1_000_000);
        assertFalse(mgr.isSettled(COMP));
    }

    /// distributeShares is terminal: a second settlement reverts (no double-pay).
    function test_distribute_shares_double_settle_reverts() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](1);
        contributors[0] = poolA;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 500_000;

        vm.prank(proposer);
        mgr.distributeShares(COMP, contributors, amounts);

        // A second settlement of the same competition reverts.
        vm.prank(proposer);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.AlreadySettled.selector, COMP));
        mgr.distributeShares(COMP, contributors, amounts);
        // Only the first settlement took effect.
        assertEq(poolA.balance, 500_000);
        assertEq(mgr.escrowOf(COMP), 500_000);
    }

    /// Only the proposer (settlement authority) may settle shares.
    function test_distribute_shares_by_non_proposer_reverts() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](1);
        contributors[0] = poolA;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 500_000;

        vm.prank(stranger);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.NotShareAuthority.selector, stranger, proposer)
        );
        mgr.distributeShares(COMP, contributors, amounts);
        assertFalse(mgr.isSettled(COMP));
    }

    /// distributeShares is gated until the deadline, exactly like distribute.
    function test_distribute_shares_before_deadline_reverts() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE - 1);

        address[] memory contributors = new address[](1);
        contributors[0] = poolA;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 500_000;

        vm.prank(proposer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.BeforeDeadline.selector, uint64(DEADLINE - 1), DEADLINE)
        );
        mgr.distributeShares(COMP, contributors, amounts);
    }

    /// Mismatched contributors/amounts lengths revert.
    function test_distribute_shares_length_mismatch_reverts() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](2);
        contributors[0] = poolA;
        contributors[1] = poolB;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 500_000;

        vm.prank(proposer);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.LengthMismatch.selector, 2, 1));
        mgr.distributeShares(COMP, contributors, amounts);
    }

    /// The competitive `distribute` and collaborative `distributeShares` share the same
    /// terminal `settled` flag: once shares settle, the competitive path is closed too.
    function test_shares_and_distribute_share_the_terminal_flag() public {
        uint256 pool = 1_000_000;
        _create(pool);
        vm.warp(DEADLINE);

        address[] memory contributors = new address[](1);
        contributors[0] = poolA;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 400_000;
        vm.prank(proposer);
        mgr.distributeShares(COMP, contributors, amounts);

        // The competitive distribute now reverts — the competition is already settled.
        vm.prank(proposer);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.AlreadySettled.selector, COMP));
        mgr.distribute(COMP, contributors, amounts);
    }

    // =======================================================================
    // Marketplace — listArtifact / buyArtifact
    // =======================================================================

    function _list(uint256 price, bool exclusive) internal returns (uint256 id) {
        _ensureProvenance();
        vm.prank(seller);
        id = mgr.listArtifact(ARTIFACT, price, PROVENANCE, exclusive);
    }

    /// List + buy happy path: the buyer pays the price, the seller receives it, the
    /// listing carries its provenance, and the contract retains nothing (escrowless).
    function test_list_and_buy_pays_seller_and_conserves() public {
        uint256 price = 3 ether;
        uint256 id = _list(price, true);

        // Listing metadata is recorded with the provenance competition.
        (address s, bytes32 ref, uint256 p, uint64 prov, bool exclusive, bool sold) = mgr.listingOf(id);
        assertEq(s, seller);
        assertEq(ref, ARTIFACT);
        assertEq(p, price);
        assertEq(prov, PROVENANCE);
        assertTrue(exclusive);
        assertFalse(sold);

        uint256 sellerBefore = seller.balance;
        // The contract may hold the provenance competition's nominal escrow (opened by
        // `_ensureProvenance` so the listing names a real competition); the SALE must add
        // nothing to it — the marketplace is escrowless.
        uint256 mgrBefore = address(mgr).balance;
        vm.prank(buyer);
        mgr.buyArtifact{ value: price }(id);

        // Pay-to-seller conservation: the seller got exactly the price; the sale moved no
        // value into the contract (its balance is unchanged — escrowless marketplace).
        assertEq(seller.balance, sellerBefore + price);
        assertEq(address(mgr).balance, mgrBefore);
        // The exclusive listing is now marked sold.
        (,,,,, bool soldAfter) = mgr.listingOf(id);
        assertTrue(soldAfter);
    }

    /// An exclusive listing cannot be bought twice (no double-sell).
    function test_exclusive_listing_double_buy_reverts() public {
        uint256 price = 1 ether;
        uint256 id = _list(price, true);

        vm.prank(buyer);
        mgr.buyArtifact{ value: price }(id);

        // A second buyer cannot purchase the already-sold exclusive listing.
        address buyer2 = address(0xB2);
        vm.deal(buyer2, 100 ether);
        vm.prank(buyer2);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.ListingSold.selector, id));
        mgr.buyArtifact{ value: price }(id);
    }

    /// A non-exclusive listing may be sold to multiple buyers (the inventory is a
    /// license, not the unique artifact).
    function test_non_exclusive_listing_sells_repeatedly() public {
        uint256 price = 1 ether;
        uint256 id = _list(price, false);

        uint256 sellerStart = seller.balance;
        vm.prank(buyer);
        mgr.buyArtifact{ value: price }(id);

        address buyer2 = address(0xB2);
        vm.deal(buyer2, 100 ether);
        vm.prank(buyer2);
        mgr.buyArtifact{ value: price }(id);

        // Both sales paid the seller; the non-exclusive listing is never marked sold.
        assertEq(seller.balance, sellerStart + 2 * price);
        (,,,,, bool sold) = mgr.listingOf(id);
        assertFalse(sold);
    }

    /// Buying at the wrong price (over or under) reverts; the seller is unpaid.
    function test_buy_wrong_price_reverts() public {
        uint256 price = 2 ether;
        uint256 id = _list(price, true);

        // Underpay.
        vm.prank(buyer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.WrongPrice.selector, price - 1, price)
        );
        mgr.buyArtifact{ value: price - 1 }(id);

        // Overpay.
        vm.prank(buyer);
        vm.expectRevert(
            abi.encodeWithSelector(CompetitionManager.WrongPrice.selector, price + 1, price)
        );
        mgr.buyArtifact{ value: price + 1 }(id);

        // Listing remains unsold and the seller unpaid.
        (,,,,, bool sold) = mgr.listingOf(id);
        assertFalse(sold);
    }

    /// A zero-priced listing is rejected at list time (a zero price cannot settle a sale).
    function test_list_zero_price_reverts() public {
        vm.prank(seller);
        vm.expectRevert(CompetitionManager.ZeroPrice.selector);
        mgr.listArtifact(ARTIFACT, 0, PROVENANCE, true);
        // No listing was created.
        assertEq(mgr.nextListingId(), 0);
    }

    /// Listing with a provenance competition that does not exist reverts: the on-chain
    /// provenance field must be a real competition id, not an arbitrary uint64, so the
    /// `ArtifactSold` ledger attests a genuine origin.
    function test_list_unknown_provenance_reverts() public {
        uint64 ghost = 424_242; // never created
        vm.prank(seller);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.UnknownCompetition.selector, ghost));
        mgr.listArtifact(ARTIFACT, 1 ether, ghost, true);
        // No listing was created.
        assertEq(mgr.nextListingId(), 0);
    }

    /// Buying an unknown listing reverts.
    function test_buy_unknown_listing_reverts() public {
        vm.prank(buyer);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.UnknownListing.selector, 0));
        mgr.buyArtifact{ value: 1 ether }(0);
    }

    /// A seller cannot buy their own listing (a self-purchase is a no-op laundering).
    function test_seller_cannot_buy_own_listing() public {
        uint256 price = 1 ether;
        uint256 id = _list(price, true);
        vm.deal(seller, 100 ether);
        vm.prank(seller);
        vm.expectRevert(abi.encodeWithSelector(CompetitionManager.SelfPurchase.selector, seller));
        mgr.buyArtifact{ value: price }(id);
    }

    /// Listing ids increment, and provenance + price travel with each listing. Both
    /// winning and (disclosed) losing artifacts are listable inventory — the chain does
    /// not distinguish; it stores the minimal metadata and settles the sale.
    function test_listing_ids_increment_and_carry_provenance() public {
        uint256 id0 = _list(1 ether, true);
        uint256 id1 = _list(2 ether, false);
        assertEq(id0, 0);
        assertEq(id1, 1);
        assertEq(mgr.nextListingId(), 2);

        (,, uint256 p0,,,) = mgr.listingOf(id0);
        (,, uint256 p1,,,) = mgr.listingOf(id1);
        assertEq(p0, 1 ether);
        assertEq(p1, 2 ether);
    }

    // --- isolation: M6 does not disturb M1 escrow/settlement ---------------

    function test_marketplace_does_not_touch_competition_escrow() public {
        _create(1 ether);
        // A marketplace sale moves value seller<->buyer and never touches the
        // competition's escrow.
        uint256 id = _list(1 ether, true);
        vm.prank(buyer);
        mgr.buyArtifact{ value: 1 ether }(id);
        assertEq(mgr.escrowOf(COMP), 1 ether, "competition escrow untouched by a sale");
        assertFalse(mgr.isSettled(COMP));
    }
}
