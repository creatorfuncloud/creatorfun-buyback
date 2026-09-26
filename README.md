# CREATORFUN Permanent Buyback & Burn

An on-chain program that lets a CREATORFUN token creator commit part of their trading fees to **buying back and burning their own token, forever**.

- Website: https://creatorfun.cloud
- Launchpad rules (immutable config): https://github.com/creatorfuncloud/creatorfun-config
- Program ID: *published here after deployment*
- Status: **not deployed yet, not audited** — see [Launch plan](#launch-plan)

> Other languages: [한국어](docs/README.ko.md)

---

## Why

A creator on CREATORFUN earns 0.80% of every trade on their token, wherever the trade happens (on creatorfun.cloud, on pump.fun, through Jupiter or any other app), because every trade goes through the token's Meteora pool.

This program lets the creator promise holders: *"X% of my fees will always be used to buy back and burn the token."*
A promise only means something if it cannot be broken. So once a creator turns it on, **no instruction exists to turn it off or change it — not for the creator, not for CREATORFUN.** The one remaining power, the program upgrade authority during probation, is described openly in [Probation](#probation-the-one-remaining-power).

## How it works

```
 trades anywhere ──► Meteora pool keeps creator fees (0.80%)
                                  │
     claim_curve_fees / claim_curve_surplus / claim_amm_fees   (anyone can call)
                                  ▼
                   buyback authority (program PDA, no private key)
                                  │
         prepare_* records the pool price  ──►  execute_* 25–300 slots later (same caller)
                   │                                   │
        buyback_bps of the SOL                  the rest of the SOL
        buys the token on its own pool          is sent to the creator wallet
                   │
        every bought token is burned in the same transaction
```

1. **`enable`** — The creator picks a buyback share (5–100%) and a run threshold (0.1–10 SOL). The program reads the Meteora pool on-chain and checks: it is owned by the Meteora bonding-curve program, it uses the CREATORFUN config, the signer is its current creator, the mint matches, and graduation has not started. The creator pays a 0.005 SOL reserve (used only for temporary account rent, never paid out). The program then moves the pool's creator rights to its authority PDA with Meteora's `transfer_pool_creator` and **re-reads the pool to confirm the move**. This is the last thing the creator can ever decide.
2. **Claims (anyone can call)**
   - `claim_curve_fees` — creator trading fees from the bonding curve.
   - `claim_curve_surplus` — the creator's share of the curve surplus after the curve completes.
   - `claim_amm_fees` — after graduation, the fees of the creator's locked LP position in the DAMM v2 pool.
   SOL arrives as wSOL and is unwrapped to plain SOL in the same instruction.
3. **`prepare_curve` / `prepare_amm`** — only when a run is ready: record the pool's current price, slot and caller. A live prepare can only be replaced by the keeper, so nobody can keep cancelling the keeper's runs.
4. **`execute_curve` / `execute_amm`** — called by the same wallet that called `prepare`, 25 to 300 slots (about 10 seconds to 2 minutes) later, and only if the price has not moved more than about 1% against the buy, the program:
   - spends `buyback_bps` of the collected SOL buying the token on its own pool,
   - burns every token it bought (SPL `burn`: the supply really goes down),
   - sends the rest of the SOL to the creator wallet saved in `enable`,
   - if the LP paid fees in the token itself, burns the same share of them and sends the rest to the creator.

### Run rules (compiled in, the same for every token)

| Rule | Value |
|---|---|
| A run is allowed when | collected SOL ≥ threshold, **or** 7 days since the last run and ≥ 0.01 SOL collected |
| Minimum gap between runs | 10 minutes |
| Max buyback per run | 1 SOL (bigger balances are processed over several runs) |
| Max buyback per 24 hours | 5 SOL per token, in fixed 24-hour windows (a window starts at the first run after the last one ended, so around a boundary two windows can be used close together). A leftover allowance under 0.01 SOL waits for the next window |
| Price check | `execute` must come 25–300 slots after `prepare`, from the same wallet, and the pool price may be at most ~1% worse than at `prepare` (50 bps on the sqrt price) |
| Slippage | the program computes a floor on-chain: at least 75% of the tokens the prepared price gives before fees and price impact. The caller can only ask for more (`min_tokens_out`), never less. If the swap gives less, the whole transaction fails and nothing is lost |
| Who can run | the CREATORFUN keeper whenever the rules allow; **anyone** once 8 days have passed since the last run |
| Small payouts | creator payouts below 0.001 SOL are kept on record (`creator_owed`) and paid with a later run, never lost |

## What this program can and cannot do

| Can | Cannot (no instruction exists for it) |
|---|---|
| Buy the token with `buyback_bps` of the fees and burn it | Turn buyback off, pause it, or undo `enable` |
| Send the rest of the fees to the creator wallet saved in `enable` | Change the share, the threshold or the creator wallet |
| Record totals (claimed, spent, burned, paid, owed) on-chain | Give the creator rights back or to anyone else |
| | Send fees to any wallet other than the creator |
| | Let CREATORFUN, the keeper or anyone else withdraw funds (there is no admin instruction) |

The rule fields of a vault (`pool`, `base_mint`, `creator`, `buyback_bps`, `threshold_lamports`, `created_at`) are written **only** in `enable`. You can check this by searching `lib.rs`: no other instruction writes them.

## Exactly what the keeper can do

The keeper (`5nMsHjBcHCtH2wu1M5BSymsZLwt3oosdk82bwXvXpd4t`) is the wallet the CREATORFUN server uses to run buybacks on time. It is its own key, separate from the fee wallet and every test wallet. (The devnet test build uses `5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF`.)

- It **can**: call `prepare_*` and `execute_*` before the 8-day public window opens, replace a live prepare, and ask for a higher `min_tokens_out` than the on-chain floor.
- It **cannot**: receive any funds, choose where funds go, skip the price check, go below the on-chain minimum, exceed the per-run or daily caps, or change any rule.
- If the keeper stops, anyone can run the vault 8 days after its last run. Claims are open to anyone at all times.
- The keeper asks for `min_tokens_out` from a fresh quote with 1% slippage (above the on-chain floor) and retries with a new prepare if a run fails.

## Probation: the one remaining power

During probation the program is deployed **with an upgrade authority**, so bugs can be fixed. Whoever holds an upgrade authority can replace the program code, so during probation the promises above rely on CREATORFUN not abusing that power. To keep that risk away from other people's money:

- While `PROBATION = true`, **only CREATORFUN's own test wallets** (listed in `PROBATION_CREATORS` in `lib.rs`) can call `enable`. No outside creator's fees depend on the program while it can still be upgraded.
- The upgrade authority address and every upgrade are published in this README.
- The public version sets `PROBATION = false` and is released **together with** the removal of the upgrade authority (`solana program set-upgrade-authority --final`). From then on nobody can change the code, including CREATORFUN.

## Known limits (please read)

- **Unaudited.** CREATORFUN could not afford a professional audit. The code is public, the rules are small pure functions with unit tests, and the launch is staged. Please review it and report problems — see [SECURITY.md](SECURITY.md).
- **MEV is reduced, not removed.** A buyback is a market buy. The `prepare` → `execute` price check makes a same-block price push fail, the 25-slot wait means a pushed price must survive about 10 seconds of arbitrage, only the preparing wallet can execute, the on-chain minimum stops a run from accepting a very bad fill, and the caps limit how much any single run or day can lose. Someone willing to hold a manipulated price for that long and take on the arbitrage risk could still make a run worse; each run is at most 1 SOL.
- **Meteora layout.** The program reads fixed byte offsets of Meteora accounts (listed at the top of `lib.rs`, checked against live mainnet accounts). If Meteora changed those layouts, calls would fail rather than misread: every read also checks the owner program and the account discriminator.
- **SPL Token only.** Token-2022 mints are not supported. CREATORFUN tokens are SPL tokens.
- **The graduated pool is fixed.** After graduation the program only uses the DAMM v2 pool derived from the CREATORFUN migration config and the token mint, and only an LP position owned by the authority, so a fake pool cannot be substituted.

## Where the creator's money goes (full list)

With CREATORFUN's config (`creatorfun-config`): creator trading fee share 80% of the 1.25% pool fee (= 0.80% of volume), creator migration fee 0%, graduated LP locked. So after `enable`, the creator's on-chain income is exactly:

| Source | Claimed by |
|---|---|
| Bonding-curve creator trading fees | `claim_curve_fees` |
| Creator share of the curve surplus | `claim_curve_surplus` |
| Fees of the creator's locked DAMM v2 LP position | `claim_amm_fees` |

All three go through the same split: `buyback_bps` → buy & burn, the rest → creator wallet.

## Launch plan

1. **Devnet**: every instruction tested end-to-end with real Meteora devnet pools (build with `--features devnet`).
2. **Mainnet probation**: deployed with an upgrade authority; only CREATORFUN test wallets can enable. Upgrade authority address and every upgrade published here.
3. **Public**: `PROBATION = false` and the upgrade authority is removed in the same release. Nobody can change the code after that.

## Build and verify

```bash
cargo test --manifest-path programs/creatorfun-buyback/Cargo.toml   # run-rule unit tests
anchor build                                                        # Anchor 0.31.1, Solana 2.1.21
sha256sum target/deploy/creatorfun_buyback.so
```

Every push is built by GitHub Actions (`.github/workflows/build.yml`), which publishes the program binary and its SHA-256 hash.

## License

MIT
