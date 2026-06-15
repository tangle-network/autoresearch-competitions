// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import { Script, console2 } from "forge-std/Script.sol";
import { Types } from "tnt-core/src/libraries/Types.sol";
import { ITangleBlueprints } from "tnt-core/src/interfaces/ITangleBlueprints.sol";
import { CompetitionManager } from "../src/CompetitionManager.sol";

/// @title Deploy
/// @notice Deploys the `CompetitionManager` service manager and (optionally)
///         registers the autoresearch-competitions blueprint on Tangle in a
///         single broadcast.
///
///         `CompetitionManager` is a regular (non-upgradeable) `BlueprintServiceManagerBase`
///         subclass with a no-argument constructor; the Tangle protocol address is
///         bound later via the `onBlueprintCreated` callback, so no proxy or
///         constructor wiring is needed here.
///
///         This script is intentionally split into two entrypoints so an operator
///         can deploy the manager on any chain (including a bare anvil) without a
///         live Tangle core, then register against Tangle once core is reachable:
///
///           - `run()`      — deploy the manager only, log its address.
///           - `register()` — deploy the manager AND `createBlueprint` on Tangle.
///
/// @dev    THE REAL MAINNET/TESTNET DEPLOY IS AN OPERATOR-RUN STEP. This script is
///         exercised non-broadcasting by `contracts/test/Deploy.t.sol`; it is never
///         broadcast to a real network from CI. See `docs/DEPLOYMENT.md`.
///
///         Deploy the manager:
///           forge script contracts/script/Deploy.s.sol \
///             --rpc-url $RPC_URL --broadcast --slow
///
///         Deploy + register on Tangle:
///           forge script contracts/script/Deploy.s.sol --sig "register()" \
///             --rpc-url $RPC_URL --broadcast --slow
contract Deploy is Script {
    // Anvil well-known deployer key (default when no PRIVATE_KEY env is set).
    uint256 internal constant DEFAULT_DEPLOYER_KEY =
        0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80;

    // Tangle protocol address on a LocalTestnet anvil snapshot. For real chains
    // (Base Sepolia, mainnet) pass TANGLE_CORE via env.
    address internal constant DEFAULT_TANGLE = 0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9;

    string internal constant METADATA_URI = "https://github.com/tangle-network/autoresearch-competitions";

    /// Deploy only the `CompetitionManager`. Safe to run against any EVM chain.
    function run() external returns (CompetitionManager mgr) {
        uint256 deployerKey = vm.envOr("PRIVATE_KEY", DEFAULT_DEPLOYER_KEY);

        vm.startBroadcast(deployerKey);
        mgr = new CompetitionManager();
        vm.stopBroadcast();

        console2.log("DEPLOY_COMPETITION_MANAGER=%s", vm.toString(address(mgr)));
    }

    /// Deploy the manager and register the blueprint on Tangle core.
    function register() external returns (CompetitionManager mgr, uint64 blueprintId) {
        uint256 deployerKey = vm.envOr("PRIVATE_KEY", DEFAULT_DEPLOYER_KEY);
        address tangleAddr = vm.envOr("TANGLE_CORE", DEFAULT_TANGLE);

        vm.startBroadcast(deployerKey);
        mgr = new CompetitionManager();
        blueprintId = ITangleBlueprints(tangleAddr).createBlueprint(_buildDefinition(address(mgr)));
        vm.stopBroadcast();

        console2.log("DEPLOY_COMPETITION_MANAGER=%s", vm.toString(address(mgr)));
        console2.log("DEPLOY_COMPETITION_BLUEPRINT_ID=%s", vm.toString(blueprintId));
    }

    // ═════════════════════════════════════════════════════════════════════════
    // Blueprint definition builder — kept `public` so `Deploy.t.sol` can assert
    // its shape without broadcasting.
    // ═════════════════════════════════════════════════════════════════════════

    function _buildDefinition(address manager) public pure returns (Types.BlueprintDefinition memory def) {
        def.metadataUri = METADATA_URI;
        // Until canonical metadata JSON is pinned via IPFS, derive the digest from
        // the metadataUri so the value is deterministic + traceable.
        def.metadataHash = keccak256(bytes(def.metadataUri));
        def.manager = manager;
        def.masterManagerRevision = 0;
        def.hasConfig = true;

        // Dynamic membership: referees/operators come and go as competitions open.
        // EventDriven pricing: requesters pay per job (x402 per-job weights mirror
        // this off-chain). Minimum one operator so a single referee can adjudicate
        // a solo competition; unbounded above.
        def.config = Types.BlueprintConfig({
            membership: Types.MembershipModel.Dynamic,
            pricing: Types.PricingModel.EventDriven,
            minOperators: 1,
            maxOperators: 0,
            subscriptionRate: 0,
            subscriptionInterval: 0,
            eventRate: 0
        });

        def.metadata = Types.BlueprintMetadata({
            name: "Autoresearch Competitions",
            description: "Decentralized auto-research competitions: post a bounty for a better artifact, scored on a held-out test. Commit-reveal submissions, a promotion gate on the lower CI bound of the lift, and conserved on-chain settlement.",
            author: "Tangle",
            category: "AI/Research",
            codeRepository: METADATA_URI,
            logo: "",
            website: "https://tangle.network",
            license: "MIT OR Apache-2.0",
            profilingData: ""
        });

        def.jobs = _buildJobs();

        def.registrationSchema = "";
        def.requestSchema = "";

        def.sources = new Types.BlueprintSource[](1);
        Types.BlueprintBinary[] memory bins = new Types.BlueprintBinary[](1);
        bins[0] = Types.BlueprintBinary({
            arch: Types.BlueprintArchitecture.Amd64,
            os: Types.BlueprintOperatingSystem.Linux,
            name: "autoresearch-competitions",
            sha256: bytes32(0)
        });
        def.sources[0] = Types.BlueprintSource({
            kind: Types.BlueprintSourceKind.Native,
            container: Types.ImageRegistrySource("", "", ""),
            wasm: Types.WasmSource(Types.WasmRuntime.Unknown, Types.BlueprintFetcherKind.None, "", ""),
            native: Types.NativeSource(
                Types.BlueprintFetcherKind.None,
                "file:///target/release/autoresearch-competitions",
                "./target/release/autoresearch-competitions"
            ),
            testing: Types.TestingSource("autoresearch-competitions-bin", "autoresearch-competitions", "."),
            binaries: bins
        });

        def.supportedMemberships = new Types.MembershipModel[](1);
        def.supportedMemberships[0] = Types.MembershipModel.Dynamic;
    }

    /// The eight competition jobs. Param/result schemas are kept empty: the Rust
    /// operator owns the typed ABI shapes (`autoresearch_competitions_lib::sol!`),
    /// matching the proven sandbox + training blueprint pattern. Job ids MUST match
    /// the `JOB_*` constants in `CompetitionManager.sol` and the Rust lib.
    function _buildJobs() internal pure returns (Types.JobDefinition[] memory jobs) {
        jobs = new Types.JobDefinition[](8);

        jobs[0] = Types.JobDefinition({
            name: "create_competition",
            description: "Open a competition and escrow its native reward pool (structure/cadence/visibility/scorer knobs)",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[1] = Types.JobDefinition({
            name: "join",
            description: "Researcher registers and posts the slashable stake bond",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[2] = Types.JobDefinition({
            name: "commit_candidate",
            description: "Commit phase of commit-reveal: submit keccak256(abi.encode(artifactRef, salt))",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[3] = Types.JobDefinition({
            name: "reveal_candidate",
            description: "Reveal phase: disclose artifact reference + salt; the contract verifies the commitment",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[4] = Types.JobDefinition({
            name: "report_score",
            description: "Referee commits a certified result; the promotion gate is evaluated on the lift's lower CI bound",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[5] = Types.JobDefinition({
            name: "settle",
            description: "Rank candidates and pay out per the RewardSchedule; settlement conserves the escrowed pool",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[6] = Types.JobDefinition({
            name: "challenge",
            description: "Staked dispute of a reported score, triggering re-score",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
        jobs[7] = Types.JobDefinition({
            name: "tick",
            description: "Cron-driven: deadline enforcement and continuous-epoch settlement",
            metadataUri: "",
            paramsSchema: "",
            resultSchema: ""
        });
    }
}
