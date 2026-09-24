# Security policy

This program has **not** been professionally audited. We rely on open code, a staged launch and people like you. Thank you for looking.

## Scope

- `programs/creatorfun-buyback/src/lib.rs` — the on-chain program.
- The build workflow (`.github/workflows/build.yml`), if it could publish a binary that does not match the source.

Out of scope: the Meteora programs themselves (report those to Meteora), the creatorfun.cloud website, and issues that need the upgrade authority key (that power is disclosed in the README).

## What we most want to hear about

- Any way to move vault SOL or tokens anywhere other than "buy & burn" or "the creator wallet saved in `enable`".
- Any way to change a vault's rule fields after `enable`, or to undo `enable`.
- Any way to make a buyback run at a manipulated price beyond the limits described in the README.
- Any way to permanently block claims or runs for a vault.
- Wrong Meteora account offsets or instruction layouts.

## How to report

- Open a **private** report: GitHub → this repository → *Security* → *Report a vulnerability*.
- Please do **not** open a public issue for an unfixed vulnerability.
- Include the instruction, the accounts and the steps. A devnet transaction or a failing test is ideal.

We will reply within 72 hours and publish the fix and the upgrade transaction in the README.

## Rewards

Every valid report is credited by name (or handle) in the README, unless you prefer to stay anonymous. Any cash reward for this program will be announced in this file before the public launch.

## Please

Test on devnet or with your own tokens. Do not attack live vaults that hold other people's funds.
