//! CREATORFUN permanent buyback, burn & donation
//!
//! A creator can turn this on ONCE for a CREATORFUN token before it graduates, for SOL coins and stock-pair
//! coins alike. From that moment the token's creator-fee rights belong to this program, forever.
//!
//! What the program does with the creator's income (and nothing else), in the pool's own pair asset
//! (SOL for SOL coins, the stock token for stock-pair coins):
//!   1. Buys the token with `buyback_bps` of it and BURNS every token it buys, in the same transaction.
//!   2. Sends `donation_bps` of it to the donation wallet chosen in `enable` (optional).
//!   3. Sends the rest to the creator wallet that turned it on.
//! Either share may be 0; together they are at most 100%.
//!
//! What nobody can do through this program (there is no instruction for it):
//!   - turn it off, pause it, or give the creator rights back
//!   - change either share, the run threshold, the creator wallet or the donation wallet
//!   - send the income anywhere except "burn", "the donation wallet" or "the creator wallet"
//!   - withdraw anything as an admin (the program has no admin instruction)
//! The program binary itself can still be upgraded during the probation period. See README "Probation".
//!
//! Instructions:
//!   enable              creator only, once per pool, before graduation
//!   claim_curve_fees    anyone: creator trading fees from the bonding curve -> this program
//!   claim_curve_surplus anyone: creator share of the curve surplus (after the curve completes) -> this program
//!   claim_amm_fees      anyone: creator LP fees from the graduated DAMM v2 pool -> this program
//!   prepare_curve       records the bonding-curve price; the same caller must buy 25..300 slots later near that price
//!   prepare_amm         the same for the graduated DAMM v2 pool
//!   execute_curve       buyback on the bonding curve, burn, pay the donation wallet and the creator
//!   execute_amm         buyback on the graduated DAMM v2 pool, burn, pay the donation wallet and the creator
//!   distribute          vaults with a 0% buyback share: pay the donation wallet and the creator (no buy)

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    pubkey,
};
use anchor_lang::system_program::{self, Transfer as SolTransfer};
use anchor_spl::associated_token::{self, get_associated_token_address, get_associated_token_address_with_program_id, AssociatedToken, Create};
use anchor_spl::token::{self, spl_token::native_mint, Burn, CloseAccount, Mint, SyncNative, Token, TokenAccount, Transfer};
use anchor_spl::token_interface::{self, Mint as AnyMint, TokenAccount as AnyTokenAccount, TokenInterface, TransferChecked};

declare_id!("eJGfjnQn4Gk7gNvyGNmDYPjBQBu6msUmSUr91fyUq2j");

// ---------------------------------------------------------------------------
// Fixed rules. Compiled into the program; the same for every token.
// ---------------------------------------------------------------------------
//
// Amounts are in the pool's pair asset. They are written in "units": one unit is 1/85 of the pool config's
// graduation amount (migration_quote_threshold), read on-chain in `enable` and stored in the vault.
// For SOL coins, whose graduation amount is 85 SOL, one unit is 1 SOL. For a stock pair, one unit is the
// amount of that stock token that was worth about 1 SOL when its config was created.
// The *_MU constants below are thousandths of a unit (1_000 = 1 unit).

/// Graduation amount divided by this is one unit.
pub const UNITS_PER_GRADUATION: u64 = 85;
/// Share limits, in basis points (100 bps = 1%). Buyback and donation are each 0-100%, together at most 100%.
pub const MAX_SHARE_BPS: u16 = 10_000;
/// Run threshold limits.
pub const MIN_THRESHOLD_MU: u64 = 100; // 0.1 unit (0.1 SOL)
pub const MAX_THRESHOLD_MU: u64 = 10_000; // 10 units (10 SOL)
/// If the threshold is not reached, a run is still allowed this long after the last run.
pub const TIMER_SECONDS: i64 = 7 * 24 * 60 * 60; // 7 days
/// Smallest amount a timer run will process.
pub const MIN_TIMER_RUN_MU: u64 = 10; // 0.01 unit
/// Minimum time between two runs of the same vault.
pub const MIN_RUN_GAP: i64 = 10 * 60; // 10 minutes
/// Most one run may spend on the buyback.
pub const MAX_BUY_PER_RUN_MU: u64 = 1_000; // 1 unit (1 SOL)
/// Most one vault may spend on buybacks in one 24-hour window.
pub const MAX_BUY_PER_DAY_MU: u64 = 5_000; // 5 units (5 SOL)
/// Smallest buy a run makes. When the day's remaining allowance is below this, the run waits (DailyCapReached).
pub const MIN_BUY_MU: u64 = 10; // 0.01 unit
/// Payouts smaller than this are kept (owed) and paid together with a later run.
pub const MIN_PAYOUT_MU: u64 = 1; // 0.001 unit
/// Anyone may prepare and run a vault once this long has passed since its last run.
/// Before that only the CREATORFUN keeper does (the keeper can trigger runs; it can never receive funds).
pub const PUBLIC_RUN_DELAY: i64 = 8 * 24 * 60 * 60; // 8 days
/// A buy must happen at least / at most this many slots after `prepare_*`.
pub const MIN_PREPARE_SLOTS: u64 = 25; // about 10 seconds: a pushed price has to survive arbitrage this long
pub const MAX_PREPARE_SLOTS: u64 = 300; // about two minutes
/// How far the pool price may move against the buy between `prepare_*` and the buy (on sqrt price; 50 bps ≈ 1% price).
pub const MAX_SQRT_PRICE_MOVE_BPS: u128 = 50;
/// The swap must return at least this share of the tokens the prepared price would give with no impact or fee.
/// Enforced on-chain for every caller; a caller may only ask for more, never less.
pub const MIN_OUT_BPS: u128 = 7_500; // 75%
/// Lamports kept by the authority to pay for its temporary wSOL account. Never spent or paid out.
pub const AUTHORITY_RESERVE: u64 = 5_000_000; // 0.005 SOL, paid by the creator in `enable`

/// Probation: while true, only the CREATORFUN test wallets below can enable, so no outside
/// creator's money depends on the program while it can still be upgraded. The public version sets
/// this to false and is published together with the removal of the upgrade authority.
pub const PROBATION: bool = true;
#[cfg(not(feature = "devnet"))]
pub const PROBATION_CREATORS: [Pubkey; 1] = [pubkey!("CooB38vtmMP4oLcSsLsmUn1YfLELG7NkfPXYTv21NcBx")]; // CREATORFUN fee wallet
#[cfg(feature = "devnet")]
pub const PROBATION_CREATORS: [Pubkey; 1] = [pubkey!("5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF")]; // devnet test wallet

// ---------------------------------------------------------------------------
// External programs and accounts (Meteora, CREATORFUN)
// ---------------------------------------------------------------------------

pub const DBC_PROGRAM_ID: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
pub const DBC_POOL_AUTHORITY: Pubkey = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
pub const DAMM_PROGRAM_ID: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
pub const DAMM_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");

/// The CREATORFUN fee wallet. A pool is a CREATORFUN pool when its Meteora config names this wallet as the
/// fee claimer: the SOL config and every stock-pair config do, and nobody else can make a config that pays us.
pub const CREATORFUN_FEE_WALLET: Pubkey = pubkey!("CooB38vtmMP4oLcSsLsmUn1YfLELG7NkfPXYTv21NcBx");

/// DAMM v2 config Meteora uses when a CREATORFUN token graduates (migration fee option 2 = FixedBps100).
/// The graduated pool address is derived from it, so no other pool can be used.
pub const DAMM_MIGRATION_CONFIG: Pubkey = pubkey!("Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp");

/// Wallet the CREATORFUN server uses to trigger runs. It pays its own network fees and receives nothing.
/// Mainnet uses its own key, separate from every test and creator wallet.
#[cfg(not(feature = "devnet"))]
pub const KEEPER: Pubkey = pubkey!("5nMsHjBcHCtH2wu1M5BSymsZLwt3oosdk82bwXvXpd4t");
#[cfg(feature = "devnet")]
pub const KEEPER: Pubkey = pubkey!("5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF"); // devnet test wallet

// Meteora account layouts (bytemuck, repr C). Every offset was checked against live mainnet accounts.
const VIRTUAL_POOL_DISCRIMINATOR: [u8; 8] = [213, 224, 5, 209, 98, 69, 119, 92];
const POOL_CONFIG_OFFSET: usize = 72;
const POOL_CREATOR_OFFSET: usize = 104;
const POOL_BASE_MINT_OFFSET: usize = 136;
const POOL_SQRT_PRICE_OFFSET: usize = 280;
const POOL_IS_MIGRATED_OFFSET: usize = 305;
const POOL_MIGRATION_PROGRESS_OFFSET: usize = 308;
const POOL_CONFIG_DISCRIMINATOR: [u8; 8] = [26, 108, 14, 123, 116, 230, 129, 43];
const CONFIG_QUOTE_MINT_OFFSET: usize = 8;
const CONFIG_FEE_CLAIMER_OFFSET: usize = 40;
const CONFIG_MIGRATION_QUOTE_THRESHOLD_OFFSET: usize = 264;
const CONFIG_MIN_LEN: usize = 1048;
const DAMM_POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const DAMM_POOL_TOKEN_A_MINT_OFFSET: usize = 168;
const DAMM_POOL_TOKEN_B_MINT_OFFSET: usize = 200;
const DAMM_POOL_SQRT_PRICE_OFFSET: usize = 456;
const POSITION_DISCRIMINATOR: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];
const POSITION_POOL_OFFSET: usize = 8;
const POSITION_NFT_MINT_OFFSET: usize = 40;
// SPL token account layout (the same for Token and Token-2022): amount at 64.
const TOKEN_ACCOUNT_AMOUNT_OFFSET: usize = 64;

// Meteora instruction discriminators.
const IX_TRANSFER_POOL_CREATOR: [u8; 8] = [20, 7, 169, 33, 58, 147, 166, 33];
const IX_CLAIM_CREATOR_TRADING_FEE: [u8; 8] = [82, 220, 250, 189, 3, 85, 107, 45];
const IX_CREATOR_WITHDRAW_SURPLUS: [u8; 8] = [165, 3, 137, 7, 28, 134, 76, 80];
const IX_CLAIM_POSITION_FEE: [u8; 8] = [180, 38, 154, 17, 133, 33, 162, 211];
const IX_SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200]; // same for DBC and DAMM v2

