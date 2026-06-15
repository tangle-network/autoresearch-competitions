//! The competition → marketplace flywheel (`docs/MECHANISM.md §10`).
//!
//! Competitions don't just settle a prize — they **manufacture certified-artifact
//! inventory**. Every competition, across its life, produces scored artifacts:
//! winners, near-misses, and losers, each carrying a Referee-attested [`Lift`] on
//! some distribution. This module turns that inventory into a listable, sellable
//! marketplace, reusing the exact trust primitives the competition uses (the §4
//! promotion [`Gate`] and the certified [`Lift`]) so a sale inherits the trust model
//! for free.
//!
//! # What this module is
//!
//! A small, deterministic, in-memory settlement model for listing and selling
//! certified artifacts. It owns the *rules* of a valid listing and a valid sale:
//!
//! - **Consent / licensing (required).** A listing is invalid without the
//!   producer's consent; [`Marketplace::list`] rejects an unconsented listing
//!   ([`MarketError::Unconsented`]).
//! - **Provenance + certified lift are bound to a Referee attestation, not
//!   seller-asserted.** Every [`ArtifactListing`] carries its producing
//!   [`CompetitionId`] and the certified [`Lift`], but [`Marketplace::list`] does NOT
//!   trust those struct fields: it requires the [`CertifiedAttestation`] the Referee
//!   produced when the competition certified the artifact, and rejects the listing
//!   ([`MarketError::ForgedListing`]) unless the listed `artifact_ref`, `certified_lift`
//!   and `provenance` all match that attestation. A seller therefore cannot mint a
//!   listing with a fabricated lift / provenance — the numbers are the ones the
//!   competition settled, so a [`Sale`] records verifiable provenance, not a vendor
//!   claim. (The on-chain `listArtifact` additionally gates `provenanceCompetitionId`
//!   through `_requireExists`, so the chain's provenance is a real competition id.)
//! - **No double-sell of an exclusive license.** Selling an exclusive listing marks
//!   it sold; a second [`Marketplace::buy`] of the same listing is rejected
//!   ([`MarketError::AlreadySold`]). Non-exclusive licenses may be sold repeatedly.
//! - **Monotone pricing in certified lift.** [`price_by_lift`] is non-decreasing in
//!   the certified lift delta, so a stronger artifact never prices below a weaker one
//!   under the same policy.
//!
//! Both **winning AND losing** (still gate-clearing-on-*some*-distribution) artifacts
//! can be listed — a candidate that placed 4th on Proposer X's metric may be exactly
//! what Buyer Y needs on *their* distribution (§10).
//!
//! # Seam (honest scope)
//!
//! This is the host-independent settlement logic. It **binds** the listed lift /
//! provenance to the producing competition's Referee attestation at list time (so the
//! seller cannot forge them), but it does **not**:
//! - re-score the artifact on the buyer's distribution (the §10 "competition-of-one
//!   against the buyer's held-out set" — that is the off-chain Referee path, the same
//!   machinery the competition runners use). The attestation this module verifies
//!   against is the *producing-distribution* certification; re-certification on the
//!   buyer's distribution remains the seam. The trust source for the producing-side
//!   number is the attestation, not the seller's struct fields.
//! - move on-chain value (the `CompetitionManager.listArtifact` / `buyArtifact` path
//!   does the escrowless on-chain settlement);
//! - police held-out-set leakage (a sale re-scores on the *buyer's* data and never
//!   reveals the original sealed held-out set — PRIVACY).
//!
//! The `price_certified_lift` the chain settles is computed here from the certified
//! number; the chain conserves and transfers.

use serde::{Deserialize, Serialize};

use crate::types::{ArtifactRef, CompetitionId, Gate, Lift, Measurement};

/// Index of a listing in a [`Marketplace`]. Stable for the lifetime of the market.
pub type ListingId = usize;

