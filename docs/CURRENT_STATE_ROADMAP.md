# Macbot maintenance roadmap

Macbot combines research tools with an execution engine, durable accounting and an operator dashboard. The current implementation and boundaries are described in [ARCHITECTURE.md](ARCHITECTURE.md) and [KNOWN_LIMITATIONS.md](KNOWN_LIMITATIONS.md).

## Maintain execution and recovery

Keep order admission, cancellation, confirmed fills, risk controls and restart recovery inside the execution actor. Retain failure-case regressions when changing these paths. Validate exchange contracts and supported receipt/accounting rules before extending venue coverage.

## Improve research evidence

Preserve chronological evaluation and independent observation windows. Historical synthetic-price backtests do not reconstruct venue execution. Further evaluation needs observed prices, documented costs and realistic fill assumptions; the bundled model remains an untrained runtime fixture.

## Reduce coordination complexity

The actor and analysis coordinators remain large. Extract focused components when behavior changes justify it, preserving the existing state ownership and financial invariants. Consolidate overlapping research reports around shared inputs and clearly scoped results.

## Recheck supported environments

Run the full source verifier on each target platform and maintain checksum-pinned native dependencies. Existing host observations do not establish acceptance of every operating system, architecture or future dependency version.
