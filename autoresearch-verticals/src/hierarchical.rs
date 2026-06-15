//! Hierarchical cross-instance training: **one model trained across `k`
//! independent m-of-n clusters** — DiLoCo across DiLoCo, "islands of islands".
//!
//! Phase 0 ([`crate::distributed_training`]) trains a recipe on a *single*
//! [`TrainingCluster`]. The next rung up is geographic/organizational: a model
//! trained simultaneously over several loosely-coupled clusters (each its own
//! operator set), with a slow **outer-outer** synchronization stitching the
//! per-cluster replicas together. This is exactly the regime Prime Intellect's
//! INTELLECT-class runs and Nous' Psyche live in — many sites, each internally a
//! DiLoCo island-set, coordinated by a higher-tier sync.
//!
//! [`HierarchicalCluster`] is itself a [`TrainingCluster`]: it composes `k` inner
//! clusters and exposes the same trait, so it drops into
//! [`DistributedTrainingEngine`](crate::DistributedTrainingEngine), the surface,
//! the scorer, and the orchestrator with **no change at the call site** — the
//! whole Phase-0 market mechanism scores a hierarchy exactly as it scores a single
//! cluster.
//!
//! # The two forces the hierarchy trades off
//!
//! 1. **Effective scale.** More clusters mean more independent replicas seeing
//!    more data in parallel. Like the island term inside one cluster, this is a
//!    log-diminishing win (`-k_bonus * ln(k)` on the loss proxy). A `k`-way
//!    hierarchy at the same recipe reaches a *lower* training loss than a single
//!    cluster — that is the whole reason to go hierarchical.
//! 2. **Cross-cluster drift.** The outer-outer sync only fires every
//!    [`cross_sync_interval`](HierarchicalCluster::cross_sync_interval) inner
//!    rounds. Push that interval too high and the per-cluster replicas drift apart
//!    between syncs; the merged model pays a coordination penalty that grows with
//!    both the interval and the number of replicas being reconciled. A
//!    well-coordinated hierarchy (tight interval) beats a poorly-coordinated one
//!    (loose interval) at the same `k` and recipe.
//!
//! Crucially the sync interval is a property of **how the hierarchy is operated**,
//! not of the researcher's [`TrainingRecipe`] — it lives on the composing cluster,
//! mirroring the real split between "what to train" (the recipe) and "how the
//! cluster-of-clusters is wired" (operator topology).
//!
//! # Honest seam — NOT a real multi-cluster run
//!
//! Like [`LocalSimCluster`](crate::LocalSimCluster), this composes the *dynamics*,
//! not real training: it dispatches the recipe to its inner clusters, reads their
//! self-reported [`TrainedArtifact::train_loss`], and folds them into one artifact
//! through the closed-form hierarchical model below. No GPUs, no clock, no I/O, no
//! `rand` — every output is byte-reproducible from the seed, so the tests assert
//! concrete inequalities. A production adapter would replace the inner clusters
//! with real `prime`/Psyche service instances and keep this composition intact.

use std::future::Future;

use autoresearch_runtime::traits::EngineError;

use crate::distributed_training::{TrainedArtifact, TrainingCluster, TrainingRecipe};

// --- Hierarchical-dynamics constants ----------------------------------------
//
// A closed-form model of how training the SAME recipe across `k` independent
// clusters converts the added cross-cluster parallelism into a lower loss, net of
// the coordination cost of an infrequent outer-outer sync. As in Phase 0 the goal
// is not physical fidelity but the right *shape*: a real scale win that
// log-saturates, and a drift penalty that punishes too-loose coordination — so
// "more clusters, tightly synced" wins and "more clusters, never synced" loses.

/// How fast more clusters reduce loss (log-diminishing, the cross-cluster analogue
/// of `ISLAND_GAIN`). One extra cluster helps; the tenth helps far less.
const CLUSTER_GAIN: f64 = 0.20;

/// Outer-outer sync interval at or below which cross-cluster drift is negligible.
/// Syncing at least this often keeps the replicas effectively coherent.
const SYNC_OPT: f64 = 4.0;

/// Penalty weight for drift accumulated between outer-outer syncs. Scaled by the
/// log-excess of the interval over its optimum so the bowl is one-sided: tighter
/// than optimal costs nothing, looser than optimal costs increasingly.
const DRIFT_PEN: f64 = 0.06;