/// Errors from the marketplace. Fail-closed: any rule violation rejects the
/// list/buy rather than silently producing an invalid sale.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MarketError {
    /// The producer did not consent to listing this artifact. A sale is invalid
    /// without consent / a stated license (MECHANISM §10) — fail-closed.
    #[error("artifact not listed for sale: the producer has not consented (MECHANISM §10)")]
    Unconsented,
    /// A listing must carry a positive price. A zero-priced listing cannot settle a
    /// sale and is rejected at list time.
    #[error("listing price must be positive; got zero")]
    ZeroPrice,
    /// The certified lift does not clear the gate AND the listing was not flagged as
    /// a disclosed sub-gate artifact. Gate-clearing artifacts list freely; a sub-gate
    /// ("losing") artifact is listable too, but only when the seller explicitly
    /// discloses it (`disclose_sub_gate == true`) so the buyer prices it knowing it
    /// did not clear the producing competition's gate.
    #[error(
        "certified lift does not clear the gate and the sub-gate status was not disclosed; \
         a losing artifact must be listed with disclosure (MECHANISM §10)"
    )]
    UndisclosedSubGate,
    /// The listing id does not exist.
    #[error("unknown listing {0}")]
    UnknownListing(ListingId),
    /// An exclusive listing was already sold; it cannot be sold a second time.
    #[error("listing {0} is an exclusive license already sold; no double-sell")]
    AlreadySold(ListingId),
    /// The listing's seller-supplied provenance / certified lift / artifact_ref does not
    /// match the Referee [`CertifiedAttestation`] for the producing competition. The
    /// seller cannot mint a listing with numbers the competition did not certify —
    /// fail-closed (MECHANISM §10).
    #[error(
        "listing does not match the certifying attestation (seller-forged provenance \
         or certified lift); rejected fail-closed (MECHANISM §10)"
    )]
    ForgedListing,
}

/// How a certified lift is priced into wei. The price is a base fee plus a
/// per-lift-point rate applied to the **clamped, non-negative** certified delta, so
/// the function is monotone non-decreasing in the certified lift (a stronger artifact
/// never prices below a weaker one). The rate stands in for the buyer-distribution
/// proxy: in a full deployment the lift is re-certified on the buyer's held-out set
/// (MECHANISM §10) and this same curve prices *that* number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingPolicy {
    /// Flat listing fee paid regardless of lift (covers a zero-or-negative lift floor).
    pub base_wei: u128,
    /// Wei per micro-unit (1e-6) of certified lift delta above zero.
    pub wei_per_micro_lift: u128,
}

impl PricingPolicy {
    /// A simple policy: `base_wei` floor plus `wei_per_micro_lift` per micro-point of
    /// positive certified lift.
    #[must_use]
    pub fn new(base_wei: u128, wei_per_micro_lift: u128) -> Self {
        Self {
            base_wei,
            wei_per_micro_lift,
        }
    }
}

/// Price a certified [`Lift`] under a [`PricingPolicy`] (MECHANISM §10).
///
/// The certified delta is clamped to `>= 0` and converted to integer micro-units
/// before the rate is applied, so:
/// - the result is **monotone non-decreasing** in `lift.delta`;
/// - a zero-or-negative lift prices at exactly `base_wei` (the floor);
/// - the arithmetic is integer and saturating (no overflow, no float deciding price).
///
/// A non-finite delta (`NaN`/`inf`, e.g. from an adversarial scorer) prices at the
/// floor — fail-closed, never an unbounded price.
#[must_use]
pub fn price_by_lift(lift: &Lift, policy: &PricingPolicy) -> u128 {
    let delta = lift.delta;
    // Fail-closed on non-finite input: floor price, never an unbounded one.
    let positive = if delta.is_finite() && delta > 0.0 {
        delta
    } else {
        0.0
    };
    // Convert to integer micro-units (1e-6) before applying the rate so floats never
    // decide the wei amount. `positive` is finite and >= 0 here.
    let micros = (positive * 1_000_000.0).floor();
    // `micros` is finite and non-negative; cap into u128 range defensively.
    let micros_u128 = if micros >= u128::MAX as f64 {
        u128::MAX
    } else {
        micros as u128
    };
    let lift_component = micros_u128.saturating_mul(policy.wei_per_micro_lift);
    policy.base_wei.saturating_add(lift_component)
}