pub const VAULT_SEED: &[u8] = b"vault";
pub const AUTHORITY_SEED: &[u8] = b"authority";
const VENUE_CURVE: u8 = 1;
const VENUE_AMM: u8 = 2;

#[program]
pub mod creatorfun_buyback {
    use super::*;

    /// Turn it on for one pool. Creator only, once, before graduation.
    /// The creator-fee rights move to this program's authority and can never come back.
    /// `donee` (the donation wallet) is only used when `donation_bps > 0`.
    pub fn enable(ctx: Context<Enable>, buyback_bps: u16, donation_bps: u16, threshold: u64) -> Result<()> {
        require!(shares_ok(buyback_bps, donation_bps), BuybackError::SharesOutOfRange);
        let a = &ctx.accounts;
        require!(!PROBATION || PROBATION_CREATORS.contains(&a.creator.key()), BuybackError::ProbationOnly);
        {
            // Read the Meteora pool directly: signer is its creator, right mint, not graduating.
            let data = curve_pool_data(&a.pool)?;
            require_keys_eq!(read_pubkey(&data, POOL_CONFIG_OFFSET), a.config.key(), BuybackError::WrongConfig);
            require_keys_eq!(read_pubkey(&data, POOL_CREATOR_OFFSET), a.creator.key(), BuybackError::NotThePoolCreator);
            require_keys_eq!(read_pubkey(&data, POOL_BASE_MINT_OFFSET), a.base_mint.key(), BuybackError::WrongBaseMint);
            require!(data[POOL_IS_MIGRATED_OFFSET] == 0 && data[POOL_MIGRATION_PROGRESS_OFFSET] == 0, BuybackError::PoolAlreadyGraduating);
        }
        // The pool's config must pay the CREATORFUN fee wallet (SOL config and every stock-pair config do).
        let unit = {
            require_keys_eq!(*a.config.owner, DBC_PROGRAM_ID, BuybackError::NotACreatorfunPool);
            let data = a.config.try_borrow_data()?;
            require!(data.len() >= CONFIG_MIN_LEN && data[..8] == POOL_CONFIG_DISCRIMINATOR, BuybackError::NotACreatorfunPool);
            require_keys_eq!(read_pubkey(&data, CONFIG_FEE_CLAIMER_OFFSET), CREATORFUN_FEE_WALLET, BuybackError::NotACreatorfunPool);
            require_keys_eq!(read_pubkey(&data, CONFIG_QUOTE_MINT_OFFSET), a.quote_mint.key(), BuybackError::WrongQuoteMint);
            read_u64(&data, CONFIG_MIGRATION_QUOTE_THRESHOLD_OFFSET) / UNITS_PER_GRADUATION
        };
        require!(unit > 0, BuybackError::NotACreatorfunPool);
        require!(threshold >= mu(unit, MIN_THRESHOLD_MU) && threshold <= mu(unit, MAX_THRESHOLD_MU), BuybackError::ThresholdOutOfRange);
        require_keys_eq!(*a.quote_mint.to_account_info().owner, a.quote_token_program.key(), BuybackError::WrongQuoteMint);
        let quote_is_sol = a.quote_mint.key() == native_mint::ID;
        let donee = if donation_bps > 0 {
            let d = &a.donee;
            // A program account cannot receive SOL, which would block every later run.
            require!(d.key() != Pubkey::default() && d.key() != a.authority.key() && !d.executable, BuybackError::BadDonee);
            d.key()
        } else {
            Pubkey::default()
        };

        // Fund the authority's small reserve (rent for its temporary wSOL account).
        system_program::transfer(
            CpiContext::new(a.system_program.to_account_info(), SolTransfer { from: a.creator.to_account_info(), to: a.authority.to_account_info() }),
            AUTHORITY_RESERVE,
        )?;

        // Stock-pair coins: open the pair-asset accounts now, paid by the creator, so later runs never have to.
        if !quote_is_sol {
            let mut owners: Vec<(AccountInfo, AccountInfo)> = vec![
                (a.authority.to_account_info(), a.authority_quote_ata.to_account_info()),
                (a.creator.to_account_info(), a.creator_quote_ata.to_account_info()),
            ];
            if donation_bps > 0 {
                owners.push((a.donee.to_account_info(), a.donee_quote_ata.to_account_info()));
            }
            for (owner, ata) in owners {
                open_ata(
                    a.creator.to_account_info(),
                    ata,
                    owner,
                    a.quote_mint.to_account_info(),
                    a.quote_token_program.to_account_info(),
                    a.associated_token_program.to_account_info(),
                    a.system_program.to_account_info(),
                    &[],
                )?;
            }
        }

        // Hand the creator rights to this program's authority. There is no instruction to undo this.
        let ix = Instruction {
            program_id: DBC_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(a.pool.key(), false),
                AccountMeta::new_readonly(a.config.key(), false),
                AccountMeta::new_readonly(a.creator.key(), true),
                AccountMeta::new_readonly(a.authority.key(), false),
                AccountMeta::new_readonly(a.dbc_event_authority.key(), false),
                AccountMeta::new_readonly(DBC_PROGRAM_ID, false),
            ],
            data: IX_TRANSFER_POOL_CREATOR.to_vec(),
        };
        invoke(
            &ix,
            &[
                a.pool.to_account_info(),
                a.config.to_account_info(),
                a.creator.to_account_info(),
                a.authority.to_account_info(),
                a.dbc_event_authority.to_account_info(),
                a.dbc_program.to_account_info(),
            ],
        )?;
        {
            let data = a.pool.try_borrow_data()?;
            require_keys_eq!(read_pubkey(&data, POOL_CREATOR_OFFSET), a.authority.key(), BuybackError::CreatorTransferFailed);
        }

        let now = Clock::get()?.unix_timestamp;
        let quote_mint = a.quote_mint.key();
        let quote_token_program = a.quote_token_program.key();
        let config = a.config.key();
        let pool = a.pool.key();
        let base_mint = a.base_mint.key();
        let creator = a.creator.key();
        let vault = &mut ctx.accounts.vault;
        vault.pool = pool;
        vault.config = config;
        vault.base_mint = base_mint;
        vault.quote_mint = quote_mint;
        vault.quote_token_program = quote_token_program;
        vault.quote_is_sol = quote_is_sol;
        vault.creator = creator;
        vault.donee = donee;
        vault.buyback_bps = buyback_bps;
        vault.donation_bps = donation_bps;
        vault.threshold = threshold;
        vault.unit = unit;
        vault.created_at = now;
        vault.last_run = now;
        vault.day_start = now;
        vault.bump = ctx.bumps.vault;
        vault.authority_bump = ctx.bumps.authority;

