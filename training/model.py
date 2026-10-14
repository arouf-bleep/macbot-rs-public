"""
MultiHeadBTCPredictor — 3-branch neural network with FiLM-conditioned
attention fusion and a binary prediction head.

Branch 1: TemporalAttention (self-attention over candle sequences with
           learnable positional encodings at 3 resolutions)
Branch 2: TransformerEncoder (orderbook microstructure dynamics)
Branch 3: ScalarMLP (macro signals + context)

Supervised outputs:
  - Primary: p(UP) via Sigmoid
  - Calibration: maps primary logit to calibrated probability
  - Three zero ABI placeholders; forward auxiliary targets are unavailable

Market context embeddings:
  - Time-remaining (15 bins, 8-dim)
  - Regime (4 categories, 8-dim)

~200K parameters. Sub-millisecond CPU inference.
"""

import torch
import torch.nn as nn
import torch.nn.functional as F
import math

from regime_contract import (
    NUM_REGIME_BUCKETS,
    REGIME_HIGH_VOL_MIN,
    REGIME_MEAN_REVERT_MAX,
    REGIME_TREND_MIN,
)

NUM_FUSION_FEATURES = 34
NUM_CONTEXT_FEATURES = 6
NUM_SCALAR_FEATURES = NUM_FUSION_FEATURES + NUM_CONTEXT_FEATURES
NUM_STALENESS_FEATURES = NUM_FUSION_FEATURES
SCALAR_MLP_INPUT_DIM = NUM_SCALAR_FEATURES + NUM_STALENESS_FEATURES
TIME_BUCKETS = 15


# ── Branch 1: Temporal Attention (#6) ──────────────────────────────────────

class TemporalSelfAttention(nn.Module):
    """Single-head self-attention over a candle sequence with learnable
    positional encoding.  32-dim keys/queries/values.

    Input:  [B, seq_len, input_dim]  (candle features per timestep)
    Output: [B, 32]  (attended representation pooled to fixed-size vector)
    """

    def __init__(self, input_dim: int = 5, seq_len: int = 15, d_attn: int = 32):
        super().__init__()
        self.d_attn = d_attn

        # Project raw candle features to d_attn space
        self.input_proj = nn.Linear(input_dim, d_attn)

        # Learnable positional encoding so the model knows recency
        self.pos_enc = nn.Parameter(torch.randn(1, seq_len, d_attn) * 0.02)

        # Q / K / V projections (single head, 32-dim)
        self.W_q = nn.Linear(d_attn, d_attn)
        self.W_k = nn.Linear(d_attn, d_attn)
        self.W_v = nn.Linear(d_attn, d_attn)

        self.scale = math.sqrt(d_attn)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        """x: [B, seq_len, input_dim] → [B, 32]"""
        x = self.input_proj(x) + self.pos_enc          # [B, T, 32]
        Q = self.W_q(x)                                 # [B, T, 32]
        K = self.W_k(x)                                 # [B, T, 32]
        V = self.W_v(x)                                 # [B, T, 32]

        attn = torch.matmul(Q, K.transpose(-1, -2)) / self.scale  # [B, T, T]
        attn = F.softmax(attn, dim=-1)
        out = torch.matmul(attn, V)                     # [B, T, 32]

        # Pool over time dimension → fixed-size vector
        return out.mean(dim=1)                           # [B, 32]


class TemporalAttentionBranch(nn.Module):
    """Multi-resolution temporal self-attention on 1m, 5m, 15m candles.

    Replaces the old TemporalCNNBranch with lightweight self-attention
    modules that learn which candles in each sequence matter most.
    """

    def __init__(self, out_dim: int = 64):
        super().__init__()
        # Self-attention per resolution
        self.attn_1m  = TemporalSelfAttention(input_dim=5, seq_len=60, d_attn=32)
        self.attn_5m  = TemporalSelfAttention(input_dim=5, seq_len=24, d_attn=32)
        self.attn_15m = TemporalSelfAttention(input_dim=5, seq_len=16, d_attn=32)

        # Concat 96d → out_dim
        self.proj = nn.Linear(96, out_dim)

    def forward(self, candles_1m, candles_5m, candles_15m):
        """
        candles_1m:  [B, 60, 5]
        candles_5m:  [B, 24, 5]
        candles_15m: [B, 16, 5]
        """
        x1  = self.attn_1m(candles_1m)    # [B, 32]
        x5  = self.attn_5m(candles_5m)    # [B, 32]
        x15 = self.attn_15m(candles_15m)  # [B, 32]

        cat = torch.cat([x1, x5, x15], dim=-1)  # [B, 96]
        return self.proj(cat)                     # [B, out_dim]