/// The Referee's certification of an artifact — the unforgeable trust source a listing
/// is bound against. This is the producing competition's signed evidence row: the
/// artifact that was scored, the competition that scored it, the certified [`Lift`], the
/// supporting [`Measurement`], and the attestation hash committed on-chain at
/// `REPORT_SCORE`. A seller does not produce this; the Referee does. [`Marketplace::list`]
/// rejects any listing whose `artifact_ref` / `certified_lift` / `provenance` does not
/// match the attestation, which is what makes the listed provenance + lift unforgeable
/// (MECHANISM §10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CertifiedAttestation {
    /// The artifact the Referee certified (content/sealed ref). A listing must sell
    /// exactly this artifact.
    pub artifact_ref: ArtifactRef,
    /// The competition that produced the certification — the provenance a listing claims
    /// must equal this.
    pub provenance: CompetitionId,
    /// The Referee-certified lift. A listing's `certified_lift` must equal this exactly.
    pub certified_lift: Lift,
    /// The certified measurement behind the lift, used for the gate check (so the seller
    /// cannot supply a fabricated measurement that makes a forged lift "clear" the gate).
    pub measurement: Measurement,
    /// keccak hash of the TEE attestation under which scoring ran, hex-encoded (empty
    /// for non-TEE referees). Recorded for provenance; the binding above is by value.
    pub attestation_hash: String,
}

impl CertifiedAttestation {
    /// Whether a listing's seller-supplied provenance, certified lift, and artifact_ref
    /// all match what the Referee certified. A `false` here is a forged listing.
    ///
    /// [`Lift`] uses `f64`, so two lifts are equal iff every field is bit-equal; a
    /// `NaN`-poisoned certified lift can never match (NaN != NaN), so it is rejected
    /// fail-closed exactly like the gate does — a forged or corrupted attestation never
    /// validates a listing.
    #[must_use]
    pub fn matches(&self, listing: &ArtifactListing) -> bool {
        listing.artifact_ref == self.artifact_ref
            && listing.provenance == self.provenance
            && listing.certified_lift == self.certified_lift
    }
}

/// A certified artifact listed for sale. Provenance ([`Self::provenance`]) and the
/// certified [`Lift`] ([`Self::certified_lift`]) travel with the artifact, but they are
/// **verified against a [`CertifiedAttestation`] at list time**, not trusted as
/// seller-asserted, so a buyer prices against a verifiable track record (MECHANISM §10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactListing {
    /// Content/sealed reference to the artifact being sold.
    pub artifact_ref: ArtifactRef,
    /// The seller (the producing researcher, or a holder they assigned the license to).
    pub seller: String,
    /// The Referee-certified lift this artifact carries (on its producing
    /// distribution). Re-certification on the buyer's distribution is the §10 seam.
    pub certified_lift: Lift,
    /// Asking price in wei. Must be positive (enforced at list time).
    pub price_wei: u128,
    /// The stated license terms (an opaque string at this layer, e.g. an SPDX id or a
    /// terms-URI). Required: a sale is invalid without a stated license (MECHANISM §10).
    pub license: String,
    /// True if the license is **exclusive** — it may be sold at most once. A
    /// non-exclusive license may be sold to many buyers.
    pub exclusive: bool,
    /// The competition that produced (certified) this artifact — its provenance.
    pub provenance: CompetitionId,
    /// The producer's consent to list. A sale is invalid without it (MECHANISM §10).
    pub consented: bool,
    /// True if the seller explicitly discloses that this artifact did NOT clear its
    /// producing competition's gate (a "losing" but still potentially useful artifact,
    /// MECHANISM §10). A gate-clearing artifact does not need this set.
    pub disclose_sub_gate: bool,
}