        emit!(BuybackEnabled { pool, base_mint, quote_mint, creator, donee, buyback_bps, donation_bps, threshold, unit });
        Ok(())
    }

    /// Anyone: move the creator trading fees from the bonding curve into this program.
    pub fn claim_curve_fees(ctx: Context<ClaimCurve>) -> Result<()> {
        let a = &ctx.accounts;
        let pool_key = a.vault.pool;
        let bump = [a.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let sol = a.vault.quote_is_sol;
        let before = income_balance(sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());

        if sol {
            open_wsol_for(&a.authority, &a.authority_quote_ata, &a.quote_mint, &a.quote_token_program, &a.associated_token_program, &a.system_program, signer)?;
        }
        let ix = Instruction {
            program_id: DBC_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new(a.pool.key(), false),
                AccountMeta::new(a.authority_base_ata.key(), false),
                AccountMeta::new(a.authority_quote_ata.key(), false),
                AccountMeta::new(a.base_vault.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new_readonly(a.base_mint.key(), false),
                AccountMeta::new_readonly(a.quote_mint.key(), false),
                AccountMeta::new_readonly(a.authority.key(), true),
                AccountMeta::new_readonly(a.token_program.key(), false),
                AccountMeta::new_readonly(a.quote_token_program.key(), false),
                AccountMeta::new_readonly(a.dbc_event_authority.key(), false),
                AccountMeta::new_readonly(DBC_PROGRAM_ID, false),
            ],
            data: ix_data_two_u64(IX_CLAIM_CREATOR_TRADING_FEE, u64::MAX, u64::MAX),
        };
        invoke_signed(
            &ix,
            &[
                a.dbc_pool_authority.to_account_info(),
                a.pool.to_account_info(),
                a.authority_base_ata.to_account_info(),
                a.authority_quote_ata.to_account_info(),
                a.base_vault.to_account_info(),
                a.quote_vault.to_account_info(),
                a.base_mint.to_account_info(),
                a.quote_mint.to_account_info(),
                a.authority.to_account_info(),
                a.token_program.to_account_info(),
                a.quote_token_program.to_account_info(),
                a.dbc_event_authority.to_account_info(),
                a.dbc_program.to_account_info(),
            ],
            signer,
        )?;
        if sol {
            close_wsol(&a.authority.to_account_info(), &a.authority_quote_ata.to_account_info(), &a.quote_token_program.to_account_info(), signer)?;
        }

        let after = income_balance(sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());
        record_claim(&mut ctx.accounts.vault, 0, after.saturating_sub(before));
        Ok(())
    }

    /// Anyone: after the curve completes, move the creator's share of the curve surplus into this program.
    pub fn claim_curve_surplus(ctx: Context<ClaimCurve>) -> Result<()> {
        let a = &ctx.accounts;
        let pool_key = a.vault.pool;
        let bump = [a.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let sol = a.vault.quote_is_sol;
        let before = income_balance(sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());

        if sol {
            open_wsol_for(&a.authority, &a.authority_quote_ata, &a.quote_mint, &a.quote_token_program, &a.associated_token_program, &a.system_program, signer)?;
        }
        let ix = Instruction {
            program_id: DBC_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.dbc_config.key(), false),
                AccountMeta::new(a.pool.key(), false),
                AccountMeta::new(a.authority_quote_ata.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new_readonly(a.quote_mint.key(), false),
                AccountMeta::new_readonly(a.authority.key(), true),
                AccountMeta::new_readonly(a.quote_token_program.key(), false),
                AccountMeta::new_readonly(a.dbc_event_authority.key(), false),
                AccountMeta::new_readonly(DBC_PROGRAM_ID, false),
            ],
            data: IX_CREATOR_WITHDRAW_SURPLUS.to_vec(),
        };
        invoke_signed(
            &ix,
            &[
                a.dbc_pool_authority.to_account_info(),
                a.dbc_config.to_account_info(),
                a.pool.to_account_info(),
                a.authority_quote_ata.to_account_info(),
                a.quote_vault.to_account_info(),
                a.quote_mint.to_account_info(),
                a.authority.to_account_info(),
                a.quote_token_program.to_account_info(),
                a.dbc_event_authority.to_account_info(),
                a.dbc_program.to_account_info(),
            ],
            signer,
        )?;
        if sol {
            close_wsol(&a.authority.to_account_info(), &a.authority_quote_ata.to_account_info(), &a.quote_token_program.to_account_info(), signer)?;
        }

        let after = income_balance(sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());
        record_claim(&mut ctx.accounts.vault, 1, after.saturating_sub(before));
        Ok(())
    }

    /// Anyone: after graduation, move the creator LP fees from the DAMM v2 position into this program.
    /// The DAMM v2 pool must be the one Meteora created at graduation (address derived on-chain),
    /// and the position must belong to that pool and be held by this program's authority.
    pub fn claim_amm_fees(ctx: Context<ClaimAmmFees>) -> Result<()> {
        let a = &ctx.accounts;
        {
            let data = curve_pool_data(&a.curve_pool)?;
            require!(data[POOL_IS_MIGRATED_OFFSET] == 1, BuybackError::NotGraduatedYet);
        }
        let base_is_a = check_amm_pool(&a.amm_pool, a.vault.base_mint, a.vault.quote_mint)?.1;
        {
            require_keys_eq!(*a.position.owner, DAMM_PROGRAM_ID, BuybackError::NotOurPosition);
            let data = a.position.try_borrow_data()?;
            require!(data.len() >= POSITION_NFT_MINT_OFFSET + 32 && data[..8] == POSITION_DISCRIMINATOR, BuybackError::NotOurPosition);
            require_keys_eq!(read_pubkey(&data, POSITION_POOL_OFFSET), a.amm_pool.key(), BuybackError::NotOurPosition);
            let nft = &a.position_nft_account;
            require_keys_eq!(nft.mint, read_pubkey(&data, POSITION_NFT_MINT_OFFSET), BuybackError::NotOurPosition);
            require_keys_eq!(nft.owner, a.authority.key(), BuybackError::NotOurPosition);
            require!(nft.amount == 1, BuybackError::NotOurPosition);
        }

        let pool_key = a.vault.pool;
        let bump = [a.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let sol = a.vault.quote_is_sol;
        let before = income_balance(sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());

        if sol {
            open_wsol_for(&a.authority, &a.authority_quote_ata, &a.quote_mint, &a.quote_token_program, &a.associated_token_program, &a.system_program, signer)?;
        }
        let (acc_a, acc_b) = if base_is_a { (a.authority_base_ata.key(), a.authority_quote_ata.key()) } else { (a.authority_quote_ata.key(), a.authority_base_ata.key()) };
        let (mint_a, mint_b) = if base_is_a { (a.base_mint.key(), a.quote_mint.key()) } else { (a.quote_mint.key(), a.base_mint.key()) };
        let (prog_a, prog_b) = if base_is_a { (a.token_program.key(), a.quote_token_program.key()) } else { (a.quote_token_program.key(), a.token_program.key()) };
        let ix = Instruction {
            program_id: DAMM_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(DAMM_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(a.amm_pool.key(), false),
                AccountMeta::new(a.position.key(), false),
                AccountMeta::new(acc_a, false),
                AccountMeta::new(acc_b, false),
                AccountMeta::new(a.token_a_vault.key(), false),
                AccountMeta::new(a.token_b_vault.key(), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new_readonly(a.position_nft_account.key(), false),
                AccountMeta::new_readonly(a.authority.key(), true),
                AccountMeta::new_readonly(prog_a, false),
                AccountMeta::new_readonly(prog_b, false),
                AccountMeta::new_readonly(a.damm_event_authority.key(), false),
                AccountMeta::new_readonly(DAMM_PROGRAM_ID, false),
            ],
            data: IX_CLAIM_POSITION_FEE.to_vec(),
        };
        invoke_signed(
            &ix,
            &[
                a.damm_pool_authority.to_account_info(),
                a.amm_pool.to_account_info(),
                a.position.to_account_info(),
                a.authority_base_ata.to_account_info(),
                a.authority_quote_ata.to_account_info(),
                a.token_a_vault.to_account_info(),
                a.token_b_vault.to_account_info(),
                a.base_mint.to_account_info(),
                a.quote_mint.to_account_info(),
                a.position_nft_account.to_account_info(),
                a.authority.to_account_info(),
                a.token_program.to_account_info(),
                a.quote_token_program.to_account_info(),
                a.damm_event_authority.to_account_info(),
                a.damm_program.to_account_info(),
            ],
            signer,
        )?;
        if sol {
            close_wsol(&a.authority.to_account_info(), &a.authority_quote_ata.to_account_info(), &a.quote_token_program.to_account_info(), signer)?;
        }

        let after = income_balance(sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());
        record_claim(&mut ctx.accounts.vault, 2, after.saturating_sub(before));
        Ok(())
    }

    /// Record the bonding-curve price. The same caller must call `execute_curve` 25..300 slots later, and the
    /// price at that moment must not be more than about 1% worse. Only runs that are ready can be prepared, and
    /// a live reference can only be replaced by the keeper.
    pub fn prepare_curve(ctx: Context<PrepareCurve>) -> Result<()> {
        let a = &ctx.accounts;
        require!(a.vault.buyback_bps > 0, BuybackError::UseDistribute);
        let balance = income_balance(a.vault.quote_is_sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());
        check_prepare(&a.vault, balance, a.caller.key(), Clock::get()?)?;
        let sqrt = {
            let data = curve_pool_data(&a.curve_pool)?;
            require!(data[POOL_IS_MIGRATED_OFFSET] == 0, BuybackError::AlreadyGraduated);
            read_u128(&data, POOL_SQRT_PRICE_OFFSET)
        };
        let caller = ctx.accounts.caller.key();
        record_reference(&mut ctx.accounts.vault, VENUE_CURVE, sqrt, caller)
    }

    /// Record the graduated DAMM v2 pool price. Same rules as `prepare_curve`.
    pub fn prepare_amm(ctx: Context<PrepareAmm>) -> Result<()> {
        let a = &ctx.accounts;
        require!(a.vault.buyback_bps > 0, BuybackError::UseDistribute);
        let balance = income_balance(a.vault.quote_is_sol, &a.authority.to_account_info(), &a.authority_quote_ata.to_account_info());
        check_prepare(&a.vault, balance, a.caller.key(), Clock::get()?)?;
        let (sqrt, _) = check_amm_pool(&a.amm_pool, a.vault.base_mint, a.vault.quote_mint)?;
        let caller = a.caller.key();
        record_reference(&mut ctx.accounts.vault, VENUE_AMM, sqrt, caller)
    }

    /// Buyback on the bonding curve, burn everything bought, pay the donation wallet and the creator.
    /// `min_tokens_out` can only raise the on-chain floor (MIN_OUT_BPS of the prepared price), never lower it.
    pub fn execute_curve(ctx: Context<ExecuteCurve>, min_tokens_out: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let a = &ctx.accounts;
        let c = &a.common;
        require_keys_eq!(a.venue_pool.key(), c.vault.pool, BuybackError::WrongVenue);
        require_keys_eq!(a.dbc_config.key(), c.vault.config, BuybackError::WrongConfig);
        let sqrt_now = {
            let data = curve_pool_data(&a.venue_pool)?;
            read_u128(&data, POOL_SQRT_PRICE_OFFSET)
        };
        // Buying the token raises the curve's sqrt price, so a higher price than recorded is "worse".
        check_reference(&c.vault, VENUE_CURVE, sqrt_now, true, c.caller.key())?;
        let plan = plan_run(&c.vault, run_balance(c), now, c.caller.key())?;

        let pool_key = c.vault.pool;
        let bump = [c.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let fee_tokens = c.authority_base_ata.amount;

        if plan.buy > 0 {
            let min_out = required_min_out(plan.buy, c.vault.ref_sqrt_price, true, min_tokens_out)?;
            if c.vault.quote_is_sol {
                fund_wsol(c, plan.buy, signer)?;
            }
            let ix = Instruction {
                program_id: DBC_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                    AccountMeta::new_readonly(a.dbc_config.key(), false),
                    AccountMeta::new(a.venue_pool.key(), false),
                    AccountMeta::new(c.authority_quote_ata.key(), false),
                    AccountMeta::new(c.authority_base_ata.key(), false),
                    AccountMeta::new(a.base_vault.key(), false),
                    AccountMeta::new(a.quote_vault.key(), false),
                    AccountMeta::new_readonly(c.base_mint.key(), false),
                    AccountMeta::new_readonly(c.quote_mint.key(), false),
                    AccountMeta::new_readonly(c.authority.key(), true),
                    AccountMeta::new_readonly(c.token_program.key(), false),
                    AccountMeta::new_readonly(c.quote_token_program.key(), false),
                    AccountMeta::new_readonly(DBC_PROGRAM_ID, false), // no referral account
                    AccountMeta::new_readonly(a.dbc_event_authority.key(), false),
                    AccountMeta::new_readonly(DBC_PROGRAM_ID, false),
                ],
                data: ix_data_two_u64(IX_SWAP, plan.buy, min_out),
            };
            invoke_signed(
                &ix,
                &[
                    a.dbc_pool_authority.to_account_info(),
                    a.dbc_config.to_account_info(),
                    a.venue_pool.to_account_info(),
                    c.authority_quote_ata.to_account_info(),
                    c.authority_base_ata.to_account_info(),
                    a.base_vault.to_account_info(),
                    a.quote_vault.to_account_info(),
                    c.base_mint.to_account_info(),
                    c.quote_mint.to_account_info(),
                    c.authority.to_account_info(),
                    c.token_program.to_account_info(),
                    c.quote_token_program.to_account_info(),
                    a.dbc_event_authority.to_account_info(),
                    a.dbc_program.to_account_info(),
                ],
                signer,
            )?;
            if c.vault.quote_is_sol {
                close_wsol(&c.authority.to_account_info(), &c.authority_quote_ata.to_account_info(), &c.quote_token_program.to_account_info(), signer)?;
            }
        }
        let result = settle(&mut ctx.accounts.common, plan, fee_tokens, now, signer)?;
        emit!(result);
        Ok(())
    }

    /// Same as `execute_curve`, but buys on the DAMM v2 pool Meteora created at graduation.
    pub fn execute_amm(ctx: Context<ExecuteAmm>, min_tokens_out: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let a = &ctx.accounts;
        let c = &a.common;
        let (sqrt_now, base_is_a) = check_amm_pool(&a.amm_pool, c.vault.base_mint, c.vault.quote_mint)?;
        // sqrt price is token B per token A. Buying the base raises it when base is A and lowers it when base is B.
        check_reference(&c.vault, VENUE_AMM, sqrt_now, base_is_a, c.caller.key())?;
        let plan = plan_run(&c.vault, run_balance(c), now, c.caller.key())?;

        let pool_key = c.vault.pool;
        let bump = [c.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let fee_tokens = c.authority_base_ata.amount;

        if plan.buy > 0 {
            let min_out = required_min_out(plan.buy, c.vault.ref_sqrt_price, base_is_a, min_tokens_out)?;
            if c.vault.quote_is_sol {
                fund_wsol(c, plan.buy, signer)?;
            }
            let (mint_a, mint_b) = if base_is_a { (c.base_mint.key(), c.quote_mint.key()) } else { (c.quote_mint.key(), c.base_mint.key()) };
            let (prog_a, prog_b) = if base_is_a { (c.token_program.key(), c.quote_token_program.key()) } else { (c.quote_token_program.key(), c.token_program.key()) };
            let ix = Instruction {
                program_id: DAMM_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(DAMM_POOL_AUTHORITY, false),
                    AccountMeta::new(a.amm_pool.key(), false),
                    AccountMeta::new(c.authority_quote_ata.key(), false),
                    AccountMeta::new(c.authority_base_ata.key(), false),
                    AccountMeta::new(a.token_a_vault.key(), false),
                    AccountMeta::new(a.token_b_vault.key(), false),
                    AccountMeta::new_readonly(mint_a, false),
                    AccountMeta::new_readonly(mint_b, false),
                    AccountMeta::new_readonly(c.authority.key(), true),
                    AccountMeta::new_readonly(prog_a, false),
                    AccountMeta::new_readonly(prog_b, false),
                    AccountMeta::new_readonly(DAMM_PROGRAM_ID, false), // no referral account
                    AccountMeta::new_readonly(a.damm_event_authority.key(), false),
                    AccountMeta::new_readonly(DAMM_PROGRAM_ID, false),
                ],
                data: ix_data_two_u64(IX_SWAP, plan.buy, min_out),
            };
            invoke_signed(
                &ix,
                &[
                    a.damm_pool_authority.to_account_info(),
                    a.amm_pool.to_account_info(),
                    c.authority_quote_ata.to_account_info(),
                    c.authority_base_ata.to_account_info(),
                    a.token_a_vault.to_account_info(),
                    a.token_b_vault.to_account_info(),
                    c.base_mint.to_account_info(),
                    c.quote_mint.to_account_info(),
                    c.authority.to_account_info(),
                    c.token_program.to_account_info(),
                    c.quote_token_program.to_account_info(),
                    a.damm_event_authority.to_account_info(),
                    a.damm_program.to_account_info(),
                ],
                signer,
            )?;
            if c.vault.quote_is_sol {
                close_wsol(&c.authority.to_account_info(), &c.authority_quote_ata.to_account_info(), &c.quote_token_program.to_account_info(), signer)?;
            }
        }
        let result = settle(&mut ctx.accounts.common, plan, fee_tokens, now, signer)?;
        emit!(result);
        Ok(())
    }

    /// Vaults with a 0% buyback share: nothing is bought, so there is no price check. Same run rules
    /// (threshold or 7-day timer, 10-minute gap, keeper first then anyone after 8 days).
    pub fn distribute(ctx: Context<Distribute>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let c = &ctx.accounts.common;
        require!(c.vault.buyback_bps == 0, BuybackError::UseExecute);
        let plan = plan_run(&c.vault, run_balance(c), now, c.caller.key())?;
        let pool_key = c.vault.pool;
        let bump = [c.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let fee_tokens = c.authority_base_ata.amount;
        let result = settle(&mut ctx.accounts.common, plan, fee_tokens, now, signer)?;
        emit!(result);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Run rules (pure functions, unit-tested below)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Spent on the buyback this run.
    pub buy: u64,
    /// Donation share produced by this run (paid now, or kept as owed if it is below the minimum payout).
    pub donate: u64,
    /// Creator share produced by this run (paid now, or kept as owed if it is below the minimum payout).
    pub pay: u64,
}

/// `mu` thousandths of a unit.
pub fn mu(unit: u64, milli: u64) -> u64 {
    ((unit as u128) * (milli as u128) / 1_000).min(u64::MAX as u128) as u64
}

/// Buyback and donation are each 0-100% and together at most 100%, and at least one of them is on.
pub fn shares_ok(buyback_bps: u16, donation_bps: u16) -> bool {
    (buyback_bps as u32) + (donation_bps as u32) <= MAX_SHARE_BPS as u32 && (buyback_bps > 0 || donation_bps > 0)
}

/// Who may prepare/run now: the keeper whenever the rules allow, anyone once PUBLIC_RUN_DELAY has passed.
pub fn caller_allowed(caller_is_keeper: bool, last_run: i64, now: i64) -> bool {
    caller_is_keeper || now.saturating_sub(last_run) >= PUBLIC_RUN_DELAY
}

/// How much of `splittable` this run buys back, donates and pays the creator.
/// `spent_today` is what the vault already spent on buybacks in the current 24-hour window.
#[allow(clippy::too_many_arguments)]
pub fn plan_amounts(splittable: u64, buyback_bps: u16, donation_bps: u16, threshold: u64, unit: u64, last_run: i64, now: i64, spent_today: u64) -> Result<Plan> {
    let waited = now.saturating_sub(last_run);
    require!(waited >= MIN_RUN_GAP, BuybackError::TooSoon);
    let ready = splittable >= threshold || (waited >= TIMER_SECONDS && splittable >= mu(unit, MIN_TIMER_RUN_MU));
    require!(ready, BuybackError::NotReady);

    let used = if buyback_bps > 0 {
        let buy_cap = mu(unit, MAX_BUY_PER_RUN_MU).min(mu(unit, MAX_BUY_PER_DAY_MU).saturating_sub(spent_today));
        // A leftover allowance too small for a real buy waits for the next window instead of making a dust run.
        require!(buy_cap >= mu(unit, MIN_BUY_MU), BuybackError::DailyCapReached);
        // Process at most the amount whose buyback share equals buy_cap; the rest waits for later runs.
        splittable.min(((buy_cap as u128) * 10_000 / (buyback_bps as u128)).min(u64::MAX as u128) as u64)
    } else {
        splittable
    };
    let buy = ((used as u128) * (buyback_bps as u128) / 10_000) as u64;
    let donate = ((used as u128) * (donation_bps as u128) / 10_000) as u64;
    Ok(Plan { buy, donate, pay: used - buy - donate })
}

/// Spend already recorded in the current 24-hour window. The window is fixed, not rolling: it starts at the
/// first run after the previous window ended and lasts 24 hours. Around a window boundary two windows' limits
/// can therefore be used close together (at most 2 x MAX_BUY_PER_DAY within 24 hours, at 1 unit per 10 minutes).
pub fn spent_in_window(day_start: i64, day_spent: u64, now: i64) -> u64 {
    if now.saturating_sub(day_start) >= 24 * 60 * 60 { 0 } else { day_spent }
}

/// Price check between `prepare_*` and the buy. `higher_is_worse` is true when the buy pushes sqrt price up.
pub fn price_ok(reference: u128, now_sqrt: u128, higher_is_worse: bool) -> bool {
    if higher_is_worse {
        now_sqrt.saturating_mul(10_000) <= reference.saturating_mul(10_000 + MAX_SQRT_PRICE_MOVE_BPS)
    } else {
        now_sqrt.saturating_mul(10_000) >= reference.saturating_mul(10_000 - MAX_SQRT_PRICE_MOVE_BPS)
    }
}

/// A reference is live from its slot until MAX_PREPARE_SLOTS later.
pub fn reference_live(ref_slot: u64, slot: u64) -> bool {
    ref_slot > 0 && slot <= ref_slot.saturating_add(MAX_PREPARE_SLOTS)
}

/// Tokens the prepared price gives for `amount_in` of the pair asset with no price impact and no fee.
/// `quote_per_token` is true when sqrt_price^2 is pair asset per token (bonding curve; DAMM v2 with the token as A).
pub fn spot_tokens_out(amount_in: u64, sqrt: u128, quote_per_token: bool) -> u64 {
    if sqrt == 0 {
        return 0;
    }
    const Q64: u128 = 1u128 << 64;
    let v = if quote_per_token {
        // amount * 2^128 / sqrt^2, in two steps so nothing overflows for real prices
        (amount_in as u128).checked_mul(Q64).map(|x| x / sqrt).and_then(|x| x.checked_mul(Q64)).map(|x| x / sqrt)
    } else {
        // amount * sqrt^2 / 2^128
        (amount_in as u128).checked_mul(sqrt).map(|x| x / Q64).and_then(|x| x.checked_mul(sqrt)).map(|x| x / Q64)
    };
    match v {
        Some(x) => x.min(u64::MAX as u128) as u64,
        None => u64::MAX,
    }
}

/// Minimum tokens the swap must return: the on-chain floor, or more if the caller asks for more.
pub fn required_min_out(amount_in: u64, ref_sqrt: u128, quote_per_token: bool, asked: u64) -> Result<u64> {
    let floor = ((spot_tokens_out(amount_in, ref_sqrt, quote_per_token) as u128) * MIN_OUT_BPS / 10_000) as u64;
    require!(floor > 0, BuybackError::MinimumOutRequired);
    Ok(floor.max(asked))
}

/// Pair asset available to split: everything held minus what is already owed (owed amounts are not split again).
pub fn splittable_of(balance: u64, creator_owed: u64, donee_owed: u64) -> u64 {
    balance.saturating_sub(creator_owed).saturating_sub(donee_owed)
}

fn plan_run(vault: &Vault, balance: u64, now: i64, caller: Pubkey) -> Result<Plan> {
    require!(caller_allowed(caller == KEEPER, vault.last_run, now), BuybackError::KeeperWindow);
    let splittable = splittable_of(balance, vault.creator_owed, vault.donee_owed);
    plan_amounts(
        splittable,
        vault.buyback_bps,
        vault.donation_bps,
        vault.threshold,
        vault.unit,
        vault.last_run,
        now,
        spent_in_window(vault.day_start, vault.day_spent, now),
    )
}

/// Rules for `prepare_*`: allowed caller, a run that is ready now, and no replacing someone else's live reference
/// (only the keeper may replace a live reference, so a stranger cannot keep cancelling the keeper's runs).
fn check_prepare(vault: &Vault, balance: u64, caller: Pubkey, clock: Clock) -> Result<()> {
    require!(caller == KEEPER || !reference_live(vault.ref_slot, clock.slot), BuybackError::AlreadyPrepared);
    plan_run(vault, balance, clock.unix_timestamp, caller)?;
    Ok(())
}

fn record_reference(vault: &mut Vault, venue: u8, sqrt: u128, caller: Pubkey) -> Result<()> {
    vault.ref_venue = venue;
    vault.ref_sqrt_price = sqrt;
    vault.ref_slot = Clock::get()?.slot;
    vault.ref_caller = caller;
    Ok(())
}

fn check_reference(vault: &Vault, venue: u8, now_sqrt: u128, higher_is_worse: bool, caller: Pubkey) -> Result<()> {
    let slot = Clock::get()?.slot;
    require!(vault.ref_venue == venue && vault.ref_slot > 0, BuybackError::NotPrepared);
    require_keys_eq!(caller, vault.ref_caller, BuybackError::NotYourPrepare);
    require!(slot >= vault.ref_slot + MIN_PREPARE_SLOTS && slot <= vault.ref_slot + MAX_PREPARE_SLOTS, BuybackError::NotPrepared);
    require!(price_ok(vault.ref_sqrt_price, now_sqrt, higher_is_worse), BuybackError::PriceMoved);
    Ok(())
}

fn record_claim(vault: &mut Vault, source: u8, amount: u64) {
    if amount == 0 {
        return;
    }
    vault.total_claimed = vault.total_claimed.saturating_add(amount);
    emit!(FeesClaimed { pool: vault.pool, source, amount });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&data[offset..offset + 32]);
    Pubkey::new_from_array(bytes)
}

fn read_u128(data: &[u8], offset: usize) -> u128 {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&data[offset..offset + 16]);
    u128::from_le_bytes(bytes)
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn ix_data_two_u64(disc: [u8; 8], a: u64, b: u64) -> Vec<u8> {
    let mut d = disc.to_vec();
    d.extend_from_slice(&a.to_le_bytes());
    d.extend_from_slice(&b.to_le_bytes());
    d
}

/// Amount held in a token account (Token or Token-2022), 0 if the account does not exist yet.
fn token_amount(account: &AccountInfo) -> u64 {
    match account.try_borrow_data() {
        Ok(data) if data.len() >= TOKEN_ACCOUNT_AMOUNT_OFFSET + 8 => read_u64(&data, TOKEN_ACCOUNT_AMOUNT_OFFSET),
        _ => 0,
    }
}

/// Pair asset the vault holds. SOL coins: the authority's SOL minus its reserve. Stock-pair coins: the
/// authority's stock-token account (its address is fixed by the account constraints).
fn income_balance(quote_is_sol: bool, authority: &AccountInfo, authority_quote_ata: &AccountInfo) -> u64 {
    if quote_is_sol {
        authority.lamports().saturating_sub(AUTHORITY_RESERVE)
    } else {
        token_amount(authority_quote_ata)
    }
}

fn run_balance(c: &RunAccounts) -> u64 {
    income_balance(c.vault.quote_is_sol, &c.authority.to_account_info(), &c.authority_quote_ata.to_account_info())
}

/// Borrow a Meteora bonding-curve pool's data after checking owner, discriminator and length.
fn curve_pool_data<'a>(pool: &'a AccountInfo) -> Result<std::cell::Ref<'a, &'a mut [u8]>> {
    require_keys_eq!(*pool.owner, DBC_PROGRAM_ID, BuybackError::NotABondingCurvePool);
    let data = pool.try_borrow_data()?;
    require!(data.len() > POOL_MIGRATION_PROGRESS_OFFSET && data[..8] == VIRTUAL_POOL_DISCRIMINATOR, BuybackError::NotABondingCurvePool);
    Ok(data)
}

/// The DAMM v2 pool Meteora creates at graduation: [b"pool", DAMM_MIGRATION_CONFIG, larger mint, smaller mint].
pub fn graduated_pool_address(base_mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    let (first, second) = if base_mint.to_bytes() > quote_mint.to_bytes() { (*base_mint, *quote_mint) } else { (*quote_mint, *base_mint) };
    Pubkey::find_program_address(&[b"pool", DAMM_MIGRATION_CONFIG.as_ref(), first.as_ref(), second.as_ref()], &DAMM_PROGRAM_ID).0
}

/// Check that `pool` is the graduated pool of the vault's token and return (sqrt price, token is A).
fn check_amm_pool(pool: &AccountInfo, base_mint: Pubkey, quote_mint: Pubkey) -> Result<(u128, bool)> {
    require_keys_eq!(pool.key(), graduated_pool_address(&base_mint, &quote_mint), BuybackError::WrongAmmPool);
    require_keys_eq!(*pool.owner, DAMM_PROGRAM_ID, BuybackError::WrongAmmPool);
    let data = pool.try_borrow_data()?;
    require!(data.len() >= DAMM_POOL_SQRT_PRICE_OFFSET + 16 && data[..8] == DAMM_POOL_DISCRIMINATOR, BuybackError::WrongAmmPool);
    let (mint_a, mint_b) = (read_pubkey(&data, DAMM_POOL_TOKEN_A_MINT_OFFSET), read_pubkey(&data, DAMM_POOL_TOKEN_B_MINT_OFFSET));
    let base_is_a = if mint_a == base_mint && mint_b == quote_mint {
        true
    } else if mint_a == quote_mint && mint_b == base_mint {
        false
    } else {
        return err!(BuybackError::WrongAmmPool);
    };
    Ok((read_u128(&data, DAMM_POOL_SQRT_PRICE_OFFSET), base_is_a))
}

/// Create an associated token account if it does not exist yet.
#[allow(clippy::too_many_arguments)]
fn open_ata<'info>(
    payer: AccountInfo<'info>,
    ata: AccountInfo<'info>,
    owner: AccountInfo<'info>,
    mint: AccountInfo<'info>,
    token_program: AccountInfo<'info>,
    ata_program: AccountInfo<'info>,
    system: AccountInfo<'info>,
    signer: &[&[&[u8]]],
) -> Result<()> {
    associated_token::create_idempotent(CpiContext::new_with_signer(
        ata_program,
        Create { payer, associated_token: ata, authority: owner, mint, system_program: system, token_program },
        signer,
    ))
}

/// SOL coins: create the authority's temporary wSOL account (the authority pays the rent from its reserve).
fn open_wsol_for<'info>(
    authority: &SystemAccount<'info>,
    wsol: &UncheckedAccount<'info>,
    wsol_mint: &InterfaceAccount<'info, AnyMint>,
    token_program: &Interface<'info, TokenInterface>,
    ata_program: &Program<'info, AssociatedToken>,
    system: &Program<'info, System>,
    signer: &[&[&[u8]]],
) -> Result<()> {
    open_ata(
        authority.to_account_info(),
        wsol.to_account_info(),
        authority.to_account_info(),
        wsol_mint.to_account_info(),
        token_program.to_account_info(),
        ata_program.to_account_info(),
        system.to_account_info(),
        signer,
    )
}

