# Minimal Nimbus V1 additions

Baseline: official v1.20000.1 (db918f6), DuckDB 2.0.0 / d54cbbe3df.

Keep safe native frontend/transaction state, single-statement preparation and fallible metadata, detached completed results and value decoding, Arrow UDF volatility/NULL/batch semantics, and transaction-local logical direct appender with construction snapshot/type checks.

Official v1.20000.1 owns streaming errors (#664), pre-epoch chrono conversion (#926), extension loading/version detection (#918), cc-backend loader reachability (#923), and bundled-cc source caching (#924). No private replacements for these fixes are added. Neo additions from the previous fork are not carried to this branch.

The Linux bundled-cmake archive-order fix remains necessary: #923's loader anchor is explicitly disabled with bundled-cmake, so it does not replace Linux -bundle on the CMake loader/extensions/core archives. Keep this narrow fix pending equivalent upstream CMake support.