/// How much the drift penalty grows with the number of replicas being reconciled:
/// more clusters means more pairwise divergence to merge across a loose sync.
const DRIFT_REPLICA_SCALE: f64 = 0.30;

/// Deterministic per-cluster seed decorrelation, so each inner cluster trains
/// under a distinct seed (independent replicas) while staying reproducible.
const CLUSTER_SEED_STRIDE: u64 = 0x9E37_79B9_7F4A_7C15;

/// `max(0, x)` — the positive part, for the one-sided drift penalty.
fn pos(x: f64) -> f64 {
    x.max(0.0)
}

// --- Hierarchical cluster ---------------------------------------------------

/// A cluster-of-clusters: trains one recipe across `k` inner [`TrainingCluster`]s
/// and folds their results into a single [`TrainedArtifact`]. Generic over the
/// inner cluster type so a hierarchy of [`LocalSimCluster`](crate::LocalSimCluster)s
/// (the in-repo stand-in) and a hierarchy of production `prime`/Psyche clusters are
/// the same code.
///
/// Because it *is* a [`TrainingCluster`], a `HierarchicalCluster` can itself be an
/// inner cluster of a deeper hierarchy — the composition nests.
#[derive(Clone, Debug)]
pub struct HierarchicalCluster<C> {
    id: String,
    clusters: Vec<C>,
    /// Inner rounds between outer-outer (cross-cluster) syncs. Lower = tighter
    /// coordination = less drift; this is the operator knob, NOT a recipe field.
    pub cross_sync_interval: u32,
}

impl<C> HierarchicalCluster<C> {
    /// Compose `clusters` under one outer-outer sync cadence. `id` names the
    /// hierarchy for provenance; `cross_sync_interval` is how many inner rounds
    /// elapse between cross-cluster syncs (lower is tighter coordination).
    ///
    /// # Panics
    ///
    /// Panics if `clusters` is empty — a hierarchy needs at least one inner
    /// cluster to train. The surface/orchestrator never construct one this way;
    /// the guard makes the misuse loud rather than silently degenerate.
    #[must_use]
    pub fn new(id: impl Into<String>, clusters: Vec<C>, cross_sync_interval: u32) -> Self {
        assert!(
            !clusters.is_empty(),
            "HierarchicalCluster needs at least one inner cluster"
        );
        Self {
            id: id.into(),
            clusters,
            cross_sync_interval,
        }
    }

    /// Number of inner clusters `k` in the hierarchy.
    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }

    /// The deterministic seed handed to inner cluster `index`, decorrelated from
    /// the base seed so each cluster is an independent replica yet reproducible.
    fn cluster_seed(base_seed: u64, index: usize) -> u64 {
        base_seed.wrapping_add((index as u64).wrapping_mul(CLUSTER_SEED_STRIDE))
    }

    /// Fold the inner clusters' training losses into the hierarchy's loss proxy.
    ///
    /// The merged loss is the mean of the per-cluster losses (the outer-outer sync
    /// averages the replicas) minus a **scale bonus** that improves with more
    /// clusters, plus a **drift penalty** that grows when the sync interval is too
    /// large for the number of replicas. Lower is better, matching
    /// [`TrainedArtifact::train_loss`].
    fn merge_loss(&self, inner_losses: &[f64]) -> f64 {
        debug_assert!(!inner_losses.is_empty());
        let k = inner_losses.len() as f64;

        // The outer-outer sync averages the per-cluster replicas.
        let mean_loss = inner_losses.iter().sum::<f64>() / k;

        // More clusters = more effective scale, log-diminishing (so k=1 adds
        // nothing and each further cluster helps a little less than the last).
        let scale_bonus = CLUSTER_GAIN * k.ln();

        // Replicas drift between outer-outer syncs; an interval above its optimum
        // costs increasingly, and more replicas means more divergence to merge.
        let interval = f64::from(self.cross_sync_interval);
        let drift_excess = pos(interval.ln() - SYNC_OPT.ln());
        let drift_pen = DRIFT_PEN * drift_excess * (1.0 + DRIFT_REPLICA_SCALE * (k - 1.0));

        mean_loss - scale_bonus + drift_pen
    }
}