/// SOL coins: close the temporary wSOL account. All its lamports (rent + wSOL) return to the authority as plain SOL.
fn close_wsol<'info>(authority: &AccountInfo<'info>, wsol: &AccountInfo<'info>, token_program: &AccountInfo<'info>, signer: &[&[&[u8]]]) -> Result<()> {
    token::close_account(CpiContext::new_with_signer(
        token_program.clone(),
        CloseAccount { account: wsol.clone(), destination: authority.clone(), authority: authority.clone() },
        signer,
    ))
}

/// SOL coins: put exactly `lamports` of the authority's SOL into its temporary wSOL account, ready to swap.
fn fund_wsol(c: &RunAccounts<'_>, lamports: u64, signer: &[&[&[u8]]]) -> Result<()> {
    open_wsol_for(&c.authority, &c.authority_quote_ata, &c.quote_mint, &c.quote_token_program, &c.associated_token_program, &c.system_program, signer)?;
    system_program::transfer(
        CpiContext::new_with_signer(c.system_program.to_account_info(), SolTransfer { from: c.authority.to_account_info(), to: c.authority_quote_ata.to_account_info() }, signer),
        lamports,
    )?;
    token::sync_native(CpiContext::new(c.quote_token_program.to_account_info(), SyncNative { account: c.authority_quote_ata.to_account_info() }))
}

