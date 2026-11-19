# API changes in datumesh-engine 0.5.0

Breaking: `EngineError::Cycle` and `EvalError::Cycle` carry `path: Vec<String>` (the canonical
calls of the cycle, read order) in place of `cell_id`. Additions: `Engine::whatif`, `Overrides`,
`Solve`, `ScenarioAnswer`, `ScenarioOverride`, `EngineError::{Override, NotCovered, WhatifNoSolution}`.
`EngineError::DepthExceeded` remains but is no longer raised by evaluation (E4). The full
public API is in `public-api.txt` and `public-api-all-features.txt`. See the CHANGELOG.