impl ArtifactListing {
    /// Whether this listing's certified lift clears `gate` (with the certified
    /// measurement). A gate-clearing listing may be sold without sub-gate disclosure.
    #[must_use]
    pub fn clears_gate(&self, gate: &Gate, measurement: &Measurement) -> bool {
        gate.clears(&self.certified_lift, measurement)
    }
}

/// A completed sale: which listing, to whom, at what price, carrying the artifact's
/// provenance and certified lift forward to the buyer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sale {
    pub listing: ListingId,
    pub artifact_ref: ArtifactRef,
    pub seller: String,
    pub buyer: String,
    pub price_wei: u128,
    /// Provenance carried forward to the buyer (the producing competition).
    pub provenance: CompetitionId,
    /// Certified lift carried forward to the buyer.
    pub certified_lift: Lift,
}

/// An in-memory certified-artifact marketplace. Owns the list/buy rules and the sale
/// ledger; it does not move on-chain value (that is the `CompetitionManager`
/// list/buy path) and does not re-score on the buyer's distribution (the §10 Referee
/// seam).
#[derive(Clone, Debug, Default)]
pub struct Marketplace {
    listings: Vec<ArtifactListing>,
    /// Per-listing sold flag (parallel to `listings`). For an exclusive listing a set
    /// flag blocks a second sale; non-exclusive listings ignore it.
    sold: Vec<bool>,
    /// Every settled sale, in order — the ledger an indexer replays.
    sales: Vec<Sale>,
}

impl Marketplace {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// List a certified artifact for sale, **bound to the Referee's
    /// [`CertifiedAttestation`]** for the producing competition.
    ///
    /// Fail-closed validation (MECHANISM §10), in order:
    /// - the listed `artifact_ref` / `certified_lift` / `provenance` must match
    ///   `attestation` ([`MarketError::ForgedListing`]) — this is the forgery defense:
    ///   the seller cannot list numbers the Referee did not certify;
    /// - the producer must have consented ([`MarketError::Unconsented`]);
    /// - the price must be positive ([`MarketError::ZeroPrice`]);
    /// - a sub-gate ("losing") artifact must be disclosed: if the *attested* lift does
    ///   not clear `gate` and `disclose_sub_gate` is false, the listing is rejected
    ///   ([`MarketError::UndisclosedSubGate`]). A gate-clearing artifact lists freely.
    ///
    /// The gate check uses the attestation's `measurement`, not a separate caller arg,
    /// so a seller cannot pair a forged lift with a fabricated matching measurement to
    /// slip past the sub-gate-disclosure rule.
    ///
    /// # Errors
    /// One of the [`MarketError`] variants above if the listing is invalid.
    pub fn list(
        &mut self,
        listing: ArtifactListing,
        gate: &Gate,
        attestation: &CertifiedAttestation,
    ) -> Result<ListingId, MarketError> {
        // Forgery defense FIRST: the listed provenance + certified lift + artifact_ref
        // must be the ones the Referee certified. A seller-fabricated lift/provenance
        // (even with a matching fabricated measurement) is rejected here, before any
        // other rule, because everything downstream prices off these numbers.
        if !attestation.matches(&listing) {
            return Err(MarketError::ForgedListing);
        }
        if !listing.consented {
            return Err(MarketError::Unconsented);
        }
        if listing.price_wei == 0 {
            return Err(MarketError::ZeroPrice);
        }
        // A losing (sub-gate) artifact is listable, but only with disclosure so the
        // buyer prices it knowing it did not clear the producing competition's gate.
        // The gate check uses the ATTESTED measurement (not seller-supplied), closing
        // the fabricated-measurement hole.
        if !listing.clears_gate(gate, &attestation.measurement) && !listing.disclose_sub_gate {
            return Err(MarketError::UndisclosedSubGate);
        }
        let id = self.listings.len();
        self.listings.push(listing);
        self.sold.push(false);
        Ok(id)
    }