/// Pay `amount` of the pair asset to `wallet`: plain SOL for SOL coins, the stock token (into the wallet's
/// token account, created if needed and paid by the caller) for stock-pair coins.
fn pay_quote<'info>(c: &RunAccounts<'info>, wallet: &AccountInfo<'info>, wallet_ata: &AccountInfo<'info>, amount: u64, signer: &[&[&[u8]]]) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    if c.vault.quote_is_sol {
        return system_program::transfer(
            CpiContext::new_with_signer(c.system_program.to_account_info(), SolTransfer { from: c.authority.to_account_info(), to: wallet.clone() }, signer),
            amount,
        );
    }
    open_ata(
        c.caller.to_account_info(),
        wallet_ata.clone(),
        wallet.clone(),
        c.quote_mint.to_account_info(),
        c.quote_token_program.to_account_info(),
        c.associated_token_program.to_account_info(),
        c.system_program.to_account_info(),
        &[],
    )?;
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            c.quote_token_program.to_account_info(),
            TransferChecked {
                from: c.authority_quote_ata.to_account_info(),
                mint: c.quote_mint.to_account_info(),
                to: wallet_ata.clone(),
                authority: c.authority.to_account_info(),
            },
            signer,
        ),
        amount,
        c.quote_mint.decimals,
    )
}