# Backward-compatible alias so any code importing TemporalCNNBranch still works
TemporalCNNBranch = TemporalAttentionBranch


# ── Branch 2: TransformerEncoder ────────────────────────────────────────────

class OrderbookTransformerBranch(nn.Module):
    """Transformer on L2 depth + order flow + liquidation sequences."""

    def __init__(self, input_dim: int = 45, out_dim: int = 64,
                 n_heads: int = 4, n_layers: int = 2, seq_len: int = 12):
        super().__init__()
        d_model = 64
        self.input_proj = nn.Linear(input_dim, d_model)
        self.pos_emb = nn.Parameter(torch.randn(1, seq_len, d_model) * 0.02)

        encoder_layer = nn.TransformerEncoderLayer(
            d_model=d_model, nhead=n_heads, dim_feedforward=128,
            dropout=0.1, activation="gelu", batch_first=True,
        )
        self.transformer = nn.TransformerEncoder(encoder_layer, num_layers=n_layers)
        self.proj = nn.Linear(d_model, out_dim)

    def forward(self, orderbook_seq):
        """orderbook_seq: [B, 12, 45]"""
        x = self.input_proj(orderbook_seq) + self.pos_emb  # [B, 12, 64]
        x = self.transformer(x)  # [B, 12, 64]
        x = x.mean(dim=1)  # [B, 64] — average pool over sequence
        return self.proj(x)  # [B, out_dim]


# ── Branch 3: ScalarMLP ────────────────────────────────────────────────────

class ScalarMLPBranch(nn.Module):
    """MLP on fusion features + context vars + staleness features."""

    def __init__(self, input_dim: int = SCALAR_MLP_INPUT_DIM, out_dim: int = 32):
        super().__init__()
        self.net = nn.Sequential(
            nn.Linear(input_dim, 96),
            nn.LayerNorm(96),
            nn.GELU(),
            nn.Dropout(0.1),
            nn.Linear(96, out_dim),
            nn.LayerNorm(out_dim),
            nn.GELU(),
        )

    def forward(self, scalars, staleness):
        """
        scalars:   [B, NUM_SCALAR_FEATURES]
        staleness: [B, NUM_STALENESS_FEATURES]
        """
        x = torch.cat([scalars, staleness], dim=-1)  # [B, SCALAR_MLP_INPUT_DIM]
        return self.net(x)  # [B, out_dim]


# ── FiLM Conditioning Layer ─────────────────────────────────────────────────

class FiLMLayer(nn.Module):
    """Feature-wise Linear Modulation — regime conditions the fusion."""

    def __init__(self, n_regimes: int = NUM_REGIME_BUCKETS, feature_dim: int = 160):
        super().__init__()
        self.gamma = nn.Embedding(n_regimes, feature_dim)
        self.beta = nn.Embedding(n_regimes, feature_dim)
        nn.init.ones_(self.gamma.weight)
        nn.init.zeros_(self.beta.weight)

    def forward(self, x, regime_idx):
        """
        x: [B, feature_dim]
        regime_idx: [B] int tensor
          0 = mean-revert/compression bucket
          1 = directional trend bucket
          2 = high-volatility bucket
          3 = fallback/other bucket
        """
        gamma = self.gamma(regime_idx)  # [B, feature_dim]
        beta = self.beta(regime_idx)    # [B, feature_dim]
        return gamma * x + beta


# ── Fusion Head ─────────────────────────────────────────────────────────────

class FusionHead(nn.Module):
    """Self-attention over branch outputs with FiLM conditioning."""

    def __init__(self, branch_dims=(64, 64, 32), out_dim: int = 64):
        super().__init__()
        total = sum(branch_dims)  # 160
        self.film = FiLMLayer(n_regimes=NUM_REGIME_BUCKETS, feature_dim=total)
        self.attn = nn.MultiheadAttention(
            embed_dim=total, num_heads=2, batch_first=True, dropout=0.1,
        )
        self.proj = nn.Sequential(
            nn.Linear(total, out_dim),
            nn.GELU(),
        )

    def forward(self, branch_outputs, regime_idx):
        """
        branch_outputs: list of [B, d_i] tensors
        regime_idx: [B] int tensor
        """
        x = torch.cat(branch_outputs, dim=-1)  # [B, 160]
        x = self.film(x, regime_idx)  # [B, 160]
        # Self-attention: treat as single-token sequence
        x = x.unsqueeze(1)  # [B, 1, 160]
        x, _ = self.attn(x, x, x)  # [B, 1, 160]
        x = x.squeeze(1)  # [B, 160]
        return self.proj(x)  # [B, 64]


