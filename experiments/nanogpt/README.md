# nanoGPT auto-research validation

A **real-world** validation of the autoresearch-competitions market: does it find and
pay for a genuine improvement to a real model, scored on a held-out test?

The vertical lives in `autoresearch-verticals/src/nanogpt.rs`; the competition is
`autoresearch-verticals/tests/e2e_nanogpt.rs`; the training loop is
`nanogpt_eval.py` (Karpathy's nanoGPT on char-level Shakespeare).

- **Surface** — nanoGPT hyper-/architecture-parameters (`learning_rate`, `n_layer`,
  `n_embd`, `dropout`, `weight_decay`, `warmup_iters`, …).
- **Scorer** (`NanoGptScorer`) — trains a candidate for a fixed compute budget over
  N seeds and returns the **held-out (`val.bin`) loss** with a CI. `value = −val_loss`,
  so the certified `lift` is the real reduction in val loss.
- **Engines** — five researchers each submit a real config hypothesis.
- **Market** — `run_oneshot_competitive` with the default gate
  (`min_lift_ci_lower = 0.02`, `min_n = 12`) and `SnapshotTopK` payout.

## Result (12 seeds, 300-iter budget, CPU)

```
baseline (lr=1e-3, 4L/128d): val_loss = 2.4197  (CI ±0.0064, n=12)

#1  scale-tune (lr=3e-3, 5L/192d, warmup=50): val_loss = 2.2478
    certified lift = 0.1719   (CI lower = 0.1565, n=12)   paid 500000
gated out (no certified lift): lr-tune, wide-highlr, too-low-lr, overshoot
pool 1000000 -> paid 500000 across 1 winner
```

The market found a **real, statistically certified improvement** — a config that lowers
held-out val loss from **2.420 → 2.248** (a **0.172-nat** reduction at the same compute,
CI lower bound 0.157, far above the 0.02 gate) — and paid only that winner.

Notably it **gated out `lr-tune`**, a config with a real but small (~0.05) point-estimate
gain whose lift was not statistically separated from noise at n=12. That is the
certified-causal-lift thesis working on real data: the market pays for *certified* lift,
not point estimates. The two clearly-bad hypotheses (`too-low-lr`, `overshoot`) and the
mis-sized one (`wide-highlr`) were likewise gated out.

This validates the **science** (auto-research improving a real model) in-process — no
testnet or operator sandbox required. The on-chain AVS lifecycle is validated separately
by `autoresearch-competitions-lib/tests/e2e_lifecycle.rs`.

## Reproduce

```bash
# one-time setup (CPU torch + data)
python3 -m venv ~/code/nanogpt-venv
~/code/nanogpt-venv/bin/pip install torch --index-url https://download.pytorch.org/whl/cpu
~/code/nanogpt-venv/bin/pip install numpy requests tqdm
git clone https://github.com/karpathy/nanoGPT.git ~/code/nanoGPT
~/code/nanogpt-venv/bin/python ~/code/nanoGPT/data/shakespeare_char/prepare.py

# run the competition (~10 min on CPU)
NANOGPT_PYTHON=~/code/nanogpt-venv/bin/python \
NANOGPT_WRAPPER="$PWD/experiments/nanogpt/nanogpt_eval.py" \
NANOGPT_THREADS=16 \
cargo test -p autoresearch-verticals --test e2e_nanogpt -- --ignored --nocapture
```