/// Send `amount` of the token itself to `wallet`'s token account (created if needed, paid by the caller).
fn pay_tokens<'info>(c: &RunAccounts<'info>, wallet: &AccountInfo<'info>, wallet_ata: &AccountInfo<'info>, amount: u64, signer: &[&[&[u8]]]) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    open_ata(
        c.caller.to_account_info(),
        wallet_ata.clone(),
        wallet.clone(),
        c.base_mint.to_account_info(),
        c.token_program.to_account_info(),
        c.associated_token_program.to_account_info(),
        c.system_program.to_account_info(),
        &[],
    )?;
    token::transfer(
        CpiContext::new_with_signer(
            c.token_program.to_account_info(),
            Transfer { from: c.authority_base_ata.to_account_info(), to: wallet_ata.clone(), authority: c.authority.to_account_info() },
            signer,
        ),
        amount,
    )
}

/// After the buy: burn every bought token, split token-denominated fees by the same shares, pay the donation
/// wallet and the creator, record the run.
fn settle(c: &mut RunAccounts<'_>, plan: Plan, fee_tokens_before: u64, now: i64, signer: &[&[&[u8]]]) -> Result<RunExecuted> {
    c.authority_base_ata.reload()?;
    let bought = c.authority_base_ata.amount.saturating_sub(fee_tokens_before);
    let bb = c.vault.buyback_bps as u128;
    let db = c.vault.donation_bps as u128;
    let fee_burn = ((fee_tokens_before as u128) * bb / 10_000) as u64;
    let fee_to_donee = ((fee_tokens_before as u128) * db / 10_000) as u64;
    let fee_to_creator = fee_tokens_before - fee_burn - fee_to_donee;
    let burned = bought + fee_burn;

    if burned > 0 {
        token::burn(
            CpiContext::new_with_signer(
                c.token_program.to_account_info(),
                Burn { mint: c.base_mint.to_account_info(), from: c.authority_base_ata.to_account_info(), authority: c.authority.to_account_info() },
                signer,
            ),
            burned,
        )?;
    }
    pay_tokens(c, &c.donee.to_account_info(), &c.donee_base_ata.to_account_info(), fee_to_donee, signer)?;
    pay_tokens(c, &c.creator.to_account_info(), &c.creator_base_ata.to_account_info(), fee_to_creator, signer)?;

    // Pair asset: pay this run's shares plus anything owed, unless a total is still below the minimum payout.
    let min_payout = mu(c.vault.unit, MIN_PAYOUT_MU).max(1);
    let donee_due = c.vault.donee_owed.saturating_add(plan.donate);
    let donated = if donee_due >= min_payout { donee_due } else { 0 };
    let creator_due = c.vault.creator_owed.saturating_add(plan.pay);
    let paid = if creator_due >= min_payout { creator_due } else { 0 };
    pay_quote(c, &c.donee.to_account_info(), &c.donee_quote_ata.to_account_info(), donated, signer)?;
    pay_quote(c, &c.creator.to_account_info(), &c.creator_quote_ata.to_account_info(), paid, signer)?;

    let caller = c.caller.key();
    let v = &mut c.vault;
    v.donee_owed = donee_due - donated;
    v.creator_owed = creator_due - paid;
    if now.saturating_sub(v.day_start) >= 24 * 60 * 60 {
        v.day_start = now;
        v.day_spent = 0;
    }
    v.day_spent = v.day_spent.saturating_add(plan.buy);
    v.ref_slot = 0; // every run needs a fresh prepare
    v.runs = v.runs.saturating_add(1);
    v.last_run = now;
    v.total_spent = v.total_spent.saturating_add(plan.buy);
    v.total_paid = v.total_paid.saturating_add(paid);
    v.total_donated = v.total_donated.saturating_add(donated);
    v.total_burned = v.total_burned.saturating_add(burned);
    v.total_fee_tokens_to_creator = v.total_fee_tokens_to_creator.saturating_add(fee_to_creator);
    v.total_fee_tokens_to_donee = v.total_fee_tokens_to_donee.saturating_add(fee_to_donee);
    Ok(RunExecuted {
        pool: v.pool,
        caller,
        run: v.runs,
        spent: plan.buy,
        bought,
        burned,
        paid,
        donated,
        creator_owed: v.creator_owed,
        donee_owed: v.donee_owed,
        fee_tokens_to_creator: fee_to_creator,
        fee_tokens_to_donee: fee_to_donee,
    })
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One per pool. The rule fields are written only by `enable`; no instruction changes them later.
/// Amounts are in the pool's pair asset (lamports for SOL coins, stock-token base units for stock pairs).
#[account]
#[derive(InitSpace)]
pub struct Vault {
    // --- rules, fixed forever ---
    pub pool: Pubkey,
    pub config: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub quote_token_program: Pubkey,
    pub quote_is_sol: bool,
    pub creator: Pubkey,
    /// Donation wallet; Pubkey::default() when there is no donation.
    pub donee: Pubkey,
    pub buyback_bps: u16,
    pub donation_bps: u16,
    pub threshold: u64,
    /// 1/85 of the config's graduation amount (1 SOL for SOL coins).
    pub unit: u64,
    pub created_at: i64,
    // --- run bookkeeping ---
    pub last_run: i64,
    pub day_start: i64,
    pub day_spent: u64,
    pub ref_venue: u8,
    pub ref_slot: u64,
    pub ref_sqrt_price: u128,
    /// Who called `prepare_*`; only the same wallet can run the buy.
    pub ref_caller: Pubkey,
    /// Produced by runs but not yet paid because the total was below the minimum payout.
    pub creator_owed: u64,
    pub donee_owed: u64,
    // --- running totals ---
    pub runs: u64,
    pub total_claimed: u64,
    pub total_spent: u64,
    pub total_paid: u64,
    pub total_donated: u64,
    pub total_burned: u64,
    pub total_fee_tokens_to_creator: u64,
    pub total_fee_tokens_to_donee: u64,
    pub bump: u8,
    pub authority_bump: u8,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct Enable<'info> {
    /// The pool's current creator. Pays for the vault, its token accounts and the 0.005 SOL reserve.
    #[account(mut)]
    pub creator: Signer<'info>,
    /// CHECK: Meteora bonding-curve pool; owner, layout, config, creator, mint and stage are checked in `enable`.
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the pool's Meteora config; owner, layout, fee claimer and pair asset are checked in `enable`.
    pub config: UncheckedAccount<'info>,
    pub base_mint: Box<Account<'info, Mint>>,
    /// The pool's pair asset (wSOL for SOL coins, the stock token for stock pairs). Checked against the config.
    pub quote_mint: Box<InterfaceAccount<'info, AnyMint>>,
    #[account(init, payer = creator, space = 8 + Vault::INIT_SPACE, seeds = [VAULT_SEED, pool.key().as_ref()], bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// Program-controlled wallet that becomes the pool creator. No private key exists for it.
    #[account(mut, seeds = [AUTHORITY_SEED, pool.key().as_ref()], bump)]
    pub authority: SystemAccount<'info>,
    #[account(init_if_needed, payer = creator, associated_token::mint = base_mint, associated_token::authority = authority)]
    pub authority_base_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: the authority's pair-asset account address (stock pairs: created here).
    #[account(mut, address = get_associated_token_address_with_program_id(&authority.key(), &quote_mint.key(), &quote_token_program.key()))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the creator's pair-asset account address (stock pairs: created here).
    #[account(mut, address = get_associated_token_address_with_program_id(&creator.key(), &quote_mint.key(), &quote_token_program.key()))]
    pub creator_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the donation wallet (any wallet). Ignored when donation_bps is 0; must not be a program account.
    pub donee: UncheckedAccount<'info>,
    /// CHECK: the donation wallet's pair-asset account address (stock pairs with a donation: created here;
    /// pass it writable in that case).
    #[account(address = get_associated_token_address_with_program_id(&donee.key(), &quote_mint.key(), &quote_token_program.key()))]
    pub donee_quote_ata: UncheckedAccount<'info>,
    /// CHECK: Meteora event authority; Meteora verifies it.
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: Meteora bonding-curve program.
    #[account(address = DBC_PROGRAM_ID)]
    pub dbc_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

/// Used by `claim_curve_fees` and `claim_curve_surplus`.
#[derive(Accounts)]
pub struct ClaimCurve<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [AUTHORITY_SEED, vault.pool.as_ref()], bump = vault.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: must be the vault's pool; Meteora checks the rest.
    #[account(mut, address = vault.pool)]
    pub pool: UncheckedAccount<'info>,
    #[account(mut, associated_token::mint = base_mint, associated_token::authority = authority)]
    pub authority_base_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: the authority's pair-asset account (SOL coins: temporary wSOL, created and closed inside).
    #[account(mut, address = get_associated_token_address_with_program_id(&authority.key(), &vault.quote_mint, &vault.quote_token_program))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: Meteora checks it matches the pool.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: Meteora checks it matches the pool.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    #[account(address = vault.base_mint)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(address = vault.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, AnyMint>>,
    /// CHECK: the pool's config saved in `enable`.
    #[account(address = vault.config)]
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: fixed Meteora address.
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: Meteora verifies it.
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: Meteora bonding-curve program.
    #[account(address = DBC_PROGRAM_ID)]
    pub dbc_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    #[account(address = vault.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimAmmFees<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [AUTHORITY_SEED, vault.pool.as_ref()], bump = vault.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: the vault's bonding-curve pool; must be graduated (checked in the handler).
    #[account(address = vault.pool)]
    pub curve_pool: UncheckedAccount<'info>,
    /// CHECK: must be the graduated DAMM v2 pool address derived from the mints (checked in the handler).
    pub amm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 position; owner, layout, pool and NFT are checked in the handler.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// The position NFT, which must be held by the authority.
    pub position_nft_account: Box<InterfaceAccount<'info, AnyTokenAccount>>,
    #[account(mut, associated_token::mint = base_mint, associated_token::authority = authority)]
    pub authority_base_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: the authority's pair-asset account (SOL coins: temporary wSOL, created and closed inside).
    #[account(mut, address = get_associated_token_address_with_program_id(&authority.key(), &vault.quote_mint, &vault.quote_token_program))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it matches the pool.
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it matches the pool.
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    #[account(address = vault.base_mint)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(address = vault.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, AnyMint>>,
    /// CHECK: fixed Meteora address.
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 verifies it.
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 program.
    #[account(address = DAMM_PROGRAM_ID)]
    pub damm_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    #[account(address = vault.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PrepareCurve<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// The vault's authority; with its pair-asset account it decides whether a run is ready.
    #[account(seeds = [AUTHORITY_SEED, vault.pool.as_ref()], bump = vault.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: the authority's pair-asset account address (read only).
    #[account(address = get_associated_token_address_with_program_id(&authority.key(), &vault.quote_mint, &vault.quote_token_program))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the vault's bonding-curve pool (address checked; layout checked in the handler).
    #[account(address = vault.pool)]
    pub curve_pool: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct PrepareAmm<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// The vault's authority; with its pair-asset account it decides whether a run is ready.
    #[account(seeds = [AUTHORITY_SEED, vault.pool.as_ref()], bump = vault.authority_bump)]
    pub authority: SystemAccount<'info>,
    /// CHECK: the authority's pair-asset account address (read only).
    #[account(address = get_associated_token_address_with_program_id(&authority.key(), &vault.quote_mint, &vault.quote_token_program))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: must be the graduated DAMM v2 pool address derived from the mints (checked in the handler).
    pub amm_pool: UncheckedAccount<'info>,
}

/// Accounts shared by every run. The creator and donation wallets and their token accounts are pinned to the vault.
/// Pass the donation wallet accounts writable when the vault has a donation share.
#[derive(Accounts)]
pub struct RunAccounts<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [AUTHORITY_SEED, vault.pool.as_ref()], bump = vault.authority_bump)]
    pub authority: SystemAccount<'info>,
    #[account(mut, associated_token::mint = base_mint, associated_token::authority = authority)]
    pub authority_base_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: the authority's pair-asset account (SOL coins: temporary wSOL, created and closed inside).
    #[account(mut, address = get_associated_token_address_with_program_id(&authority.key(), &vault.quote_mint, &vault.quote_token_program))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the creator wallet saved in `enable` (address checked).
    #[account(mut, address = vault.creator)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the creator's token account for the token (address checked); created if needed.
    #[account(mut, address = get_associated_token_address(&vault.creator, &vault.base_mint))]
    pub creator_base_ata: UncheckedAccount<'info>,
    /// CHECK: the creator's pair-asset account (address checked; used for stock pairs).
    #[account(mut, address = get_associated_token_address_with_program_id(&vault.creator, &vault.quote_mint, &vault.quote_token_program))]
    pub creator_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the donation wallet saved in `enable` (address checked).
    #[account(address = vault.donee)]
    pub donee: UncheckedAccount<'info>,
    /// CHECK: the donation wallet's token account for the token (address checked); created if needed.
    #[account(address = get_associated_token_address(&vault.donee, &vault.base_mint))]
    pub donee_base_ata: UncheckedAccount<'info>,
    /// CHECK: the donation wallet's pair-asset account (address checked; used for stock pairs).
    #[account(address = get_associated_token_address_with_program_id(&vault.donee, &vault.quote_mint, &vault.quote_token_program))]
    pub donee_quote_ata: UncheckedAccount<'info>,
    #[account(mut, address = vault.base_mint)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(address = vault.quote_mint)]
    pub quote_mint: Box<InterfaceAccount<'info, AnyMint>>,
    pub token_program: Program<'info, Token>,
    #[account(address = vault.quote_token_program)]
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExecuteCurve<'info> {
    pub common: RunAccounts<'info>,
    /// CHECK: must be the vault's pool (checked in the handler).
    #[account(mut)]
    pub venue_pool: UncheckedAccount<'info>,
    /// CHECK: Meteora checks it matches the pool.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: Meteora checks it matches the pool.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: must be the vault's config (checked in the handler).
    pub dbc_config: UncheckedAccount<'info>,
    /// CHECK: fixed Meteora address.
    #[account(address = DBC_POOL_AUTHORITY)]
    pub dbc_pool_authority: UncheckedAccount<'info>,
    /// CHECK: Meteora verifies it.
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: Meteora bonding-curve program.
    #[account(address = DBC_PROGRAM_ID)]
    pub dbc_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct ExecuteAmm<'info> {
    pub common: RunAccounts<'info>,
    /// CHECK: must be the graduated DAMM v2 pool address derived from the mints (checked in the handler).
    #[account(mut)]
    pub amm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it matches the pool.
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it matches the pool.
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    /// CHECK: fixed Meteora address.
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 verifies it.
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 program.
    #[account(address = DAMM_PROGRAM_ID)]
    pub damm_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct Distribute<'info> {
    pub common: RunAccounts<'info>,
}