# ── Task Heads ──────────────────────────────────────────────────────────────

class MultiHeadBTCPredictor(nn.Module):
    """
    Full model: 3 branches → FiLM fusion → 5 task heads + calibration.

    Includes:
      - #6  Temporal self-attention (replaces CNN)
      - #7  Market context embeddings (time-remaining + regime)
      - #9  Binary outcome head with three compatibility placeholders
      - #10 Calibration head (logit → calibrated probability via temperature scaling)

    When disable_cnn_tf=True, temporal-attention and Transformer outputs are
    zeroed so the model learns purely from scalar features (MLP branch).
    """

    def __init__(self, disable_cnn_tf: bool = False):
        super().__init__()
        self.disable_cnn_tf = disable_cnn_tf
        self.cnn = TemporalAttentionBranch(out_dim=64)
        self.transformer = OrderbookTransformerBranch(out_dim=64)
        self.mlp = ScalarMLPBranch(input_dim=SCALAR_MLP_INPUT_DIM, out_dim=32)
        self.fusion = FusionHead(branch_dims=(64, 64, 32), out_dim=64)

        # ── #7 Market Context Embeddings ──────────────────────────────────
        # Time remaining: 15 bins (0-1 min, 1-2 min, ..., 14-15 min), 8-dim
        self.time_embedding = nn.Embedding(TIME_BUCKETS, 8)
        # Keep the current 4-bucket regime contract explicit until the next
        # deep-model retrain expands the regime input semantics end to end.
        self.regime_embedding = nn.Embedding(NUM_REGIME_BUCKETS, 8)

        # Fused dim = 64 (fusion) + 8 (time emb) + 8 (regime emb) = 80
        fused_dim = 80

        # ── #9 Multi-Task Heads ───────────────────────────────────────────
        self.head_p_up = nn.Linear(fused_dim, 1)       # Primary: p(UP)
        # No forward auxiliary targets are recorded. Preserve the four-output
        # inference ABI with zero placeholders, not randomly trained forecasts.

        # ── #10 Calibration Head ──────────────────────────────────────────
        # Takes the main head's raw logit (1-dim) → calibrated probability.
        # Temperature-scaling approach: learns to map logits to well-calibrated probs.
        self.calibration_head = nn.Sequential(
            nn.Linear(1, 16),
            nn.ReLU(),
            nn.Linear(16, 1),
            nn.Sigmoid(),
        )

    def _discretize_time_remaining(self, scalars: torch.Tensor) -> torch.Tensor:
        """Extract time_remaining from scalars and discretize into 15 bins.

        scalars[:, NUM_FUSION_FEATURES] is the fraction of the window remaining
        (the first context value after the fusion feature block).
        15 bins: 0-1 min → 0, 1-2 min → 1, ..., 14-15 min → 14.
        """
        time_remaining = scalars[:, NUM_FUSION_FEATURES]  # [B]
        buckets = (time_remaining.clamp(0.0, 1.0) * TIME_BUCKETS).long().clamp(0, TIME_BUCKETS - 1)
        return buckets

    def _bucketize_regime(self, regime: torch.Tensor) -> torch.Tensor:
        """Preserve the current Rust→ONNX regime contract explicitly.

        Runtime feeds ordinal floats from Rust input_builder.rs. Collapse those
        values into the current 4-bucket embedding contract:
          0 => bearish/mean-revert/compression (-1.0, 0.0, 0.1, 0.3)
          1 => directional trend (1.0, 1.5)
          2 => high volatility (2.0)
          3 => fallback / unknown (0.5 default)
        """
        regime = regime.squeeze(-1)
        bucket = torch.full_like(regime.long(), 3)
        bucket = torch.where(
            regime <= REGIME_MEAN_REVERT_MAX,
            torch.zeros_like(bucket),
            bucket,
        )
        bucket = torch.where(
            (regime >= REGIME_TREND_MIN) & (regime < REGIME_HIGH_VOL_MIN),
            torch.ones_like(bucket),
            bucket,
        )
        bucket = torch.where(
            regime >= REGIME_HIGH_VOL_MIN,
            torch.full_like(bucket, 2),
            bucket,
        )
        return bucket.clamp(0, NUM_REGIME_BUCKETS - 1)

    def forward(self, candles_1m, candles_5m, candles_15m,
                orderbook_seq, scalars, staleness, regime):
        """
        candles_1m:    [B, 60, 5]
        candles_5m:    [B, 24, 5]
        candles_15m:   [B, 16, 5]
        orderbook_seq: [B, 12, 45]
        scalars:       [B, NUM_SCALAR_FEATURES]
        staleness:     [B, NUM_STALENESS_FEATURES]
        regime:        [B, 1]  float → quantized to int
        """
        regime_idx = self._bucketize_regime(regime)  # [B]

        # Branch forward passes
        if self.disable_cnn_tf:
            batch_size = scalars.size(0)
            cnn_out = torch.zeros(batch_size, 64, device=scalars.device)
            tf_out = torch.zeros(batch_size, 64, device=scalars.device)
        else:
            cnn_out = self.cnn(candles_1m, candles_5m, candles_15m)  # [B, 64]
            tf_out = self.transformer(orderbook_seq)  # [B, 64]
        mlp_out = self.mlp(scalars, staleness)  # [B, 32]

        # Fusion
        fused_base = self.fusion([cnn_out, tf_out, mlp_out], regime_idx)  # [B, 64]

        # ── #7 Market Context Embeddings ──────────────────────────────────
        time_buckets = self._discretize_time_remaining(scalars)  # [B]
        time_emb = self.time_embedding(time_buckets)              # [B, 8]
        regime_emb = self.regime_embedding(regime_idx)             # [B, 8]
        fused = torch.cat([fused_base, time_emb, regime_emb], dim=-1)  # [B, 80]

        # ── #9 Task heads ─────────────────────────────────────────────────
        logit_p_up = self.head_p_up(fused)                   # [B, 1] raw logit
        p_up = torch.sigmoid(logit_p_up)                     # [B, 1]
        magnitude = torch.zeros_like(p_up)
        volatility = torch.zeros_like(p_up)
        time_to_move = torch.zeros_like(p_up)

        # ── #10 Calibration head: logit → calibrated probability ──────────
        calibration = self.calibration_head(logit_p_up.detach())  # [B, 1]

        return p_up, magnitude, volatility, time_to_move, calibration

    def count_parameters(self):
        return sum(p.numel() for p in self.parameters() if p.requires_grad)