    /// Buy a listing, recording the sale and transferring provenance + certified lift
    /// to the buyer.
    ///
    /// For an **exclusive** listing, a second buy is rejected
    /// ([`MarketError::AlreadySold`]) — the license is sold exactly once. A
    /// non-exclusive listing may be bought repeatedly (each buy is a distinct sale).
    ///
    /// # Errors
    /// - [`MarketError::UnknownListing`] if `id` is out of range.
    /// - [`MarketError::AlreadySold`] if `id` is an exclusive listing already sold.
    pub fn buy(&mut self, id: ListingId, buyer: &str) -> Result<Sale, MarketError> {
        let listing = self
            .listings
            .get(id)
            .ok_or(MarketError::UnknownListing(id))?;
        if listing.exclusive && self.sold[id] {
            return Err(MarketError::AlreadySold(id));
        }
        let sale = Sale {
            listing: id,
            artifact_ref: listing.artifact_ref.clone(),
            seller: listing.seller.clone(),
            buyer: buyer.to_string(),
            price_wei: listing.price_wei,
            provenance: listing.provenance,
            certified_lift: listing.certified_lift,
        };
        self.sold[id] = true;
        self.sales.push(sale.clone());
        Ok(sale)
    }

    /// A listing by id.
    #[must_use]
    pub fn listing(&self, id: ListingId) -> Option<&ArtifactListing> {
        self.listings.get(id)
    }

    /// Whether a listing has been sold at least once.
    #[must_use]
    pub fn is_sold(&self, id: ListingId) -> bool {
        self.sold.get(id).copied().unwrap_or(false)
    }

    /// The full sale ledger, in order.
    #[must_use]
    pub fn sales(&self) -> &[Sale] {
        &self.sales
    }