// ---------------------------------------------------------------------------
// Events and errors
// ---------------------------------------------------------------------------

#[event]
pub struct BuybackEnabled {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub creator: Pubkey,
    pub donee: Pubkey,
    pub buyback_bps: u16,
    pub donation_bps: u16,
    pub threshold: u64,
    pub unit: u64,
}

#[event]
pub struct FeesClaimed {
    pub pool: Pubkey,
    /// 0 = curve trading fees, 1 = curve surplus, 2 = DAMM v2 LP fees
    pub source: u8,
    pub amount: u64,
}

#[event]
pub struct RunExecuted {
    pub pool: Pubkey,
    pub caller: Pubkey,
    pub run: u64,
    pub spent: u64,
    pub bought: u64,
    pub burned: u64,
    pub paid: u64,
    pub donated: u64,
    pub creator_owed: u64,
    pub donee_owed: u64,
    pub fee_tokens_to_creator: u64,
    pub fee_tokens_to_donee: u64,
}

#[error_code]
pub enum BuybackError {
    #[msg("Buyback and donation shares must each be 0-100%, together at most 100%, and at least one above 0.")]
    SharesOutOfRange,
    #[msg("Run threshold must be between 0.1 and 10 units (SOL coins: 0.1 to 10 SOL).")]
    ThresholdOutOfRange,
    #[msg("During probation only CREATORFUN test wallets can enable.")]
    ProbationOnly,
    #[msg("Not a Meteora bonding-curve pool.")]
    NotABondingCurvePool,
    #[msg("This pool was not launched on CREATORFUN.")]
    NotACreatorfunPool,
    #[msg("Only the current pool creator can enable.")]
    NotThePoolCreator,
    #[msg("Base mint does not match the pool.")]
    WrongBaseMint,
    #[msg("This can only be enabled before the token graduates.")]
    PoolAlreadyGraduating,
    #[msg("The creator transfer did not complete.")]
    CreatorTransferFailed,
    #[msg("This position is not the graduated position held by the buyback authority.")]
    NotOurPosition,
    #[msg("This is not the pool Meteora created when the token graduated.")]
    WrongAmmPool,
    #[msg("The token has not graduated yet.")]
    NotGraduatedYet,
    #[msg("The token has graduated; use the graduated pool.")]
    AlreadyGraduated,
    #[msg("Wrong pool for this vault.")]
    WrongVenue,
    #[msg("Too soon: runs must be at least 10 minutes apart.")]
    TooSoon,
    #[msg("Not ready: below the threshold and the 7-day timer has not passed.")]
    NotReady,
    #[msg("This vault already spent its 24-hour buyback limit.")]
    DailyCapReached,
    #[msg("Only the CREATORFUN keeper can do this now; anyone can after 8 days without a run.")]
    KeeperWindow,
    #[msg("Call prepare first; the buy must follow 25 to 300 slots later.")]
    NotPrepared,
    #[msg("The pool price moved against the buy since prepare. Try again.")]
    PriceMoved,
    #[msg("A minimum token amount is required to protect the buy.")]
    MinimumOutRequired,
    #[msg("Only the wallet that called prepare can run this buy.")]
    NotYourPrepare,
    #[msg("A recent prepare is still live; wait until it expires.")]
    AlreadyPrepared,
    #[msg("The config does not belong to this pool.")]
    WrongConfig,
    #[msg("The pair asset does not match the pool's config.")]
    WrongQuoteMint,
    #[msg("The donation wallet must be a normal wallet address.")]
    BadDonee,
    #[msg("This vault has a 0% buyback share; use distribute.")]
    UseDistribute,
    #[msg("This vault has a buyback share; use prepare and execute.")]
    UseExecute,
}

#[cfg(test)]
mod tests {
    use super::*;
    const DAY: i64 = 24 * 60 * 60;
    const SOL: u64 = 1_000_000_000; // one unit for SOL coins

    fn plan(splittable: u64, bb: u16, db: u16, threshold: u64, last_run: i64, now: i64, spent: u64) -> Result<Plan> {
        plan_amounts(splittable, bb, db, threshold, SOL, last_run, now, spent)
    }

