# CREATORFUN Permanent Buyback, Burn & Donation

An on-chain program that lets a CREATORFUN token creator commit part of their trading fees, **forever**, to
buying back and burning their own token, to a donation wallet, or to both.

- Website: https://creatorfun.cloud
- Launchpad rules (immutable config): https://github.com/creatorfuncloud/creatorfun-config
- Program ID: *published here after deployment*
- Status: **not deployed yet, not audited** — see [Launch plan](#launch-plan)

> Other languages: [한국어](docs/README.ko.md)

---

## Why

A creator on CREATORFUN earns 0.80% of every trade on their token, wherever the trade happens (on creatorfun.cloud,
through Jupiter or any other app), because every trade goes through the token's Meteora pool.

This program lets the creator make a promise that cannot be broken:

- *"X% of my fees will always buy back and burn the token."*
- *"Y% of my fees will always go to this donation wallet."*

Once a creator turns it on, **no instruction exists to turn it off or change it — not for the creator, not for
CREATORFUN.** The one remaining power, the program upgrade authority during probation, is described openly in
[Probation](#probation-the-one-remaining-power).

It works the same for **SOL coins** and **stock-pair coins** (tokens paired with a tokenized stock such as TSLAx).
Everything is paid in the pool's own pair asset: SOL for SOL coins, the stock token for stock-pair coins.

## The creator's choice

At launch the creator picks one of four setups. The choice is final.

| Buyback share | Donation share | What happens to the creator's fees |
|---|---|---|
| 0% | 0% | Program not used. Fees go to the creator as usual. |
| 1–100% | 0% | Buyback share buys & burns the token, the rest goes to the creator. |
| 0% | 1–100% | Donation share goes to the donation wallet, the rest goes to the creator. |
| any | any | Buyback, donation and creator, together at most 100%. |

The **donation wallet** can be any normal wallet address. It does not need a CREATORFUN profile, so people in an
emergency can be helped right away. Program accounts are refused (they cannot receive SOL, which would block every
later run). Every amount the vault donates is recorded on-chain (`total_donated`) and visible on Solscan.

## How it works

```
 trades anywhere ──► Meteora pool keeps creator fees (0.80%, in the pair asset)
                                  │
     claim_curve_fees / claim_curve_surplus / claim_amm_fees   (anyone can call)
                                  ▼
                   program authority (PDA, no private key)
                                  │
      buyback share ≥ 1%:  prepare_*  ──►  execute_*  25–300 slots later (same caller)
      buyback share = 0%:  distribute
                                  │
        ┌─────────────────────────┼──────────────────────────┐
  buyback share               donation share               the rest
  buys the token on its       sent to the donation         sent to the creator
  own pool; every bought      wallet saved in enable       wallet saved in enable
  token is burned in the
  same transaction
```

1. **`enable`** — The creator picks the buyback share, the donation share, the donation wallet and a run threshold.
   The program reads the Meteora pool and its config on-chain and checks: the pool is a Meteora bonding-curve pool,
   its config names the **CREATORFUN fee wallet** as fee claimer (the SOL config and every stock-pair config do),
   the pair asset matches the config, the signer is the current pool creator, the mint matches, and graduation has
   not started. The creator pays a 0.005 SOL reserve (rent for temporary accounts, never paid out). For stock-pair
   coins, the stock-token accounts of the program, the creator and the donation wallet are opened here, paid by the
   creator. The program then moves the pool's creator rights to its authority PDA with Meteora's
   `transfer_pool_creator` and **re-reads the pool to confirm the move**. This is the last thing the creator can
   ever decide.
2. **Claims (anyone can call)**
   - `claim_curve_fees` — creator trading fees from the bonding curve.
   - `claim_curve_surplus` — the creator's share of the curve surplus after the curve completes.
   - `claim_amm_fees` — after graduation, the fees of the creator's locked LP position in the DAMM v2 pool.
3. **`prepare_curve` / `prepare_amm`** (buyback share ≥ 1%) — only when a run is ready: record the pool's current
   price, slot and caller. A live prepare can only be replaced by the keeper, so nobody can keep cancelling the
   keeper's runs.
4. **`execute_curve` / `execute_amm`** — called by the same wallet that called `prepare`, 25 to 300 slots (about 10
   seconds to 2 minutes) later, and only if the price has not moved more than about 1% against the buy. The program
   buys with the buyback share, burns every token it bought (SPL `burn`: the supply really goes down), sends the
   donation share to the donation wallet and the rest to the creator. If fees were paid in the token itself (LP
   fees), it burns, donates and pays those by the same shares.
5. **`distribute`** (buyback share = 0%) — same run rules, no buy, no price check: donation share to the donation
   wallet, the rest to the creator.

### Units

Limits are written in **units**. One unit is 1/85 of the pool config's graduation amount, read on-chain in `enable`.

- SOL coins graduate at 85 SOL, so **1 unit = 1 SOL**.
- A stock-pair config graduates at the value of 85 SOL in that stock token (priced when the config was created),
  so 1 unit is the amount of that stock token that was worth about 1 SOL then.

Nobody chooses the unit; it comes from the pool's own config, which cannot change.

### Run rules (compiled in, the same for every token)

| Rule | Value |
|---|---|
| A run is allowed when | collected ≥ threshold, **or** 7 days since the last run and ≥ 0.01 unit collected |
| Run threshold (chosen in `enable`) | 0.1 to 10 units |
| Minimum gap between runs | 10 minutes |
| Max buyback per run | 1 unit (bigger balances are processed over several runs) |
| Max buyback per 24 hours | 5 units per token, in fixed 24-hour windows (a window starts at the first run after the last one ended, so around a boundary two windows can be used close together). A leftover allowance under 0.01 unit waits for the next window |
| Price check | `execute` must come 25–300 slots after `prepare`, from the same wallet, and the pool price may be at most ~1% worse than at `prepare` (50 bps on the sqrt price) |
| Slippage | the program computes a floor on-chain: at least 75% of the tokens the prepared price gives before fees and price impact. The caller can only ask for more (`min_tokens_out`), never less. If the swap gives less, the whole transaction fails and nothing is lost |
| Who can run | the CREATORFUN keeper whenever the rules allow; **anyone** once 8 days have passed since the last run |
| Small payouts | payouts below 0.001 unit are kept on record (`creator_owed`, `donee_owed`) and paid with a later run, never lost |

## What this program can and cannot do

| Can | Cannot (no instruction exists for it) |
|---|---|
| Buy the token with the buyback share and burn it | Turn it off, pause it, or undo `enable` |
| Send the donation share to the donation wallet saved in `enable` | Change either share, the threshold, the creator wallet or the donation wallet |
| Send the rest to the creator wallet saved in `enable` | Give the creator rights back or to anyone else |
| Record totals (claimed, spent, burned, donated, paid, owed) on-chain | Send fees to any wallet other than the donation wallet or the creator |
| | Let CREATORFUN, the keeper or anyone else withdraw funds (there is no admin instruction) |

The rule fields of a vault (`pool`, `config`, `base_mint`, `quote_mint`, `creator`, `donee`, `buyback_bps`,
`donation_bps`, `threshold`, `unit`, `created_at`) are written **only** in `enable`. You can check this by searching
`lib.rs`: no other instruction writes them.

## Exactly what the keeper can do

The keeper (`5nMsHjBcHCtH2wu1M5BSymsZLwt3oosdk82bwXvXpd4t`) is the wallet the CREATORFUN server uses to run vaults
on time. It is its own key, separate from the fee wallet and every test wallet. (The devnet test build uses
`5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF`.)

- It **can**: call `prepare_*`, `execute_*` and `distribute` before the 8-day public window opens, replace a live
  prepare, and ask for a higher `min_tokens_out` than the on-chain floor.
- It **cannot**: receive any funds, choose where funds go, skip the price check, go below the on-chain minimum,
  exceed the per-run or daily caps, or change any rule.
- If the keeper stops, anyone can run the vault 8 days after its last run. Claims are open to anyone at all times.
- The keeper asks for `min_tokens_out` from a fresh quote with 1% slippage (above the on-chain floor) and retries
  with a new prepare if a run fails.

## Probation: the one remaining power

During probation the program is deployed **with an upgrade authority**, so bugs can be fixed. Whoever holds an
upgrade authority can replace the program code, so during probation the promises above rely on CREATORFUN not
abusing that power. To keep that risk away from other people's money:

- While `PROBATION = true`, **only CREATORFUN's own wallet** (listed in `PROBATION_CREATORS` in `lib.rs`) can call
  `enable`. No outside creator's fees depend on the program while it can still be upgraded.
- The upgrade authority address and every upgrade are published in this README.
- The public version sets `PROBATION = false` and is released **together with** the removal of the upgrade authority
  (`solana program set-upgrade-authority --final`). From then on nobody can change the code, including CREATORFUN.

## Known limits (please read)

- **Unaudited.** CREATORFUN could not afford a professional audit. The code is public, the rules are small pure
  functions with unit tests, and the launch is staged. Please review it and report problems — see
  [SECURITY.md](SECURITY.md).
- **MEV is reduced, not removed.** A buyback is a market buy. The `prepare` → `execute` price check makes a
  same-block price push fail, the 25-slot wait means a pushed price must survive about 10 seconds of arbitrage, only
  the preparing wallet can execute, the on-chain minimum stops a run from accepting a very bad fill, and the caps
  limit how much any single run or day can lose. Someone willing to hold a manipulated price for that long and take
  on the arbitrage risk could still make a run worse; each run is at most 1 unit.
- **Stock tokens have an issuer.** Tokenized stocks (for example xStocks such as TSLAx) are Token-2022 tokens whose
  issuer keeps powers this program cannot remove: it can pause transfers, move tokens out of any account (permanent
  delegate), freeze accounts, or add a transfer hook later. If the issuer pauses or freezes, runs of stock-pair coins
  stop until it resumes; if it adds a transfer hook, those runs fail until a program upgrade supports it. SOL coins
  are not affected.
- **Fees before `enable`.** Creator fees that were not claimed before `enable` belong to the program afterwards and
  are split like any other income.
- **Rent paid by callers.** When a payout needs a new token account (for example the creator's token account for LP
  fees paid in the token), the caller of the run pays its rent.
- **Meteora layout.** The program reads fixed byte offsets of Meteora accounts (listed at the top of `lib.rs`,
  checked against live mainnet accounts). If Meteora changed those layouts, calls would fail rather than misread:
  every read also checks the owner program and the account discriminator.
- **The graduated pool is fixed.** After graduation the program only uses the DAMM v2 pool derived from the CREATORFUN
  migration config and the two mints, and only an LP position owned by the authority, so a fake pool cannot be
  substituted.

## Where the creator's money goes (full list)

With CREATORFUN's config (`creatorfun-config`): creator trading fee share 80% of the 1.25% pool fee (= 0.80% of
volume), creator migration fee 0%, graduated LP locked. So after `enable`, the creator's on-chain income is exactly:

| Source | Claimed by |
|---|---|
| Bonding-curve creator trading fees | `claim_curve_fees` |
| Creator share of the curve surplus | `claim_curve_surplus` |
| Fees of the creator's locked DAMM v2 LP position | `claim_amm_fees` |

All three go through the same split: buyback share → buy & burn, donation share → donation wallet, the rest →
creator wallet.

## Launch plan

1. **Devnet**: every instruction tested end-to-end with real Meteora devnet pools (build with `--features devnet`):
   SOL coins with buyback + donation, donation only, and a Token-2022 stock-pair stand-in.
2. **Mainnet probation**: deployed with an upgrade authority; only the CREATORFUN wallet can enable. Upgrade authority
   address and every upgrade published here.
3. **Public**: `PROBATION = false` and the upgrade authority is removed in the same release. Nobody can change the
   code after that.

## Build and verify

```bash
cargo test --manifest-path programs/creatorfun-buyback/Cargo.toml   # run-rule unit tests
cargo build-sbf --manifest-path programs/creatorfun-buyback/Cargo.toml   # Solana 2.1.21, Anchor 0.31.1
sha256sum target/deploy/creatorfun_buyback.so
python3 scripts/mkidl.py programs/creatorfun-buyback/src/lib.rs idl.json   # client IDL from the source
```

Every push is built by GitHub Actions (`.github/workflows/build.yml`), which publishes the program binary and its
SHA-256 hash.

## License

MIT
