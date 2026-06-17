// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.8.26;

import "tnt-core/src/BlueprintServiceManagerBase.sol";

/// @title CompetitionManager
/// @notice On-chain settlement and commitment spine for the autoresearch-competitions
///         blueprint — a decentralized market for verifiable improvement.
///
/// The chain stores ONLY commitments, certified scores, attestation hashes, and
/// payouts. Artifacts, data, traces, and held-out evals never touch the chain;
/// they live in sandboxes and with the Referee (see docs/ARCHITECTURE.md §3).
///
/// M1 status: escrow is real (native value is locked per competition at creation),
/// commit-reveal is verified cryptographically (keccak256(abi.encode(artifactRef, salt))
/// must equal the stored commitment), and settlement conserves the pool (the sum of
/// distributed amounts can never exceed the escrowed balance, the competition pays
/// out at most once, and only after its deadline). The Referee computes the ranking
/// and amounts off-chain; the chain enforces conservation and pays. ERC-20 reward
/// assets are a documented seam (see `createCompetition`). Naming is (proposed)
/// until the contract suite freezes.
contract CompetitionManager is BlueprintServiceManagerBase {
    // --- Job IDs (MUST match autoresearch-competitions-lib::JOB_*) ---------
    uint8 public constant JOB_CREATE_COMPETITION = 0;
    uint8 public constant JOB_JOIN = 1;
    uint8 public constant JOB_COMMIT_CANDIDATE = 2;
    uint8 public constant JOB_REVEAL_CANDIDATE = 3;
    uint8 public constant JOB_REPORT_SCORE = 4;
    uint8 public constant JOB_SETTLE = 5;
    uint8 public constant JOB_CHALLENGE = 6;
    uint8 public constant JOB_TICK = 7;

    // --- Errors ------------------------------------------------------------
    error CompetitionExists(uint64 competitionId);
    error UnknownCompetition(uint64 competitionId);
    error EmptyPool();
    error NotProposer(address caller, address proposer);
    error BeforeDeadline(uint64 nowTs, uint64 deadline);
    error AlreadySettled(uint64 competitionId);
    error LengthMismatch(uint256 winners, uint256 amounts);
    error Overdistribution(uint256 requested, uint256 escrowed);
    error NoCommitment(uint64 competitionId, address researcher);
    error RevealMismatch(uint64 competitionId, address researcher);
    error TransferFailed(address to, uint256 amount);
    // --- M2: stake / challenge / dispute ---
    error BelowMinStake(uint256 posted, uint256 minStake);
    error NoStake(uint64 competitionId, address researcher);
    error StakeSlashed(uint64 competitionId, address researcher);
    error NotStakedToSubmit(uint64 competitionId, address researcher);
    error ChallengeExists(uint64 competitionId, bytes32 candidateId);
    error UnknownChallenge(uint64 competitionId, bytes32 candidateId);
    error ChallengeResolved(uint64 competitionId, bytes32 candidateId);
    error NotDisputeAuthority(address caller, address proposer);
    error BadOutcome(uint8 outcome);
    error SlashExceedsStake(uint256 requested, uint256 staked);
    error RewardExceedsSlash(uint256 reward, uint256 slashed);
    error ResearcherNotRevealed(uint64 competitionId, address researcher);
    error StakeLockedByChallenge(uint64 competitionId, address researcher, uint256 openChallenges);
    // --- M3: continuous (king-of-the-hill) arena ---
    error NotContinuous(uint64 competitionId);
    error AlreadyContinuous(uint64 competitionId);
    error NotRecordAuthority(address caller, address proposer);
    error SubEpsilonBeat(int256 newBestMicros, int256 bestMicros, int256 epsilonMicros);
    error MarginalMismatch(uint256 provided, uint256 expected);
    error WrongContinuousMode(uint64 competitionId);
    error NoTopHolder(uint64 competitionId);
    /// A continuous payout (recordBeat/tickEpoch) was attempted after the competition
    /// was settled. `distribute` is terminal: once it runs, the continuous window is
    /// closed and any residual escrow is no longer payable through the continuous path.
    error ContinuousClosed(uint64 competitionId);
    // --- M4: privacy tiers + structural attestation commitment ---
    /// Only the attestation authority (the proposer, who routes the off-chain
    /// Referee's reports — same authority seam as `recordBeat`/`resolveDispute`) may
    /// commit an attestation hash.
    error NotAttestationAuthority(address caller, address proposer);
    /// An empty (zero) attestation hash was submitted. A commitment must reference a
    /// real report; the zero hash is reserved for "no attestation committed".
    error EmptyAttestationHash(uint64 competitionId, bytes32 candidateId);
    // --- M5: scorer-kind record (which referee adjudicated) ---
    /// Only the proposer may set a competition's scorer kind — it is part of the
    /// declared competition spec, the same authority seam as `setPrivacy`.
    error NotScorerAuthority(address caller, address proposer);
    /// An out-of-range scorer kind was supplied. Valid values are 0..=3, mirroring
    /// `autoresearch_runtime::types::ScorerKind`.
    error BadScorerKind(uint8 scorerKind);
    // --- M6: collaborative contribution settlement -------------------------
    /// Only the proposer (the same settlement authority as `distribute`) may settle
    /// collaborative contribution shares.
    error NotShareAuthority(address caller, address proposer);
    // --- M6: certified-artifact marketplace --------------------------------
    /// A listing must carry a positive price; a zero-priced listing cannot settle a sale.
    error ZeroPrice();
    /// The listing id does not exist.
    error UnknownListing(uint256 listingId);
    /// An exclusive listing was already sold; it cannot be sold a second time.
    error ListingSold(uint256 listingId);
    /// `msg.value` did not equal the listing's asking price.
    error WrongPrice(uint256 sent, uint256 price);
    /// A seller tried to buy their own listing.
    error SelfPurchase(address buyer);

    // --- Storage -----------------------------------------------------------

    /// On-chain competition record. The rich spec stays off-chain behind `specRef`;
    /// only what escrow + settlement need is mirrored here. `escrowedWei` starts at
    /// `rewardPoolWei` and is drawn down by `distribute`.
    struct Competition {
        address proposer;
        uint256 rewardPoolWei; // original pool size (immutable record)
        uint256 escrowedWei; // remaining locked balance
        address rewardAsset; // zero = native (the only path funded in M1)
        uint64 deadline; // unix ts; settlement is gated until on/after this
        uint256 minStakeWei; // M2: researcher stake floor (MECHANISM.md §3); 0 = open
        bool exists;
        bool settled;
    }

    /// M2: a staked dispute of a reported score (MECHANISM.md §7). The challenger
    /// locks `stake` when opening; resolution moves stake per the committee outcome.
    ///
    /// `researcher` is the disputed candidate's researcher, BOUND at open time (they
    /// must have a recorded reveal for this competition). Resolution can only slash
    /// THIS researcher — the slashed party is not a free proposer parameter — and
    /// their bond is locked against withdrawal while the challenge is unresolved.
    struct Challenge {
        address challenger;
        address researcher;
        uint256 stake;
        bool exists;
        bool resolved;
    }

    /// M3: per-competition continuous (king-of-the-hill) state (MECHANISM.md §5,
    /// docs/MECHANISM.md "Continuous"). A continuous competition shares the same
    /// escrow `Competition` record (its pool is locked at creation) and carries this
    /// parallel state machine: the marginal-improvement (`RecordBounty`) or
    /// time-at-top (`TimeAtTopStreaming`) leaderboard. The `RecordBeat` /
    /// `EpochCredited` events are the verifiable leaderboard: an indexer recomputes
    /// every rank and payout by replaying them.
    ///
    /// `weiPerMicro` is set for `RecordBounty`; `weiPerEpoch` for `TimeAtTopStreaming`.
    /// Exactly one is non-zero, selected by `streaming`. `bestMicros` starts at
    /// `baselineMicros` and only ever moves up. `spentWei` mirrors the escrow drawdown
    /// for the continuous payouts and can never exceed the locked pool (conservation).
    struct ContinuousState {
        bool exists;
        bool streaming; // false = RecordBounty, true = TimeAtTopStreaming
        int256 epsilonMicros; // min marginal (in micro-points) for a record to count
        uint256 weiPerMicro; // RecordBounty rate
        uint256 weiPerEpoch; // TimeAtTopStreaming rate
        int256 baselineMicros; // the bar the first record is measured from
        int256 bestMicros; // current best; monotone non-decreasing
        address topHolder; // current #1 (paid per epoch under streaming)
        uint256 spentWei; // continuous payouts so far; <= pool (conservation)
        uint64 epoch; // epoch cursor (advanced by tickEpoch)
    }

    /// competitionId => competition record.
    mapping(uint64 => Competition) public competitions;

    /// M3: competitionId => continuous arena state.
    mapping(uint64 => ContinuousState) public continuousStates;

    /// competitionId => researcher => commitment hash (commit-reveal anti-copy).
    mapping(uint64 => mapping(address => bytes32)) public commitments;

    /// competitionId => researcher => whether their commitment has been revealed.
    mapping(uint64 => mapping(address => bool)) public revealed;

    /// M2: competitionId => researcher => posted, slashable stake (MECHANISM.md §3).
    mapping(uint64 => mapping(address => uint256)) public stakes;

    /// M2: competitionId => candidateId => open/resolved challenge (MECHANISM.md §7).
    mapping(uint64 => mapping(bytes32 => Challenge)) public challenges;

    /// M2: competitionId => researcher => number of unresolved challenges that name
    /// this researcher. A positive count LOCKS the researcher's stake against
    /// `withdrawStake` so a caught researcher cannot exit their bond before the
    /// slashable dispute resolves (MECHANISM.md §7). Incremented in `openChallenge`,
    /// decremented in `resolveDispute`.
    mapping(uint64 => mapping(address => uint256)) public openChallengeCount;

    // --- M4: privacy tiers + structural attestation commitment -------------

    /// competitionId => privacy tier (uint8). Mirrors
    /// `autoresearch_runtime::privacy::PrivacyTier`:
    ///   0 = BlackBox, 1 = RedactedFeedback, 2 = WhiteBoxNoEgress, 3 = AttestedHarness.
    /// 0 is also the default for any competition created without a tier (the
    /// privacy-easy default — researchers see only scores; docs/PRIVACY.md §1).
    mapping(uint64 => uint8) public privacyTier;

    /// competitionId => required TEE type (uint8). Mirrors
    /// `autoresearch_runtime::attestation::TeeType`:
    ///   0 = None, 1 = PhalaTdx, 2 = AwsNitro, 3 = GcpConfidential, 4 = AzureSnp.
    /// This records what the off-chain Referee is REQUIRED to attest to; the chain
    /// does NOT verify the quote (see `commitAttestation`).
    mapping(uint64 => uint8) public requiredTee;

    /// competitionId => candidateId => committed attestation hash (keccak of the
    /// canonical attestation report; see `autoresearch_runtime::attestation`).
    ///
    /// This is a STRUCTURAL COMMITMENT, not a verification. Storing the hash lets a
    /// disputer prove WHICH report a score was produced against; it does NOT prove the
    /// report came from genuine TEE silicon running the expected image. On-chain quote
    /// verification (DCAP/KDS/NSM signature recovery + measurement pinning + nonce
    /// binding) is the documented seam — see `commitAttestation`.
    mapping(uint64 => mapping(bytes32 => bytes32)) public attestationHashes;

    // --- M5: scorer kind (which referee adjudicated this competition) ------

    /// competitionId => scorer kind (uint8). Mirrors
    /// `autoresearch_runtime::types::ScorerKind`:
    ///   0 = HeldOutEval, 1 = PrivateOracle, 2 = PrivilegedHardware, 3 = HumanPanel.
    /// 0 is also the default for any competition created without a kind (the
    /// agent-profile held-out-eval default). Recording WHICH scorer adjudicated
    /// puts that fact on the verifiable leaderboard: an indexer / disputer can see the
    /// referee class a payout was certified under. Scoring itself is OFF-CHAIN by design
    /// (the chain never runs an oracle / privileged device / human panel); this records
    /// only the declared kind.
    mapping(uint64 => uint8) public scorerKind;

    // --- M6: certified-artifact marketplace --------------------------------

    /// A marketplace listing of a certified artifact (docs/MECHANISM.md §10). The chain
    /// keeps listing metadata MINIMAL — the artifact hash, price, provenance, and
    /// exclusivity; the rich metadata (certified lift CI, license terms, attestation
    /// hashes) lives off-chain and travels in the `ArtifactListing` domain type. Both
    /// WINNING and (gate-clearing) LOSING artifacts can be listed — the inventory
    /// competitions manufacture. `exclusive` listings sell at most once.
    struct Listing {
        address seller;
        bytes32 artifactRef; // keccak/content hash of the artifact (off-chain bytes)
        uint256 price; // asking price in wei; must be positive
        uint64 provenanceCompetitionId; // the competition that certified the artifact
        bool exclusive; // an exclusive license sells at most once
        bool sold; // set on the (first) sale of an exclusive listing
        bool exists;
    }

    /// listingId => listing. Ids are assigned sequentially from `nextListingId`.
    mapping(uint256 => Listing) public listings;

    /// The next listing id to assign (also the count of listings ever created).
    uint256 public nextListingId;

    // --- Events (verifiable-leaderboard surface) ---------------------------
    // Off-chain indexers recompute the leaderboard purely from these; any party
    // can challenge a divergence on-chain (docs/ARCHITECTURE.md §10).
    event CompetitionCreated(
        uint64 indexed competitionId, address indexed proposer, uint256 rewardPoolWei, uint64 deadline
    );
    event CandidateCommitted(uint64 indexed competitionId, address indexed researcher, bytes32 commitment);
    event CandidateRevealed(uint64 indexed competitionId, address indexed researcher, string artifactRef);
    event ScoreReported(uint64 indexed competitionId, address indexed referee, uint64 jobCallId);
    event PayoutMade(uint64 indexed competitionId, address indexed researcher, uint256 amountWei);
    event CompetitionSettled(uint64 indexed competitionId, uint256 totalPaidWei);
    // M2 dispute surface.
    event StakePosted(uint64 indexed competitionId, address indexed researcher, uint256 amountWei);
    event StakeWithdrawn(uint64 indexed competitionId, address indexed researcher, uint256 amountWei);
    event ChallengeOpened(
        uint64 indexed competitionId, bytes32 indexed candidateId, address indexed challenger, uint256 stakeWei
    );
    event DisputeResolved(uint64 indexed competitionId, bytes32 indexed candidateId, uint8 outcome);
    // M3 continuous-arena surface. These events ARE the verifiable leaderboard:
    // replaying them reproduces every rank and payout (docs/ARCHITECTURE.md §10).
    event ContinuousCreated(
        uint64 indexed competitionId, bool streaming, int256 epsilonMicros, uint256 rate, int256 baselineMicros
    );
    event RecordBeat(
        uint64 indexed competitionId,
        address indexed researcher,
        bytes32 indexed candidateId,
        int256 newBestMicros,
        uint256 marginalWei,
        uint64 epoch
    );
    event EpochCredited(
        uint64 indexed competitionId, address indexed researcher, uint256 amountWei, uint64 epoch, int256 bestMicros
    );
    // M4 privacy surface. `PrivacySet` records the tier + required TEE a competition
    // declared; `AttestationCommitted` records a per-candidate structural commitment
    // (NOT a verified quote — see `commitAttestation`).
    event PrivacySet(uint64 indexed competitionId, uint8 privacyTier, uint8 requiredTee);
    event AttestationCommitted(uint64 indexed competitionId, bytes32 indexed candidateId, bytes32 attestationHash);
    // M5 scorer-kind surface. `ScorerKindSet` records which scorer class adjudicated a
    // competition (held-out eval / private oracle / privileged hardware / human panel),
    // so the verifiable leaderboard carries the referee class behind each payout.
    event ScorerKindSet(uint64 indexed competitionId, uint8 scorerKind);
    // M6 collaborative-settlement surface. `ContributionsSettled` is the verifiable
    // record of a Collaborative competition paying its contributors by share (the
    // off-chain held-out-gated single-permutation marginal attribution; the chain
    // conserves + pays). `PayoutMade` is reused per contributor, exactly as `distribute`
    // does for the competitive path.
    event ContributionsSettled(uint64 indexed competitionId, uint256 totalPaidWei, uint256 contributors);
    // M6 marketplace surface. `ArtifactListed` / `ArtifactSold` are the verifiable
    // marketplace ledger: an indexer replays them to reconstruct inventory + sales.
    event ArtifactListed(
        uint256 indexed listingId,
        address indexed seller,
        bytes32 artifactRef,
        uint256 price,
        uint64 provenanceCompetitionId
    );
    event ArtifactSold(uint256 indexed listingId, address indexed buyer, address indexed seller, uint256 price);

    // --- Escrow + creation -------------------------------------------------

    /// Open a competition and escrow its native reward pool in one call. `msg.value`
    /// IS the pool — the contract holds it until settlement. The off-chain
    /// CompetitionSpec is referenced by hash/CID elsewhere; on-chain we need only the
    /// economic record.
    ///
    /// ERC-20 reward assets are an intentional seam: a sibling `createCompetitionERC20`
    /// would `transferFrom` the proposer and set `rewardAsset`; `distribute` already
    /// branches on `rewardAsset` (native-only in M1) so the conservation logic is shared.
    function createCompetition(uint64 competitionId, uint64 deadline) external payable {
        _createCompetition(competitionId, deadline, 0);
    }

    /// M2 variant: open a competition with a researcher stake floor (`minStakeWei`,
    /// MECHANISM.md §3). Researchers must `postStake` at least this much before they
    /// may submit. `minStakeWei == 0` is the open M1 behaviour.
    function createCompetitionWithStake(uint64 competitionId, uint64 deadline, uint256 minStakeWei)
        external
        payable
    {
        _createCompetition(competitionId, deadline, minStakeWei);
    }

    function _createCompetition(uint64 competitionId, uint64 deadline, uint256 minStakeWei) internal {
        if (competitions[competitionId].exists) revert CompetitionExists(competitionId);
        if (msg.value == 0) revert EmptyPool();

        competitions[competitionId] = Competition({
            proposer: msg.sender,
            rewardPoolWei: msg.value,
            escrowedWei: msg.value,
            rewardAsset: address(0),
            deadline: deadline,
            minStakeWei: minStakeWei,
            exists: true,
            settled: false
        });

        emit CompetitionCreated(competitionId, msg.sender, msg.value, deadline);
    }

    // --- M2: researcher staking (MECHANISM.md §3) --------------------------

    /// Post (top up) researcher stake for a competition. `msg.value` is locked as the
    /// slashable bond. The cumulative stake must clear the competition's `minStakeWei`
    /// floor; below-floor posts revert so an unstaked researcher cannot become eligible.
    function postStake(uint64 competitionId) external payable {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        uint256 newStake = stakes[competitionId][msg.sender] + msg.value;
        if (newStake < c.minStakeWei) revert BelowMinStake(newStake, c.minStakeWei);
        stakes[competitionId][msg.sender] = newStake;
        emit StakePosted(competitionId, msg.sender, msg.value);
    }

    /// Withdraw posted stake after the competition has settled, provided it was not
    /// slashed AND no challenge naming this researcher is still open. Honest losing is
    /// never slashable, so an un-slashed researcher recovers their full bond once
    /// settlement is final — but a researcher with an unresolved challenge against them
    /// is in the slashable window and cannot exit their bond until it resolves
    /// (MECHANISM.md §7). This closes the escape where a guilty researcher withdraws
    /// after settlement but before `resolveDispute`, leaving the slash unenforceable.
    function withdrawStake(uint64 competitionId) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (!c.settled) revert BeforeDeadline(uint64(block.timestamp), c.deadline);
        uint256 open = openChallengeCount[competitionId][msg.sender];
        if (open != 0) revert StakeLockedByChallenge(competitionId, msg.sender, open);
        uint256 amount = stakes[competitionId][msg.sender];
        if (amount == 0) revert NoStake(competitionId, msg.sender);
        // Effects before interaction: zero the bond first (re-entrancy safe).
        stakes[competitionId][msg.sender] = 0;
        (bool ok,) = msg.sender.call{ value: amount }("");
        if (!ok) revert TransferFailed(msg.sender, amount);
        emit StakeWithdrawn(competitionId, msg.sender, amount);
    }

    /// Whether `researcher` has posted enough stake to be eligible to submit. With a
    /// positive `minStakeWei` floor, the researcher must have posted at least the
    /// floor; an open (`minStakeWei == 0`) competition admits anyone.
    function isStaked(uint64 competitionId, address researcher) public view returns (bool) {
        Competition storage c = competitions[competitionId];
        if (!c.exists) return false;
        if (c.minStakeWei == 0) return true;
        return stakes[competitionId][researcher] >= c.minStakeWei;
    }

    // --- M2: challenge + dispute resolution (MECHANISM.md §7) --------------

    /// Open a staked challenge against a reported score for `candidateId`, naming the
    /// `researcher` whose candidate is disputed. The challenger locks `msg.value` as
    /// challenger stake; the committee re-scores off-chain (`collect_verdicts` +
    /// `committee_verdict`) and the proposer/referee submits the authenticated outcome
    /// via `resolveDispute`.
    ///
    /// The disputed `researcher` is BOUND here, not at resolution time: they must have
    /// a recorded reveal for this competition (`revealed[competitionId][researcher]`),
    /// which ties the opaque `candidateId` to an actual on-chain participant. Opening a
    /// challenge LOCKS that researcher's stake against withdrawal until the dispute
    /// resolves, and `resolveDispute` can slash only this bound researcher — closing
    /// the gap where a proposer could slash an arbitrary innocent staker.
    ///
    /// One open challenge per candidate; a second openChallenge before resolution
    /// reverts so the dispute accounting stays unambiguous.
    function openChallenge(uint64 competitionId, bytes32 candidateId, address researcher) external payable {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.value == 0) revert EmptyPool();
        // Bind the dispute to a real participant: only a revealed candidate is
        // challengeable, so the slashed party is provably the disputed researcher.
        if (!revealed[competitionId][researcher]) revert ResearcherNotRevealed(competitionId, researcher);
        Challenge storage ch = challenges[competitionId][candidateId];
        if (ch.exists && !ch.resolved) revert ChallengeExists(competitionId, candidateId);
        challenges[competitionId][candidateId] = Challenge({
            challenger: msg.sender,
            researcher: researcher,
            stake: msg.value,
            exists: true,
            resolved: false
        });
        // Lock the researcher's bond: it cannot be withdrawn while this dispute is open.
        openChallengeCount[competitionId][researcher] += 1;
        emit ChallengeOpened(competitionId, candidateId, msg.sender, msg.value);
    }

    /// Resolve a dispute with the committee's authenticated outcome.
    ///
    /// `outcome`: 0 = Upheld (certification stands, challenger was wrong), 1 =
    /// Overturned (certification was wrong, challenger was right), 2 = Inconclusive
    /// (no quorum — both stakes refunded). The disputed researcher is NOT a free
    /// parameter: it is the `researcher` bound into the challenge at `openChallenge`
    /// time, so only the provably-disputed researcher can be slashed. `researcherSlash`
    /// is the researcher stake to slash on Overturned; `challengerReward` is paid to
    /// the challenger out of that slash (the rest is retained by the contract as the
    /// validator/burn share). The arithmetic mirrors
    /// `autoresearch_protocol::slash::resolve_dispute` and conserves: the contract can
    /// never pay out more than the two stakes locked for this dispute.
    ///
    /// SEAM — k-of-n EIP-712 signature verification. In M2 this is authority-gated to
    /// the proposer (the dispute authority), who is TRUSTED to report the honest
    /// committee outcome (Upheld/Overturned/Inconclusive) and a `researcherSlash` /
    /// `challengerReward` consistent with the off-chain verdict. The contract binds the
    /// SUBJECT of the slash on-chain (only the challenge's bound, revealed researcher
    /// can be slashed, up to their bond) but does NOT yet verify EIP-712 signatures
    /// from the m-of-n Validator committee on-chain; that is the documented seam,
    /// mirroring the trading blueprint's `TradeValidator` m-of-n EIP-712 pattern
    /// (default 2-of-3, score threshold >= 50; see docs/ARCHITECTURE.md §3.1
    /// `DisputeManager`). The committee outcome is computed and signed off-chain by
    /// the real mechanism (`committee_verdict`); wiring `submitReScore(verdict,
    /// signatures[])` with on-chain signature recovery + threshold replaces proposer
    /// trust for the outcome itself, and is the next step.
    function resolveDispute(
        uint64 competitionId,
        bytes32 candidateId,
        uint8 outcome,
        uint256 researcherSlash,
        uint256 challengerReward
    ) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotDisputeAuthority(msg.sender, c.proposer);
        if (outcome > 2) revert BadOutcome(outcome);

        Challenge storage ch = challenges[competitionId][candidateId];
        if (!ch.exists) revert UnknownChallenge(competitionId, candidateId);
        if (ch.resolved) revert ChallengeResolved(competitionId, candidateId);

        // Effects before interactions: mark resolved up front so a re-entrant payee
        // cannot trigger a second resolution.
        ch.resolved = true;
        address challenger = ch.challenger;
        address researcher = ch.researcher;
        uint256 challengerStake = ch.stake;

        // Release the withdrawal lock on this researcher's bond: this dispute is
        // resolved, so it no longer holds the stake. Slashing (below) happens first
        // on the still-locked stake, then the (reduced) remainder becomes withdrawable.
        openChallengeCount[competitionId][researcher] -= 1;

        if (outcome == 0) {
            // Upheld: the challenger was wrong → their stake is slashed (retained by
            // the contract as the validator/burn share). The researcher keeps their
            // stake (no slash). Nothing is paid out; conservation is trivial.
            emit DisputeResolved(competitionId, candidateId, outcome);
        } else if (outcome == 1) {
            // Overturned: the researcher was wrong → slash up to their staked bond and
            // pay the challenger a reward (<= the slash) plus refund the challenger's
            // own stake. The slash remainder (slash - reward) is retained as the
            // validator/burn share. Conserve: never pay more than the two stakes.
            uint256 staked = stakes[competitionId][researcher];
            if (researcherSlash > staked) revert SlashExceedsStake(researcherSlash, staked);
            if (challengerReward > researcherSlash) revert RewardExceedsSlash(challengerReward, researcherSlash);

            // Slash the researcher's bond.
            stakes[competitionId][researcher] = staked - researcherSlash;

            // Pay challenger: own stake refunded + reward from the researcher's slash.
            uint256 payout = challengerStake + challengerReward;
            if (payout > 0) {
                (bool ok,) = challenger.call{ value: payout }("");
                if (!ok) revert TransferFailed(challenger, payout);
            }
            emit DisputeResolved(competitionId, candidateId, outcome);
        } else {
            // Inconclusive: no fault proven → refund the challenger's stake in full;
            // the researcher's stake is untouched. No slash, no reward.
            if (challengerStake > 0) {
                (bool ok,) = challenger.call{ value: challengerStake }("");
                if (!ok) revert TransferFailed(challenger, challengerStake);
            }
            emit DisputeResolved(competitionId, candidateId, outcome);
        }
    }

    // --- M3: continuous (king-of-the-hill) arena ---------------------------

    /// Open a continuous `RecordBounty` competition: a king-of-the-hill leaderboard
    /// that keeps moving. `msg.value` IS the locked pool. Each new state-of-the-art
    /// that beats the current best by at least `epsilonMicros` earns its MARGINAL
    /// improvement at `weiPerMicro` (paid in `recordBeat`). `baselineMicros` is the
    /// bar the first record is measured from.
    function createContinuousCompetition(
        uint64 competitionId,
        uint64 deadline,
        int256 epsilonMicros,
        uint256 weiPerMicro,
        int256 baselineMicros
    ) external payable {
        _createCompetition(competitionId, deadline, 0);
        _initContinuous(competitionId, false, epsilonMicros, weiPerMicro, 0, baselineMicros);
    }

    /// Open a continuous `TimeAtTopStreaming` competition: the current top holder
    /// earns `weiPerEpoch` for each epoch held (credited in `tickEpoch`). Records
    /// (via `recordBeat`) seize the top spot but pay nothing on the beat itself.
    function createContinuousStreaming(
        uint64 competitionId,
        uint64 deadline,
        int256 epsilonMicros,
        uint256 weiPerEpoch,
        int256 baselineMicros
    ) external payable {
        _createCompetition(competitionId, deadline, 0);
        _initContinuous(competitionId, true, epsilonMicros, 0, weiPerEpoch, baselineMicros);
    }

    function _initContinuous(
        uint64 competitionId,
        bool streaming,
        int256 epsilonMicros,
        uint256 weiPerMicro,
        uint256 weiPerEpoch,
        int256 baselineMicros
    ) internal {
        if (continuousStates[competitionId].exists) revert AlreadyContinuous(competitionId);
        continuousStates[competitionId] = ContinuousState({
            exists: true,
            streaming: streaming,
            epsilonMicros: epsilonMicros,
            weiPerMicro: weiPerMicro,
            weiPerEpoch: weiPerEpoch,
            baselineMicros: baselineMicros,
            bestMicros: baselineMicros,
            topHolder: address(0),
            spentWei: 0,
            epoch: 0
        });
        emit ContinuousCreated(
            competitionId, streaming, epsilonMicros, streaming ? weiPerEpoch : weiPerMicro, baselineMicros
        );
    }

    /// Record a new state-of-the-art for a continuous competition.
    ///
    /// SEAM — referee-auth / k-of-n. `recordBeat` is authority-gated to the proposer
    /// (the record authority / referee). The proposer is TRUSTED to submit only
    /// records the off-chain Referee certified on the held-out split (the candidate
    /// cleared the promotion gate and the measured `newBestMicros` is honest). The
    /// contract enforces the ECONOMIC invariants on-chain — strict epsilon-margin,
    /// exact marginal arithmetic, and pool conservation — but does NOT yet verify
    /// m-of-n EIP-712 signatures from the Validator committee on-chain. That on-chain
    /// signature recovery + threshold is the documented seam, identical to
    /// `resolveDispute` and the trading blueprint's `TradeValidator` m-of-n pattern
    /// (default 2-of-3); wiring `recordBeat(beat, signatures[])` replaces proposer
    /// trust for the certified score itself and is the next step.
    ///
    /// Enforced on-chain:
    ///   - the competition is continuous,
    ///   - the new best beats the current best by at least `epsilonMicros` (sub-epsilon
    ///     beats and regressions revert — they never move the bar or pay),
    ///   - `marginalWei == weiPerMicro * (newBestMicros - bestMicros)` and does not
    ///     exceed the remaining pool (conservation: `spentWei` can never exceed escrow).
    ///
    /// Under `RecordBounty` the marginal is paid to `researcher` immediately. Under
    /// `TimeAtTopStreaming` the record only seizes the top spot (`marginalWei` must be
    /// 0; payment happens in `tickEpoch`). The `RecordBeat` event is the verifiable
    /// leaderboard row.
    function recordBeat(
        uint64 competitionId,
        address researcher,
        bytes32 candidateId,
        int256 newBestMicros,
        uint256 marginalWei
    ) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotRecordAuthority(msg.sender, c.proposer);
        // Lifecycle gate: distribute() is terminal. Once a competition is settled the
        // continuous window is closed, so no further marginal can be paid out of the
        // residual escrow through the record path (MECHANISM.md §5 liveness intent).
        if (c.settled) revert ContinuousClosed(competitionId);

        ContinuousState storage s = continuousStates[competitionId];
        if (!s.exists) revert NotContinuous(competitionId);

        // Strict epsilon-margin: the marginal must clear epsilon AND be positive.
        int256 marginalMicros = newBestMicros - s.bestMicros;
        if (marginalMicros < s.epsilonMicros || marginalMicros <= 0) {
            revert SubEpsilonBeat(newBestMicros, s.bestMicros, s.epsilonMicros);
        }

        if (s.streaming) {
            // Streaming records seize the top spot but pay nothing on the beat.
            if (marginalWei != 0) revert MarginalMismatch(marginalWei, 0);
            s.bestMicros = newBestMicros;
            s.topHolder = researcher;
            emit RecordBeat(competitionId, researcher, candidateId, newBestMicros, 0, s.epoch);
            return;
        }

        // RecordBounty: marginalWei must be the exact marginal, bounded by the pool.
        uint256 expected = s.weiPerMicro * uint256(marginalMicros);
        if (marginalWei != expected) revert MarginalMismatch(marginalWei, expected);

        // `escrowedWei` is the single source of remaining balance, shared with
        // `distribute`; a continuous payout draws it down so the same wei can never be
        // paid twice across the continuous and terminal paths (conservation).
        if (marginalWei > c.escrowedWei) revert Overdistribution(marginalWei, c.escrowedWei);

        // Effects before interaction: advance the best and draw down the pool first so
        // a re-entrant payee cannot double-record.
        s.bestMicros = newBestMicros;
        s.topHolder = researcher;
        s.spentWei += marginalWei;
        c.escrowedWei -= marginalWei;

        if (marginalWei > 0) {
            (bool ok,) = researcher.call{ value: marginalWei }("");
            if (!ok) revert TransferFailed(researcher, marginalWei);
        }
        emit RecordBeat(competitionId, researcher, candidateId, newBestMicros, marginalWei, s.epoch);
    }

    /// Advance the epoch and, under `TimeAtTopStreaming`, credit the current top
    /// holder `min(weiPerEpoch, remaining pool)`. Authority-gated to the proposer
    /// (the same referee-auth / k-of-n seam as `recordBeat`). Returns the wei
    /// credited this epoch (0 if the pool is exhausted). For `RecordBounty` this is a
    /// pure epoch advance (no per-epoch pay; that is the deadline/window seam).
    function tickEpoch(uint64 competitionId) external returns (uint256) {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotRecordAuthority(msg.sender, c.proposer);
        // Lifecycle gate: distribute() is terminal — a settled competition can no
        // longer credit epochs out of the residual escrow (see recordBeat).
        if (c.settled) revert ContinuousClosed(competitionId);

        ContinuousState storage s = continuousStates[competitionId];
        if (!s.exists) revert NotContinuous(competitionId);

        s.epoch += 1;
        if (!s.streaming) return 0; // RecordBounty pays on the beat, not per epoch.
        if (s.topHolder == address(0)) revert NoTopHolder(competitionId);

        // `escrowedWei` is the shared remaining-balance source (see `recordBeat`).
        uint256 remaining = c.escrowedWei;
        uint256 credited = s.weiPerEpoch < remaining ? s.weiPerEpoch : remaining;
        if (credited == 0) {
            emit EpochCredited(competitionId, s.topHolder, 0, s.epoch, s.bestMicros);
            return 0;
        }

        // Effects before interaction.
        s.spentWei += credited;
        c.escrowedWei -= credited;
        address holder = s.topHolder;
        (bool ok,) = holder.call{ value: credited }("");
        if (!ok) revert TransferFailed(holder, credited);
        emit EpochCredited(competitionId, holder, credited, s.epoch, s.bestMicros);
        return credited;
    }

    // --- Continuous views --------------------------------------------------

    function continuousBest(uint64 competitionId) external view returns (int256) {
        return continuousStates[competitionId].bestMicros;
    }

    function continuousSpent(uint64 competitionId) external view returns (uint256) {
        return continuousStates[competitionId].spentWei;
    }

    function continuousTopHolder(uint64 competitionId) external view returns (address) {
        return continuousStates[competitionId].topHolder;
    }

    // --- M4: privacy tiers + structural attestation (docs/PRIVACY.md) ------

    /// Declare a competition's privacy tier and the TEE its Referee must attest to.
    /// Proposer-gated. `tier` mirrors `PrivacyTier` (0..3) and `tee` mirrors `TeeType`
    /// (0..4); both default to 0 (BlackBox / None — the privacy-easy default) when
    /// never set. This records the DECLARED privacy posture; the enforcement of the
    /// pick-at-most-two-of-three exfiltration rule and feedback gating is off-chain
    /// (`autoresearch_runtime::privacy`), because it governs how the Referee runs the
    /// scorer, not how the chain moves money.
    function setPrivacy(uint64 competitionId, uint8 tier, uint8 tee) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotProposer(msg.sender, c.proposer);
        privacyTier[competitionId] = tier;
        requiredTee[competitionId] = tee;
        emit PrivacySet(competitionId, tier, tee);
    }

    /// Commit the structural attestation hash for a scored candidate.
    ///
    /// Authority-gated to the proposer, who routes the off-chain Referee's certified
    /// reports — the SAME authority seam as `recordBeat` and `resolveDispute`. The
    /// Referee scores the candidate inside (a stand-in for) a TEE and produces an
    /// attestation report; its keccak hash is committed here so a disputer can later
    /// prove WHICH report a score was produced against.
    ///
    /// SEAM — on-chain quote verification is UNIMPLEMENTED (docs/PRIVACY.md §12,
    /// ARCHITECTURE §7). This function stores the hash as a STRUCTURAL COMMITMENT
    /// ONLY. It does NOT verify a hardware quote signature (DCAP/KDS for Intel TDX,
    /// NSM for AWS Nitro, the GCP/Azure equivalents), does NOT pin the enclave
    /// measurement against an expected value, and does NOT bind a challenge nonce.
    /// "Attestation committed" therefore does NOT mean "attestation valid": a
    /// malicious host who forged a structurally-correct report would currently pass.
    /// Closing the seam — quote-signature recovery + on-chain measurement pinning +
    /// nonce binding, mirroring the trading blueprint's `TradeValidator` m-of-n
    /// EIP-712 verification pattern and the agent-sandbox attestation gap — is the
    /// documented next step; until then a Proposer relying on white-box modes must
    /// treat the host as NOT YET cryptographically verified and gate that risk
    /// operationally.
    function commitAttestation(uint64 competitionId, bytes32 candidateId, bytes32 attestationHash) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotAttestationAuthority(msg.sender, c.proposer);
        if (attestationHash == bytes32(0)) revert EmptyAttestationHash(competitionId, candidateId);
        attestationHashes[competitionId][candidateId] = attestationHash;
        emit AttestationCommitted(competitionId, candidateId, attestationHash);
    }

    /// Read back a committed attestation hash for recomputation / dispute. Returns the
    /// zero hash if none was committed. This is a COMMITMENT readout, not a proof of
    /// verification (see `commitAttestation`).
    function attestationOf(uint64 competitionId, bytes32 candidateId) external view returns (bytes32) {
        return attestationHashes[competitionId][candidateId];
    }

    // --- M5: scorer kind (which referee adjudicated) -----------------------

    /// Declare which scorer kind adjudicates a competition. Proposer-gated (the same
    /// authority seam as `setPrivacy` — it is part of the declared spec). `kind` mirrors
    /// `autoresearch_runtime::types::ScorerKind` (0..=3) and defaults to 0 (HeldOutEval)
    /// when never set. This records the DECLARED referee class on the verifiable
    /// leaderboard; the scoring itself is off-chain by design (the chain never runs an
    /// oracle, a privileged device, or a human panel — see docs/ARCHITECTURE.md §3).
    function setScorerKind(uint64 competitionId, uint8 kind) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotScorerAuthority(msg.sender, c.proposer);
        if (kind > 3) revert BadScorerKind(kind);
        scorerKind[competitionId] = kind;
        emit ScorerKindSet(competitionId, kind);
    }

    /// Read back a competition's declared scorer kind (0 = HeldOutEval default). This is
    /// a record of the declared referee class, not a proof that scoring ran under it
    /// (that lives in the off-chain certified evidence + attestation commitment).
    function scorerKindOf(uint64 competitionId) external view returns (uint8) {
        return scorerKind[competitionId];
    }

    // --- M6: collaborative contribution settlement (docs/MECHANISM.md §6) ---

    /// Settle a `Collaborative` competition by paying contributors their CONTRIBUTION
    /// SHARE of the escrowed pool.
    ///
    /// This is the collaborative counterpart of `distribute`. Where `distribute` pays a
    /// ranked field of RIVAL submissions, `distributeShares` pays the contributors to
    /// ONE shared artifact by their share. The off-chain runner
    /// (`autoresearch_protocol::collaborative::run_collaborative`) computes the share
    /// amounts from HELD-OUT-GATED, single-permutation marginal attribution (a
    /// first-difference estimator over a canonical fold order, not a permutation-invariant
    /// Shapley value; a free-rider's zero-marginal delta yields a zero amount, which is
    /// simply omitted from `contributors`/`amounts`); the chain CONSERVES and pays. It is
    /// gated identically to `distribute`:
    ///   - only the proposer (settlement authority) may call it,
    ///   - only on/after the deadline,
    ///   - only once (the competition is terminal after settlement),
    ///   - sum(amounts) <= escrowed balance (conservation — never mint).
    ///
    /// SEAM — the on-chain quote/committee verification of the off-chain attribution is
    /// the same documented seam as `distribute`/`recordBeat`: the chain enforces
    /// conservation and authority, not the correctness of the marginal-attribution
    /// estimate (that is the off-chain Referee + Validator spot-check path, MECHANISM §6.2).
    function distributeShares(uint64 competitionId, address[] calldata contributors, uint256[] calldata amounts)
        external
    {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotShareAuthority(msg.sender, c.proposer);
        if (block.timestamp < c.deadline) revert BeforeDeadline(uint64(block.timestamp), c.deadline);
        if (c.settled) revert AlreadySettled(competitionId);
        if (contributors.length != amounts.length) revert LengthMismatch(contributors.length, amounts.length);

        uint256 total = 0;
        for (uint256 i = 0; i < amounts.length; i++) {
            total += amounts[i];
        }
        if (total > c.escrowedWei) revert Overdistribution(total, c.escrowedWei);

        // Effects before interactions: mark settled (terminal) and draw down escrow
        // first so a re-entrant contributor contract cannot trigger a second settlement.
        c.settled = true;
        c.escrowedWei -= total;

        for (uint256 i = 0; i < contributors.length; i++) {
            uint256 amount = amounts[i];
            if (amount == 0) continue; // a zero-share free-rider is paid nothing
            (bool ok,) = contributors[i].call{ value: amount }("");
            if (!ok) revert TransferFailed(contributors[i], amount);
            emit PayoutMade(competitionId, contributors[i], amount);
        }

        emit ContributionsSettled(competitionId, total, contributors.length);
    }

    // --- M6: certified-artifact marketplace (docs/MECHANISM.md §10) ---------

    /// List a certified artifact for sale. `msg.sender` is the seller. The chain holds
    /// only the minimal listing metadata (artifact hash + price + provenance +
    /// exclusivity); the rich certified-lift / license metadata lives off-chain and
    /// travels in the `ArtifactListing` domain type. Both WINNING and (gate-clearing)
    /// LOSING artifacts may be listed — the off-chain layer enforces the consent /
    /// sub-gate-disclosure rules and binds the certified lift to the Referee attestation
    /// (`autoresearch_runtime::marketplace`); the chain enforces that the price is
    /// positive AND that `provenanceCompetitionId` is a REAL competition, so the
    /// on-chain provenance field is a genuine competition id and not an arbitrary uint —
    /// then settles the purchase.
    ///
    /// Returns the new listing id (also emitted in `ArtifactListed`).
    function listArtifact(bytes32 artifactRef, uint256 price, uint64 provenanceCompetitionId, bool exclusive)
        external
        returns (uint256 listingId)
    {
        if (price == 0) revert ZeroPrice();
        // Provenance must reference a real competition: an arbitrary uint64 with no
        // linkage would make the on-chain ArtifactSold ledger attest a fake origin.
        _requireExists(provenanceCompetitionId);
        listingId = nextListingId;
        nextListingId = listingId + 1;
        listings[listingId] = Listing({
            seller: msg.sender,
            artifactRef: artifactRef,
            price: price,
            provenanceCompetitionId: provenanceCompetitionId,
            exclusive: exclusive,
            sold: false,
            exists: true
        });
        emit ArtifactListed(listingId, msg.sender, artifactRef, price, provenanceCompetitionId);
    }

    /// Buy a listed artifact, transferring `price` to the seller. `msg.value` MUST equal
    /// the listing price (no over/underpay). An EXCLUSIVE listing is marked sold and a
    /// second buy reverts (`ListingSold`); a non-exclusive listing may be bought
    /// repeatedly. The artifact bytes are off-chain — this settles the PAYMENT and emits
    /// the verifiable `ArtifactSold` ledger row; off-chain provenance + certified lift
    /// (carried in the `Sale` domain type) travel to the buyer.
    function buyArtifact(uint256 listingId) external payable {
        Listing storage l = listings[listingId];
        if (!l.exists) revert UnknownListing(listingId);
        if (l.exclusive && l.sold) revert ListingSold(listingId);
        if (msg.value != l.price) revert WrongPrice(msg.value, l.price);
        if (msg.sender == l.seller) revert SelfPurchase(msg.sender);

        // Effects before interaction: mark an exclusive listing sold first so a
        // re-entrant seller contract cannot re-enter and double-sell.
        if (l.exclusive) l.sold = true;
        address seller = l.seller;
        uint256 price = l.price;

        (bool ok,) = seller.call{ value: price }("");
        if (!ok) revert TransferFailed(seller, price);
        emit ArtifactSold(listingId, msg.sender, seller, price);
    }

    /// Read back a listing (minimal on-chain metadata). Rich metadata is off-chain.
    function listingOf(uint256 listingId)
        external
        view
        returns (
            address seller,
            bytes32 artifactRef,
            uint256 price,
            uint64 provenanceCompetitionId,
            bool exclusive,
            bool sold
        )
    {
        Listing storage l = listings[listingId];
        return (l.seller, l.artifactRef, l.price, l.provenanceCompetitionId, l.exclusive, l.sold);
    }

    // --- Commit-reveal -----------------------------------------------------

    /// Record a commitment directly (test/EOA path). The Tangle job path routes the
    /// same write through `onJobCall` (see below); both store into the same mapping.
    function commitCandidate(uint64 competitionId, bytes32 commitment) external {
        _requireExists(competitionId);
        commitments[competitionId][msg.sender] = commitment;
        emit CandidateCommitted(competitionId, msg.sender, commitment);
    }

    /// Reveal a previously committed artifact reference. Reverts unless
    /// `keccak256(abi.encode(artifactRef, salt))` equals the stored commitment.
    /// This is the anti-copy guarantee: a researcher cannot reveal an artifact they
    /// did not commit to before the deadline.
    ///
    /// `abi.encode` is length-prefixed (unlike `abi.encodePacked`), so the boundary
    /// between the variable-length `artifactRef` and the `salt` is unambiguous and the
    /// commitment cannot be satisfied by a different (artifactRef, salt) pair.
    function revealCandidate(uint64 competitionId, string calldata artifactRef, bytes32 salt) external {
        _requireExists(competitionId);
        bytes32 stored = commitments[competitionId][msg.sender];
        if (stored == bytes32(0)) revert NoCommitment(competitionId, msg.sender);

        bytes32 computed = keccak256(abi.encode(artifactRef, salt));
        if (computed != stored) revert RevealMismatch(competitionId, msg.sender);

        revealed[competitionId][msg.sender] = true;
        emit CandidateRevealed(competitionId, msg.sender, artifactRef);
    }

    /// Pure verification helper — the same check `revealCandidate` enforces, exposed
    /// for off-chain pre-validation and tests.
    function verifyReveal(uint64 competitionId, address researcher, string calldata artifactRef, bytes32 salt)
        external
        view
        returns (bool)
    {
        return keccak256(abi.encode(artifactRef, salt)) == commitments[competitionId][researcher];
    }

    // --- Settlement --------------------------------------------------------

    /// Distribute the escrowed pool to winners. The Referee computes `winners` and
    /// `amounts` off-chain (rank + RewardSchedule); the chain enforces:
    ///   - only the proposer may trigger settlement,
    ///   - only on/after the deadline,
    ///   - only once,
    ///   - sum(amounts) <= escrowed balance (conservation — never mint).
    /// Any dust left after a partial distribution stays escrowed (it is not minted
    /// and not stranded: a proposer-only `reclaim` after settlement is a later seam).
    function distribute(uint64 competitionId, address[] calldata winners, uint256[] calldata amounts) external {
        Competition storage c = competitions[competitionId];
        if (!c.exists) revert UnknownCompetition(competitionId);
        if (msg.sender != c.proposer) revert NotProposer(msg.sender, c.proposer);
        if (block.timestamp < c.deadline) revert BeforeDeadline(uint64(block.timestamp), c.deadline);
        if (c.settled) revert AlreadySettled(competitionId);
        if (winners.length != amounts.length) revert LengthMismatch(winners.length, amounts.length);

        uint256 total = 0;
        for (uint256 i = 0; i < amounts.length; i++) {
            total += amounts[i];
        }
        if (total > c.escrowedWei) revert Overdistribution(total, c.escrowedWei);

        // Effects before interactions: mark settled and draw down escrow first so a
        // re-entrant winner contract cannot trigger a second distribution.
        c.settled = true;
        c.escrowedWei -= total;

        for (uint256 i = 0; i < winners.length; i++) {
            uint256 amount = amounts[i];
            if (amount == 0) continue;
            (bool ok,) = winners[i].call{ value: amount }("");
            if (!ok) revert TransferFailed(winners[i], amount);
            emit PayoutMade(competitionId, winners[i], amount);
        }

        emit CompetitionSettled(competitionId, total);
    }

    // --- Views -------------------------------------------------------------

    function escrowOf(uint64 competitionId) external view returns (uint256) {
        return competitions[competitionId].escrowedWei;
    }

    function isSettled(uint64 competitionId) external view returns (bool) {
        return competitions[competitionId].settled;
    }

    // --- Lifecycle hooks ---------------------------------------------------

    function onRegister(address, bytes calldata) external payable virtual override onlyFromTangle {
        // Operator (Node Operator) registration. No-op.
    }

    function onRequest(uint64, address, address[] calldata, bytes calldata, uint64, address, uint256)
        external
        payable
        virtual
        override
        onlyFromTangle
    {
        // Service request acceptance. No-op.
    }

    /// Fired before operator execution. We capture commit-reveal commitments here
    /// because the protocol does not guarantee `inputs` are forwarded to
    /// `onJobResult`. `tx.origin` is the researcher that initiated the job call.
    function onJobCall(uint64, uint8 job, uint64, bytes calldata inputs)
        external
        payable
        virtual
        override
        onlyFromTangle
    {
        if (job == JOB_COMMIT_CANDIDATE) {
            // CommitCandidateRequest = (uint64 competition_id, bytes32 commitment)
            (uint64 competitionId, bytes32 commitment) = abi.decode(inputs, (uint64, bytes32));
            if (competitions[competitionId].exists) {
                commitments[competitionId][tx.origin] = commitment;
                emit CandidateCommitted(competitionId, tx.origin, commitment);
            }
        }
    }

    /// Fired after an operator submits a job result. The Tangle-driven reveal path
    /// verifies the commitment here; the economic settlement path runs through the
    /// proposer-gated `distribute` above (the chain never trusts an operator to
    /// decide amounts).
    function onJobResult(uint64, uint8 job, uint64 jobCallId, address operator, bytes calldata, bytes calldata outputs)
        external
        payable
        virtual
        override
        onlyFromTangle
    {
        if (job == JOB_REVEAL_CANDIDATE) {
            // RevealCandidateRequest = (uint64 competition_id, bytes32 commitment,
            //                           string artifact_ref, bytes32 salt)
            // Salt is bytes32 (canonical across both reveal paths) and the commitment
            // uses abi.encode (length-prefixed) so the EOA and Tangle paths are
            // interchangeable and the (artifactRef, salt) boundary is unambiguous.
            (uint64 competitionId,, string memory artifactRef, bytes32 salt) =
                abi.decode(outputs, (uint64, bytes32, string, bytes32));
            bytes32 stored = commitments[competitionId][tx.origin];
            if (stored == bytes32(0)) revert NoCommitment(competitionId, tx.origin);
            if (keccak256(abi.encode(artifactRef, salt)) != stored) {
                revert RevealMismatch(competitionId, tx.origin);
            }
            revealed[competitionId][tx.origin] = true;
            emit CandidateRevealed(competitionId, tx.origin, artifactRef);
        } else if (job == JOB_REPORT_SCORE) {
            emit ScoreReported(0, operator, jobCallId);
        }
        // CREATE / JOIN / SETTLE / CHALLENGE / TICK: economic effects run through the
        // explicit escrow + distribute path, not through operator results.
    }

    /// Number of operator results required before `onJobResult` consensus.
    /// 0 = protocol default (all assigned operators).
    function getRequiredResultCount(uint64, uint8) external view virtual override returns (uint32) {
        return 0;
    }

    // --- internals ---------------------------------------------------------

    function _requireExists(uint64 competitionId) internal view {
        if (!competitions[competitionId].exists) revert UnknownCompetition(competitionId);
    }
}