# ── ONNX Export ─────────────────────────────────────────────────────────────

def export_onnx(model: MultiHeadBTCPredictor, path: str):
    """Export trained model to ONNX format for Rust inference.

    Preserves disable_cnn_tf flag — if the model was trained with CNN/TF
    disabled (zeroed), the ONNX export must match so inference uses
    the same architecture that was trained.
    """
    # KEEP disable_cnn_tf as-is — exporting must match training config
    model.cpu()
    model.eval()
    batch = 1
    dummy = (
        torch.randn(batch, 60, 5),   # candles_1m
        torch.randn(batch, 24, 5),   # candles_5m
        torch.randn(batch, 16, 5),   # candles_15m
        torch.randn(batch, 12, 45),  # orderbook_seq
        torch.randn(batch, NUM_SCALAR_FEATURES),      # scalars
        torch.randn(batch, NUM_STALENESS_FEATURES),   # staleness
        torch.zeros(batch, 1),       # regime
    )
    torch.onnx.export(
        model, dummy, path,
        # Keep the established opset-17, single-file export contract. Recent
        # Torch defaults select a different exporter and external weight files;
        # publishing/copying latest.onnx must not depend on hidden sidecars.
        dynamo=False,
        input_names=[
            "candles_1m", "candles_5m", "candles_15m",
            "orderbook_seq", "scalars", "staleness", "regime",
        ],
        output_names=["p_up", "magnitude", "volatility", "time_to_move", "calibration"],
        dynamic_axes={name: {0: "batch"} for name in [
            "candles_1m", "candles_5m", "candles_15m",
            "orderbook_seq", "scalars", "staleness", "regime",
            "p_up", "magnitude", "volatility", "time_to_move", "calibration",
        ]},
        opset_version=17,
    )
    from model_artifact import validate_model_artifact
    validate_model_artifact(path)
    print(f"Exported ONNX model to {path}")


if __name__ == "__main__":
    model = MultiHeadBTCPredictor()
    print(f"Parameters: {model.count_parameters():,}")

    # Quick forward pass sanity check
    out = model(
        torch.randn(2, 60, 5),
        torch.randn(2, 24, 5),
        torch.randn(2, 16, 5),
        torch.randn(2, 12, 45),
        torch.randn(2, NUM_SCALAR_FEATURES),
        torch.randn(2, NUM_STALENESS_FEATURES),
        torch.zeros(2, 1),
    )
    print(f"Output shapes: {[o.shape for o in out]}")
    export_onnx(model, "test_model.onnx")