    #[test]
    fn units_for_sol_match_the_old_sol_amounts() {
        assert_eq!(85_000_000_000 / UNITS_PER_GRADUATION, SOL);
        assert_eq!(mu(SOL, MIN_THRESHOLD_MU), 100_000_000);
        assert_eq!(mu(SOL, MAX_THRESHOLD_MU), 10_000_000_000);
        assert_eq!(mu(SOL, MAX_BUY_PER_RUN_MU), 1_000_000_000);
        assert_eq!(mu(SOL, MAX_BUY_PER_DAY_MU), 5_000_000_000);
        assert_eq!(mu(SOL, MIN_BUY_MU), 10_000_000);
        assert_eq!(mu(SOL, MIN_PAYOUT_MU), 1_000_000);
    }

    #[test]
    fn share_rules() {
        assert!(shares_ok(10_000, 0));
        assert!(shares_ok(0, 10_000));
        assert!(shares_ok(3_000, 7_000));
        assert!(shares_ok(1, 0));
        assert!(!shares_ok(0, 0));
        assert!(!shares_ok(5_000, 5_001));
        assert!(!shares_ok(u16::MAX, 0));
    }

    #[test]
    fn below_threshold_and_timer_is_not_ready() {
        assert!(plan(50_000_000, 3_000, 0, 500_000_000, 0, 6 * DAY, 0).is_err());
    }

    #[test]
    fn threshold_reached_splits_by_share() {
        let p = plan(500_000_000, 3_000, 0, 500_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 150_000_000, donate: 0, pay: 350_000_000 });
        let p = plan(500_000_000, 3_000, 2_000, 500_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 150_000_000, donate: 100_000_000, pay: 250_000_000 });
    }

    #[test]
    fn donation_only_has_no_buy_cap() {
        let p = plan(20_000_000_000, 0, 4_000, 100_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 0, donate: 8_000_000_000, pay: 12_000_000_000 });
        // the daily buyback cap does not block a vault that never buys
        let p = plan(1_000_000_000, 0, 10_000, 100_000_000, 0, DAY, MAX_BUY_PER_DAY_MU * SOL).unwrap();
        assert_eq!(p, Plan { buy: 0, donate: 1_000_000_000, pay: 0 });
    }

    #[test]
    fn runs_are_at_least_10_minutes_apart() {
        assert!(plan(500_000_000, 3_000, 0, 100_000_000, 0, 9 * 60, 0).is_err());
        assert!(plan(500_000_000, 3_000, 0, 100_000_000, 0, 10 * 60, 0).is_ok());
        assert!(plan(500_000_000, 0, 3_000, 100_000_000, 0, 9 * 60, 0).is_err());
    }

    #[test]
    fn timer_allows_small_runs_after_7_days() {
        let p = plan(20_000_000, 5_000, 0, 1_000_000_000, 0, 7 * DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 10_000_000, donate: 0, pay: 10_000_000 });
        assert!(plan(5_000_000, 5_000, 0, 1_000_000_000, 0, 7 * DAY, 0).is_err());
    }

    #[test]
    fn buy_is_capped_per_run() {
        let p = plan(5_000_000_000, 10_000, 0, 100_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 1_000_000_000, donate: 0, pay: 0 });
        let p = plan(10_000_000_000, 3_000, 1_000, 100_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p.buy, 999_999_999);
        assert_eq!(p.buy + p.donate + p.pay, 3_333_333_333);
    }

    #[test]
    fn buy_is_capped_per_day() {
        let p = plan(5_000_000_000, 10_000, 0, 100_000_000, 0, DAY, 4_600_000_000).unwrap();
        assert_eq!(p.buy, 400_000_000);
        assert!(plan(5_000_000_000, 10_000, 0, 100_000_000, 0, DAY, 5_000_000_000).is_err());
        assert_eq!(spent_in_window(0, 5_000_000_000, DAY), 0);
        assert_eq!(spent_in_window(0, 5_000_000_000, DAY - 1), 5_000_000_000);
    }

    #[test]
    fn tiny_leftover_allowance_waits() {
        let min_buy = mu(SOL, MIN_BUY_MU);
        assert!(plan(5_000_000_000, 10_000, 0, 100_000_000, 0, DAY, 5_000_000_000 - min_buy + 1).is_err());
        let p = plan(5_000_000_000, 10_000, 0, 100_000_000, 0, DAY, 5_000_000_000 - min_buy).unwrap();
        assert_eq!(p.buy, min_buy);
    }

    #[test]
    fn stock_pair_units_scale_every_limit() {
        // A stock token with 8 decimals whose config graduates at 1.7 tokens: one unit = 0.02 tokens.
        let unit = 170_000_000 / UNITS_PER_GRADUATION;
        assert_eq!(unit, 2_000_000);
        let p = plan_amounts(10 * unit, 10_000, 0, mu(unit, MIN_THRESHOLD_MU), unit, 0, DAY, 0).unwrap();
        assert_eq!(p.buy, mu(unit, MAX_BUY_PER_RUN_MU));
        assert!(plan_amounts(mu(unit, MIN_THRESHOLD_MU) - 1, 5_000, 0, mu(unit, MIN_THRESHOLD_MU), unit, 0, DAY, 0).is_err());
    }

    #[test]
    fn owed_amounts_are_not_split_again() {
        assert_eq!(splittable_of(1_000, 200, 300), 500);
        assert_eq!(splittable_of(100, 200, 300), 0);
    }

    #[test]
    fn public_callers_wait_8_days() {
        assert!(!caller_allowed(false, 0, 7 * DAY));
        assert!(caller_allowed(false, 0, 8 * DAY));
        assert!(caller_allowed(true, 0, 60));
    }

    #[test]
    fn price_check_blocks_pushes_against_the_buy() {
        let r: u128 = 1_000_000_000_000;
        assert!(price_ok(r, r, true));
        assert!(price_ok(r, r * 10_050 / 10_000, true));
        assert!(!price_ok(r, r * 10_051 / 10_000, true));
        assert!(price_ok(r, r / 2, true)); // price fell: buying is cheaper, allowed
        assert!(price_ok(r, r * 9_950 / 10_000, false));
        assert!(!price_ok(r, r * 9_949 / 10_000, false));
    }

    #[test]
    fn nothing_is_lost_in_the_split() {
        for (bb, db) in [(0u16, 1u16), (1, 0), (1_234, 4_321), (3_000, 7_000), (9_999, 1), (10_000, 0), (0, 10_000)] {
            for avail in [10_000_000u64, 123_456_789, 999_999_999, 3_000_000_000] {
                let p = plan(avail, bb, db, MIN_THRESHOLD_MU * SOL / 1_000, 0, 30 * DAY, 0).unwrap();
                assert!(p.buy + p.donate + p.pay <= avail);
                assert!(p.buy <= mu(SOL, MAX_BUY_PER_RUN_MU));
            }
        }
    }

    fn sqrt_q64(price: f64) -> u128 {
        (price.sqrt() * 18_446_744_073_709_551_616f64) as u128
    }

    #[test]
    fn spot_out_matches_the_price_both_ways() {
        // 0.00003 lamports per token atom (1B supply, 6 decimals, ~30 SOL market cap)
        let price = 0.000_03f64;
        let expect = 1_000_000_000f64 / price;
        let a = spot_tokens_out(1_000_000_000, sqrt_q64(price), true) as f64;
        assert!((a - expect).abs() / expect < 1e-6);
        // the same pool quoted the other way round (token per pair asset)
        let b = spot_tokens_out(1_000_000_000, sqrt_q64(1.0 / price), false) as f64;
        assert!((b - expect).abs() / expect < 1e-6);
        // a very expensive token still works
        let c = spot_tokens_out(1_000_000, sqrt_q64(50_000.0), true);
        assert!(c == 19 || c == 20);
        assert_eq!(spot_tokens_out(1_000_000_000, 0, true), 0);
    }

    #[test]
    fn min_out_floor_cannot_be_lowered() {
        let sqrt = sqrt_q64(0.000_03);
        let spot = spot_tokens_out(500_000_000, sqrt, true);
        let floor = ((spot as u128) * MIN_OUT_BPS / 10_000) as u64;
        assert_eq!(required_min_out(500_000_000, sqrt, true, 0).unwrap(), floor);
        assert_eq!(required_min_out(500_000_000, sqrt, true, 1).unwrap(), floor);
        assert_eq!(required_min_out(500_000_000, sqrt, true, floor + 7).unwrap(), floor + 7);
        assert!(required_min_out(500_000_000, 0, true, 5).is_err());
    }

    #[test]
    fn reference_is_live_for_max_prepare_slots() {
        assert!(!reference_live(0, 10));
        assert!(reference_live(100, 100));
        assert!(reference_live(100, 100 + MAX_PREPARE_SLOTS));
        assert!(!reference_live(100, 101 + MAX_PREPARE_SLOTS));
        assert!(MIN_PREPARE_SLOTS >= 20 && MIN_PREPARE_SLOTS < MAX_PREPARE_SLOTS);
    }

    #[cfg(not(feature = "devnet"))]
    #[test]
    fn mainnet_keeper_is_its_own_key() {
        assert_ne!(KEEPER, pubkey!("5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF"));
        assert_ne!(KEEPER, CREATORFUN_FEE_WALLET);
    }

    #[test]
    fn graduated_pool_is_derived_not_chosen() {
        let t1 = pubkey!("CGDFGSBNNKcAdwdV4Q3WvkwgngRsiUSj5oEdAtymZd4Y");
        let t2 = pubkey!("DTmBFBxCNLsZgEsGqWBSVfKj1rrQtZQMT3WuTTGRPMwH");
        let stock = pubkey!("5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF"); // any other pair asset
        assert_eq!(graduated_pool_address(&t1, &native_mint::ID), graduated_pool_address(&t1, &native_mint::ID));
        assert_ne!(graduated_pool_address(&t1, &native_mint::ID), graduated_pool_address(&t2, &native_mint::ID));
        assert_ne!(graduated_pool_address(&t1, &native_mint::ID), graduated_pool_address(&t1, &stock));
    }
}