impl<C> TrainingCluster for HierarchicalCluster<C>
where
    C: TrainingCluster + Sync,
{
    fn id(&self) -> &str {
        &self.id
    }

    // The trait requires `-> impl Future + Send`; the `async fn` rewrite that
    // `manual_async_fn` suggests cannot express that explicit `Send` bound, so the
    // manual `impl Future + Send { async move }` form is required, not stylistic.
    #[allow(clippy::manual_async_fn)]
    fn train(
        &self,
        recipe: &TrainingRecipe,
        seed: u64,
    ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
        async move {
            // Dispatch the SAME recipe to each inner cluster under a distinct
            // per-cluster seed (independent replicas), then read their
            // self-reported training loss. As in Phase 0 the hierarchy trusts the
            // inner losses only as a dev signal; the Referee still re-scores the
            // merged artifact on held-out.
            let mut inner_losses = Vec::with_capacity(self.clusters.len());
            for (index, cluster) in self.clusters.iter().enumerate() {
                let cluster_seed = Self::cluster_seed(seed, index);
                let artifact = cluster.train(recipe, cluster_seed).await?;
                inner_losses.push(artifact.train_loss);
            }

            Ok(TrainedArtifact {
                recipe: *recipe,
                train_seed: seed,
                train_loss: self.merge_loss(&inner_losses),
            })
        }
    }

    /// A hierarchy is sealed iff **every** inner cluster is sealed — one
    /// non-isolated cluster leaks the private competition's data, so the binding
    /// is all-or-nothing. (An empty hierarchy cannot exist; see [`Self::new`].)
    fn provides_sealed_isolation(&self) -> bool {
        self.clusters
            .iter()
            .all(TrainingCluster::provides_sealed_isolation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distributed_training::LocalSimCluster;

    /// A minimal test-double that trains via the local sim but reports a
    /// *configurable* sealed-isolation flag — so the all-or-nothing binding can be
    /// exercised across a heterogeneous (mixed-sealed) hierarchy with one type.
    /// (`TrainingCluster::train` returns `impl Future`, so it is not
    /// dyn-compatible; a single configurable type is how a mixed hierarchy is
    /// built without trait objects.)
    #[derive(Clone, Copy, Debug)]
    struct FlaggedCluster {
        sealed: bool,
    }
    impl TrainingCluster for FlaggedCluster {
        fn id(&self) -> &str {
            "flagged-test-cluster"
        }
        fn train(
            &self,
            recipe: &TrainingRecipe,
            seed: u64,
        ) -> impl Future<Output = Result<TrainedArtifact, EngineError>> + Send {
            std::future::ready(Ok(LocalSimCluster.train_sync(recipe, seed)))
        }
        fn provides_sealed_isolation(&self) -> bool {
            self.sealed
        }
    }

    /// Drive a `train` future to completion without an executor (the bodies are
    /// synchronous: `ready` leaves and `LocalSimCluster` resolve immediately).
    fn block_on<F: Future>(future: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(value) => return value,
                Poll::Pending => continue,
            }
        }
    }

    fn tuned_recipe() -> TrainingRecipe {
        // A reasonable tuned recipe (not the degenerate baseline) so the inner
        // clusters have real, well-separated losses to fold.
        TrainingRecipe {
            islands: 8,
            inner_steps: 32,
            inner_lr: 3e-3,
            outer_lr: 0.7,
            keep_fraction: 0.2,
        }
    }

    fn hierarchy(k: usize, sync: u32) -> HierarchicalCluster<LocalSimCluster> {
        HierarchicalCluster::new("hier", vec![LocalSimCluster; k], sync)
    }

    #[test]
    fn hierarchy_beats_single_cluster_on_train_loss() {
        // (a) A well-coordinated k>1 hierarchy reaches a lower training loss than a
        // single cluster on the SAME recipe — more effective cross-cluster scale.
        let recipe = tuned_recipe();
        let seed = 42;

        let single = LocalSimCluster.train_sync(&recipe, seed).train_loss;
        let hier = block_on(hierarchy(8, SYNC_OPT as u32).train(&recipe, seed))
            .unwrap()
            .train_loss;

        assert!(
            hier < single,
            "k>1 hierarchy should beat a single cluster on train_loss: single={single} hier={hier}"
        );
    }

    #[test]
    fn scale_bonus_grows_with_more_clusters() {
        // The scale win is monotone in k (log-diminishing but always positive),
        // holding coordination tight so drift doesn't confound it.
        let recipe = tuned_recipe();
        let seed = 7;
        let sync = SYNC_OPT as u32;

        let two = block_on(hierarchy(2, sync).train(&recipe, seed))
            .unwrap()
            .train_loss;
        let eight = block_on(hierarchy(8, sync).train(&recipe, seed))
            .unwrap()
            .train_loss;

        assert!(
            eight < two,
            "more clusters (tightly synced) should lower train_loss: k2={two} k8={eight}"
        );
    }

    #[test]
    fn loose_coordination_loses_to_tight_coordination() {
        // (b) Same k and recipe: a hierarchy whose outer-outer sync is too
        // infrequent (replicas drift) is worse than a tightly-synced one.
        let recipe = tuned_recipe();
        let seed = 99;
        let k = 8;

        let tight = block_on(hierarchy(k, SYNC_OPT as u32).train(&recipe, seed))
            .unwrap()
            .train_loss;
        let loose = block_on(hierarchy(k, 4096).train(&recipe, seed))
            .unwrap()
            .train_loss;

        assert!(
            loose > tight,
            "too-large cross_sync_interval (drift) must hurt: tight={tight} loose={loose}"
        );
    }

    #[test]
    fn drift_can_erase_the_scale_win_entirely() {
        // Pushing the sync interval far enough makes a poorly-coordinated hierarchy
        // worse than even a single cluster — coordination is load-bearing, not free.
        let recipe = tuned_recipe();
        let seed = 5;

        let single = LocalSimCluster.train_sync(&recipe, seed).train_loss;
        let badly_coordinated = block_on(hierarchy(8, 1_000_000).train(&recipe, seed))
            .unwrap()
            .train_loss;

        assert!(
            badly_coordinated > single,
            "extreme drift should erase the scale win: single={single} bad={badly_coordinated}"
        );
    }

    #[test]
    fn sealed_iff_all_inner_clusters_sealed() {
        // (c) The TEE binding is all-or-nothing across the hierarchy.
        let all_sealed = HierarchicalCluster::new("s", vec![FlaggedCluster { sealed: true }; 3], 4);
        assert!(
            all_sealed.provides_sealed_isolation(),
            "a hierarchy of only sealed clusters must report sealed isolation"
        );

        let none_sealed = HierarchicalCluster::new("u", vec![LocalSimCluster; 3], 4);
        assert!(
            !none_sealed.provides_sealed_isolation(),
            "a hierarchy of non-sealed clusters must NOT report sealed isolation"
        );

        // One non-sealed cluster taints the whole hierarchy. Same inner type with
        // a per-cluster flag, since `train`'s `impl Future` return is not
        // dyn-compatible (no trait objects).
        let mixed = HierarchicalCluster::new(
            "m",
            vec![
                FlaggedCluster { sealed: true },
                FlaggedCluster { sealed: false },
            ],
            4,
        );
        assert!(
            !mixed.provides_sealed_isolation(),
            "one non-sealed inner cluster must taint the whole hierarchy"
        );
    }

    #[test]
    fn train_is_deterministic_per_seed() {
        // No rand, no clock: the same seed yields byte-identical merged loss.
        let recipe = tuned_recipe();
        let a = block_on(hierarchy(4, 4).train(&recipe, 11))
            .unwrap()
            .train_loss;
        let b = block_on(hierarchy(4, 4).train(&recipe, 11))
            .unwrap()
            .train_loss;
        assert_eq!(a, b, "hierarchical training must be deterministic per seed");
    }

    #[test]
    fn distinct_per_cluster_seeds_decorrelate_replicas() {
        // Each inner cluster trains under a distinct seed, so a k>1 hierarchy is
        // not just k identical replicas (which would defeat the parallelism).
        let s0 = HierarchicalCluster::<LocalSimCluster>::cluster_seed(100, 0);
        let s1 = HierarchicalCluster::<LocalSimCluster>::cluster_seed(100, 1);
        assert_ne!(s0, s1, "per-cluster seeds must differ across replicas");
    }

    #[test]
    fn single_cluster_hierarchy_matches_its_inner_cluster() {
        // k=1: ln(1)=0 scale bonus and (with a tight sync) no drift, so a
        // one-cluster hierarchy reproduces its inner cluster's loss exactly — the
        // composition degrades gracefully to the Phase-0 single-cluster case.
        let recipe = tuned_recipe();
        let seed = 3;
        let inner = LocalSimCluster.train_sync(&recipe, seed).train_loss;
        let hier = block_on(hierarchy(1, SYNC_OPT as u32).train(&recipe, seed))
            .unwrap()
            .train_loss;
        assert_eq!(
            inner, hier,
            "k=1 hierarchy with tight sync must equal its single inner cluster"
        );
    }
}
