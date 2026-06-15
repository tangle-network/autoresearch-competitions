// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Test } from "forge-std/Test.sol";
import { Types } from "tnt-core/src/libraries/Types.sol";
import { Deploy } from "../script/Deploy.s.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// M7 proof for the deploy path: the `Deploy` script produces a fresh
/// `CompetitionManager` with the expected initial state, and the blueprint
/// definition it registers is internally consistent (8 jobs, matching ids,
/// dynamic membership, event-driven pricing). No broadcast to a real network.
contract DeployTest is Test {
    Deploy internal deployer;

    function setUp() public {
        deployer = new Deploy();
    }

    /// The script's `run()` deploys a manager in a clean initial state: no
    /// competitions exist, no listings have been minted, and the blueprintId is
    /// unset until the on-chain `onBlueprintCreated` callback binds it.
    function test_run_deploys_manager_with_clean_initial_state() public {
        CompetitionManager mgr = deployer.run();

        assertTrue(address(mgr) != address(0), "manager should be deployed");

        // No competition has been created yet.
        (address proposer,,,,,, bool exists,) = mgr.competitions(1);
        assertEq(proposer, address(0), "no proposer before creation");
        assertFalse(exists, "no competition before creation");

        // Marketplace listing counter starts at zero.
        assertEq(mgr.nextListingId(), 0, "listing id starts at 0");

        // blueprintId is bound by the Tangle protocol via onBlueprintCreated.
        assertEq(mgr.blueprintId(), 0, "blueprintId unset before registration");
    }

    /// A freshly deployed manager preserves the M1 escrow path end-to-end: a
    /// competition can be created and its pool is escrowed. This guards against a
    /// deploy script that silently produces a broken manager.
    function test_deployed_manager_supports_create_and_escrow() public {
        CompetitionManager mgr = deployer.run();

        vm.deal(address(this), 10 ether);
        mgr.createCompetition{ value: 1 ether }(7, uint64(block.timestamp + 1000));

        (address proposer, uint256 pool, uint256 escrowed,,,, bool exists,) = mgr.competitions(7);
        assertEq(proposer, address(this), "proposer recorded");
        assertEq(pool, 1 ether, "pool recorded");
        assertEq(escrowed, 1 ether, "pool escrowed");
        assertTrue(exists, "competition exists");
        assertEq(address(mgr).balance, 1 ether, "native value held by manager");
    }

    /// The registered blueprint definition is well-formed: exactly the 8 jobs the
    /// Rust router serves, named and ordered to match the on-chain JOB_* ids, with
    /// dynamic membership + event-driven pricing.
    function test_blueprint_definition_is_consistent() public {
        CompetitionManager mgr = deployer.run();
        Types.BlueprintDefinition memory def = deployer._buildDefinition(address(mgr));

        assertEq(def.manager, address(mgr), "manager wired into definition");
        assertEq(def.metadataHash, keccak256(bytes(def.metadataUri)), "metadata hash derived from uri");
        assertTrue(def.hasConfig, "config applied");

        assertEq(uint8(def.config.membership), uint8(Types.MembershipModel.Dynamic), "dynamic membership");
        assertEq(uint8(def.config.pricing), uint8(Types.PricingModel.EventDriven), "event-driven pricing");
        assertEq(def.config.minOperators, 1, "at least one referee");

        // Exactly 8 jobs, ordered to match the JOB_* constants.
        assertEq(def.jobs.length, 8, "eight jobs");
        assertEq(def.jobs[mgr.JOB_CREATE_COMPETITION()].name, "create_competition");
        assertEq(def.jobs[mgr.JOB_JOIN()].name, "join");
        assertEq(def.jobs[mgr.JOB_COMMIT_CANDIDATE()].name, "commit_candidate");
        assertEq(def.jobs[mgr.JOB_REVEAL_CANDIDATE()].name, "reveal_candidate");
        assertEq(def.jobs[mgr.JOB_REPORT_SCORE()].name, "report_score");
        assertEq(def.jobs[mgr.JOB_SETTLE()].name, "settle");
        assertEq(def.jobs[mgr.JOB_CHALLENGE()].name, "challenge");
        assertEq(def.jobs[mgr.JOB_TICK()].name, "tick");

        // One native source advertising the operator binary.
        assertEq(def.sources.length, 1, "one source");
        assertEq(uint8(def.sources[0].kind), uint8(Types.BlueprintSourceKind.Native), "native source");
    }
}