    /// Number of active listings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.listings.len()
    }

    /// Whether the market has no listings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.listings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_lift() -> Lift {
        Lift {
            delta: 0.35,
            ci_lower: 0.30,
            ci_upper: 0.40,
            n: 80,
        }
    }

    fn good_measurement() -> Measurement {
        Measurement {
            value: 0.85,
            ci_lower: 0.80,
            ci_upper: 0.90,
            n: 80,
            cost: 80.0,
        }
    }

    fn weak_lift() -> Lift {
        // A positive but sub-gate lift (lower bound below the default 0.02 floor).
        Lift {
            delta: 0.10,
            ci_lower: 0.001,
            ci_upper: 0.20,
            n: 80,
        }
    }

    fn listing(consented: bool, price: u128, exclusive: bool, lift: Lift) -> ArtifactListing {
        ArtifactListing {
            artifact_ref: ArtifactRef("artifact:winner".into()),
            seller: "0xseller".into(),
            certified_lift: lift,
            price_wei: price,
            license: "exclusive-v1".into(),
            exclusive,
            provenance: 7,
            consented,
            disclose_sub_gate: false,
        }
    }

    /// The Referee attestation matching the `listing` helper above (same artifact_ref,
    /// provenance, lift, and measurement). A listing built from `listing(.., lift)` is
    /// bound by `attestation_for(lift, measurement)`.
    fn attestation_for(lift: Lift, measurement: Measurement) -> CertifiedAttestation {
        CertifiedAttestation {
            artifact_ref: ArtifactRef("artifact:winner".into()),
            provenance: 7,
            certified_lift: lift,
            measurement,
            attestation_hash: "0xattest".into(),
        }
    }

    // --- listing rules -----------------------------------------------------

    #[test]
    fn cannot_list_without_consent() {
        let mut market = Marketplace::new();
        let err = market
            .list(
                listing(false, 1_000, true, good_lift()),
                &Gate::default(),
                &attestation_for(good_lift(), good_measurement()),
            )
            .unwrap_err();
        assert_eq!(err, MarketError::Unconsented);
        assert!(market.is_empty(), "a rejected listing must not be stored");
    }

    #[test]
    fn cannot_list_at_zero_price() {
        let mut market = Marketplace::new();
        let err = market
            .list(
                listing(true, 0, true, good_lift()),
                &Gate::default(),
                &attestation_for(good_lift(), good_measurement()),
            )
            .unwrap_err();
        assert_eq!(err, MarketError::ZeroPrice);
    }

    #[test]
    fn gate_clearing_artifact_lists_without_disclosure() {
        let mut market = Marketplace::new();
        let id = market
            .list(
                listing(true, 1_000, true, good_lift()),
                &Gate::default(),
                &attestation_for(good_lift(), good_measurement()),
            )
            .expect("a gate-clearing winner lists freely");
        assert_eq!(id, 0);
        assert_eq!(market.len(), 1);
    }

    #[test]
    fn losing_artifact_requires_disclosure() {
        let mut market = Marketplace::new();
        // A sub-gate artifact without disclosure is rejected...
        let err = market
            .list(
                listing(true, 1_000, true, weak_lift()),
                &Gate::default(),
                &attestation_for(weak_lift(), good_measurement()),
            )
            .unwrap_err();
        assert_eq!(err, MarketError::UndisclosedSubGate);

        // ...but with disclosure it lists (a losing artifact is sellable inventory).
        let mut disclosed = listing(true, 1_000, true, weak_lift());
        disclosed.disclose_sub_gate = true;
        let id = market
            .list(
                disclosed,
                &Gate::default(),
                &attestation_for(weak_lift(), good_measurement()),
            )
            .expect("a disclosed losing artifact is listable inventory");
        assert_eq!(id, 0);
    }

    #[test]
    fn forged_lift_listing_is_rejected() {
        // A seller fabricates a high certified_lift (and a matching fabricated
        // measurement) that the Referee never certified. The attestation carries the
        // REAL (weak, sub-gate) numbers; the listing must be rejected fail-closed —
        // the seller cannot list-and-sell at the inflated price.
        let mut market = Marketplace::new();
        let real_lift = weak_lift();
        let forged_lift = good_lift(); // a fat, gate-clearing lift the seller invented
        let attestation = attestation_for(real_lift, good_measurement());

        // (a) forged certified_lift does not match the attestation.
        let err = market
            .list(
                listing(true, 5_000, true, forged_lift),
                &Gate::default(),
                &attestation,
            )
            .unwrap_err();
        assert_eq!(err, MarketError::ForgedListing);
        assert!(market.is_empty(), "a forged listing must not be stored");

        // (b) forged provenance (wrong competition) is also rejected.
        let mut wrong_prov = listing(true, 5_000, true, real_lift);
        wrong_prov.provenance = 999;
        assert_eq!(
            market.list(wrong_prov, &Gate::default(), &attestation),
            Err(MarketError::ForgedListing),
        );

        // (c) forged artifact_ref (selling a different artifact than was certified).
        let mut wrong_ref = listing(true, 5_000, true, real_lift);
        wrong_ref.artifact_ref = ArtifactRef("artifact:other".into());
        assert_eq!(
            market.list(wrong_ref, &Gate::default(), &attestation),
            Err(MarketError::ForgedListing),
        );

        // (d) the truthful listing (matching the attestation) still lists, with the
        // real sub-gate lift disclosed.
        let mut honest = listing(true, 5_000, true, real_lift);
        honest.disclose_sub_gate = true;
        market
            .list(honest, &Gate::default(), &attestation)
            .expect("a listing matching the attestation lists");
    }

    // --- sale rules --------------------------------------------------------

    #[test]
    fn buy_transfers_provenance_and_certified_lift() {
        let mut market = Marketplace::new();
        let id = market
            .list(
                listing(true, 5_000, true, good_lift()),
                &Gate::default(),
                &attestation_for(good_lift(), good_measurement()),
            )
            .unwrap();
        let sale = market.buy(id, "0xbuyer").unwrap();
        assert_eq!(sale.buyer, "0xbuyer");
        assert_eq!(sale.seller, "0xseller");
        assert_eq!(sale.price_wei, 5_000);
        // Provenance + certified lift travel with the artifact.
        assert_eq!(sale.provenance, 7);
        assert_eq!(sale.certified_lift, good_lift());
        assert_eq!(market.sales().len(), 1);
        assert!(market.is_sold(id));
    }

    #[test]
    fn exclusive_listing_cannot_be_double_sold() {
        let mut market = Marketplace::new();
        let id = market
            .list(
                listing(true, 5_000, true, good_lift()),
                &Gate::default(),
                &attestation_for(good_lift(), good_measurement()),
            )
            .unwrap();
        market.buy(id, "0xfirst").expect("first buy succeeds");
        let err = market.buy(id, "0xsecond").unwrap_err();
        assert_eq!(err, MarketError::AlreadySold(id));
        // Only the first sale is recorded.
        assert_eq!(market.sales().len(), 1);
        assert_eq!(market.sales()[0].buyer, "0xfirst");
    }

    #[test]
    fn non_exclusive_listing_can_be_sold_repeatedly() {
        let mut market = Marketplace::new();
        let id = market
            .list(
                listing(true, 5_000, false, good_lift()),
                &Gate::default(),
                &attestation_for(good_lift(), good_measurement()),
            )
            .unwrap();
        market.buy(id, "0xa").unwrap();
        market.buy(id, "0xb").unwrap();
        assert_eq!(
            market.sales().len(),
            2,
            "a non-exclusive license sells again"
        );
    }

    #[test]
    fn buy_unknown_listing_is_rejected() {
        let mut market = Marketplace::new();
        assert_eq!(
            market.buy(0, "0xbuyer"),
            Err(MarketError::UnknownListing(0))
        );
    }

    // --- pricing -----------------------------------------------------------

    #[test]
    fn price_is_monotone_in_certified_lift() {
        let policy = PricingPolicy::new(1_000, 10);
        let deltas = [-0.5, 0.0, 0.05, 0.10, 0.20, 0.35, 0.50, 1.0];
        let prices: Vec<u128> = deltas
            .iter()
            .map(|&d| {
                price_by_lift(
                    &Lift {
                        delta: d,
                        ci_lower: d - 0.05,
                        ci_upper: d + 0.05,
                        n: 80,
                    },
                    &policy,
                )
            })
            .collect();
        // Monotone non-decreasing in the certified delta.
        for pair in prices.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "price must be non-decreasing in lift: {prices:?}"
            );
        }
        // A non-positive lift prices at exactly the floor.
        assert_eq!(prices[0], 1_000, "negative lift floors at base");
        assert_eq!(prices[1], 1_000, "zero lift floors at base");
        // A positive lift prices strictly above the floor.
        assert!(prices[2] > 1_000, "positive lift exceeds the floor");
        // 0.35 lift = 350_000 micros * 10 wei + 1_000 base.
        assert_eq!(prices[5], 350_000u128 * 10 + 1_000);
    }

    #[test]
    fn price_is_fail_closed_on_non_finite_lift() {
        let policy = PricingPolicy::new(1_000, 10);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let price = price_by_lift(
                &Lift {
                    delta: bad,
                    ci_lower: bad,
                    ci_upper: bad,
                    n: 80,
                },
                &policy,
            );
            assert_eq!(
                price, 1_000,
                "non-finite lift must floor at base, not overflow"
            );
        }
    }
}
