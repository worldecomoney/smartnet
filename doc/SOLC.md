For WEM, we use a small, explicitly versioned set of Solidity compilers rather than every released `solc` version. one important distinction: the compiler version is part of contract reproducibility and risk assessment, not a security guarantee by itself.

avoid selecting versions solely because they're popular. A widely used compiler can still have code-generation bugs, and a newer compiler can introduce regressions. Solidity maintains a machine-readable list of known compiler bugs, including affected version ranges, severity, and conditions such as whether `viaIR` is enabled.

Three goals policy:

- Preserve compatibility with older Solidity projects.
- Make a newer, maintained compiler the default for new deployments.
- Prevent known-buggy compiler releases from becoming an easy route to deploying unsafe bytecode.

## 1. Recommended WEM allowlist

change your proposed list as follows. The important discovery is that some of the versions you've selected fall inside known compiler-bug ranges.

0.8.37 — primary compiler

Recommended

Use as the default for new WEM contracts. It fixes several bugs affecting previous 0.8.x versions, including an IR code-generation issue introduced in 0.8.26 and fixed in 0.8.37.


0.8.23 — legacy 0.8.x

Compatibility

A reasonable historical checkpoint to consider for older projects. It includes the fix for the storage-array write bug introduced in 0.8.5. It is still an older compiler, so it must not be treated as bug-free.
Solidity 0.8.38-develop documentation



0.7.6 — legacy 0.7.x

The final patch release of the 0.7 series is a more sensible compatibility choice than 0.7.0. Legacy support should be explicitly marked and subject to additional review.

0.6.12 — legacy 0.6.x

Keep this only if supporting genuinely older contracts is a requirement. It is the final release of the 0.6 series, but it predates many later compiler fixes.



## 2. What to do with your original choices

| Your version | Recommendation             | Reason                                                                           |
| ------------ | -------------------------- | -------------------------------------------------------------------------------- |
| `0.6.12`     | Keep as legacy             | Older compatibility                                                              |
| `0.7.0`      | Replace with `0.7.6`       | Prefer the final patch release                                                   |
| `0.8.26`     | Exclude                    | Known IR code-generation bug fixed in `0.8.37`                                   |
| `0.8.30`     | Exclude                    | Falls within a known transient-storage bug range fixed in `0.8.34`               |
| `0.8.36`     | Optional legacy checkpoint | Fixes several 2026 bugs, but is superseded by `0.8.37` for the identified issues |
| `0.8.37`     | Add                        | Recommended primary candidate                                                    |

Sources: Solidity's official [known compiler bugs](https://docs.soliditylang.org/en/latest/bugs.html) and its per-version bug records.


One qualification: these findings concern specific compiler pipelines and settings. For example, the transient-storage bug requires `viaIR` and an EVM target of Cancun or later. Excluding an entire version is a defensible conservative policy, but not every contract compiled with that version is necessarily affected.

## 3. The policy matters more than the list

For WEM, separate three things:

- Compiler admission: which exact compiler binaries your supported tooling permits.
- Deployment security: whether the resulting contract passes your deployment checks.
- Runtime consensus: whether the bytecode is valid and executable under WEM's EVM rules.

This distinction is important because deployed EVM bytecode does not generally prove which compiler produced it. A `pragma solidity` declaration is a source-level constraint, not a consensus-enforced compiler restriction.

avoid making validator consensus depend on trusting a compiler version string embedded in source metadata. If WEM needs an on-chain compiler policy, it needs a verifiable deployment mechanism, not merely an allowlist in a wallet or explorer.

### final recommendation

Start with four supported releases:

- `0.6.12` — legacy
- `0.7.6` — legacy
- `0.8.23` — older 0.8 compatibility
- `0.8.37` — primary

Treat the three older releases as compatibility exceptions, not as equally secure alternatives. Before freezing this list for mainnet, run every candidate against Solidity's machine-readable bug database, then repeat that review whenever you consider adding or replacing a compiler.

One final point: pin the exact compiler build and its hash, not just the version number. Also record optimizer settings, `viaIR`, EVM target, and other compilation inputs when contracts are verified. That makes contract builds reproducible and allows you to assess compiler bugs against the actual configuration used.
