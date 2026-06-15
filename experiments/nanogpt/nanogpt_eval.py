#!/usr/bin/env python3
"""Train a char-level nanoGPT for a fixed compute budget under a given config and
report the best held-out (val) loss as JSON.

This is the *real training loop* the autoresearch-competitions `NanoGptScorer`
shells out to: the proposer's "held-out test" is val.bin; a candidate config that
reaches a lower val loss at the same budget is a genuine improvement. Char-level
Shakespeare (vocab 65) keeps it CPU-tractable while remaining a real GPT.

Usage:  python nanogpt_eval.py '<config-json>'
Output (stdout, last line): {"val_loss": <best>, "final_val_loss": ..., "iters": N,
                             "seconds": T, "params": P}

The config is the tunable Surface: learning_rate, n_layer, n_head, n_embd, dropout,
weight_decay, warmup_iters, block_size, batch_size, max_iters, eval_interval, seed.
Everything not given falls back to a small baseline.
"""

import json
import math
import os
import pickle
import sys
import time

import numpy as np
import torch

NANOGPT = os.environ.get("NANOGPT_DIR", os.path.expanduser("~/code/nanoGPT"))
sys.path.insert(0, NANOGPT)
from model import GPT, GPTConfig  # noqa: E402  (nanoGPT's model)

DATA_DIR = os.path.join(NANOGPT, "data", "shakespeare_char")


def main() -> None:
    cfg = json.loads(sys.argv[1]) if len(sys.argv) > 1 else {}
    seed = int(cfg.get("seed", 1337))
    torch.manual_seed(seed)
    np.random.seed(seed)
    torch.set_num_threads(int(os.environ.get("NANOGPT_THREADS", "8")))
    device = "cpu"

    block_size = int(cfg.get("block_size", 64))
    batch_size = int(cfg.get("batch_size", 12))
    max_iters = int(cfg.get("max_iters", 300))
    warmup = int(cfg.get("warmup_iters", 20))
    eval_interval = int(cfg.get("eval_interval", 100))
    eval_iters = int(cfg.get("eval_iters", 40))
    lr = float(cfg.get("learning_rate", 1e-3))
    min_lr = lr * 0.1
    wd = float(cfg.get("weight_decay", 0.1))

    train_data = np.memmap(os.path.join(DATA_DIR, "train.bin"), dtype=np.uint16, mode="r")
    val_data = np.memmap(os.path.join(DATA_DIR, "val.bin"), dtype=np.uint16, mode="r")
    with open(os.path.join(DATA_DIR, "meta.pkl"), "rb") as f:
        vocab_size = pickle.load(f)["vocab_size"]

    def get_batch(split):
        data = train_data if split == "train" else val_data
        ix = torch.randint(len(data) - block_size, (batch_size,))
        x = torch.stack([torch.from_numpy(data[i : i + block_size].astype(np.int64)) for i in ix])
        y = torch.stack([torch.from_numpy(data[i + 1 : i + 1 + block_size].astype(np.int64)) for i in ix])
        return x.to(device), y.to(device)

    conf = GPTConfig(
        n_layer=int(cfg.get("n_layer", 4)),
        n_head=int(cfg.get("n_head", 4)),
        n_embd=int(cfg.get("n_embd", 128)),
        block_size=block_size,
        bias=False,
        vocab_size=vocab_size,
        dropout=float(cfg.get("dropout", 0.0)),
    )
    model = GPT(conf).to(device)
    optimizer = model.configure_optimizers(wd, lr, (0.9, 0.99), "cpu")

    def lr_at(it):
        if it < warmup:
            return lr * (it + 1) / (warmup + 1)
        if it > max_iters:
            return min_lr
        ratio = (it - warmup) / max(1, max_iters - warmup)
        return min_lr + 0.5 * (1.0 + math.cos(math.pi * ratio)) * (lr - min_lr)

    @torch.no_grad()
    def est_val():
        model.eval()
        losses = [model(*get_batch("val"))[1].item() for _ in range(eval_iters)]
        model.train()
        return sum(losses) / len(losses)

    t0 = time.time()
    best = float("inf")
    vl = float("inf")
    for it in range(max_iters + 1):
        for g in optimizer.param_groups:
            g["lr"] = lr_at(it)
        if it % eval_interval == 0 or it == max_iters:
            vl = est_val()
            best = min(best, vl)
        if it == max_iters:
            break
        x, y = get_batch("train")
        _, loss = model(x, y)
        optimizer.zero_grad(set_to_none=True)
        loss.backward()
        optimizer.step()

    print(
        json.dumps(
            {
                "val_loss": round(best, 4),
                "final_val_loss": round(vl, 4),
                "iters": max_iters,
                "seconds": round(time.time() - t0, 2),
                "params": model.get_num_params(),
            }
        )
    )


if __name__ == "__main__":
    main()
