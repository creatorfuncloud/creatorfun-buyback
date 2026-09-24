//! CREATORFUN permanent buyback & burn
//!
//! A creator can turn this on ONCE for a CREATORFUN token before it graduates.
//! From that moment the token's creator-fee rights belong to this program, forever.
//!
//! What the program does with the creator's income (and nothing else):
//!   1. Buys the token with `buyback_bps` of it and BURNS every token it buys, in the same transaction.
//!   2. Sends the rest to the creator wallet that turned buyback on.
//!
//! What nobody can do through this program (there is no instruction for it):
//!   - turn buyback off, pause it, or give the creator rights back
//!   - change the buyback share, the run threshold or the creator wallet
//!   - send the income anywhere except "burn" or "the creator wallet"
//!   - withdraw anything as an admin (the program has no admin instruction)
//! The program binary itself can still be upgraded during the probation period. See README "Probation".
//!
//! Instructions:
//!   enable              creator only, once per pool, before graduation
//!   claim_curve_fees    anyone: creator trading fees from the bonding curve -> this program
//!   claim_curve_surplus anyone: creator share of the curve surplus (after the curve completes) -> this program
//!   claim_amm_fees      anyone: creator LP fees from the graduated DAMM v2 pool -> this program
//!   prepare_curve       records the bonding-curve price; a buy must happen 2..150 slots later near that price
//!   prepare_amm         the same for the graduated DAMM v2 pool
//!   execute_curve       buyback on the bonding curve, burn, pay the creator
//!   execute_amm         buyback on the graduated DAMM v2 pool, burn, pay the creator

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    pubkey,
};
use anchor_lang::system_program::{self, Transfer as SolTransfer};
use anchor_spl::associated_token::{self, get_associated_token_address, AssociatedToken, Create};
use anchor_spl::token::{self, spl_token::native_mint, Burn, CloseAccount, Mint, SyncNative, Token, TokenAccount, Transfer};
use anchor_spl::token_interface::TokenAccount as AnyTokenAccount;

declare_id!("eJGfjnQn4Gk7gNvyGNmDYPjBQBu6msUmSUr91fyUq2j");

// ---------------------------------------------------------------------------
// Fixed rules. Compiled into the program; the same for every token.
// ---------------------------------------------------------------------------

/// Buyback share limits, in basis points (100 bps = 1%).
pub const MIN_BUYBACK_BPS: u16 = 500; // 5%
pub const MAX_BUYBACK_BPS: u16 = 10_000; // 100%
/// Run threshold limits, in lamports.
pub const MIN_THRESHOLD: u64 = 100_000_000; // 0.1 SOL
pub const MAX_THRESHOLD: u64 = 10_000_000_000; // 10 SOL
/// If the threshold is not reached, a run is still allowed this long after the last run.
pub const TIMER_SECONDS: i64 = 7 * 24 * 60 * 60; // 7 days
/// Smallest amount a timer run will process.
pub const MIN_TIMER_RUN: u64 = 10_000_000; // 0.01 SOL
/// Minimum time between two runs of the same vault.
pub const MIN_RUN_GAP: i64 = 10 * 60; // 10 minutes
/// Most SOL one run may spend on the buyback.
pub const MAX_BUY_PER_RUN: u64 = 1_000_000_000; // 1 SOL
/// Most SOL one vault may spend on buybacks in any 24-hour window.
pub const MAX_BUY_PER_DAY: u64 = 5_000_000_000; // 5 SOL
/// Anyone may prepare and run a vault once this long has passed since its last run.
/// Before that only the CREATORFUN keeper does (the keeper can trigger runs; it can never receive funds).
pub const PUBLIC_RUN_DELAY: i64 = 8 * 24 * 60 * 60; // 8 days
/// A buy must happen at least / at most this many slots after `prepare_*`.
pub const MIN_PREPARE_SLOTS: u64 = 2;
pub const MAX_PREPARE_SLOTS: u64 = 150; // about one minute
/// How far the pool price may move against the buy between `prepare_*` and the buy (on sqrt price; 50 bps ≈ 1% price).
pub const MAX_SQRT_PRICE_MOVE_BPS: u128 = 50;
/// Creator payouts smaller than this are kept (owed) and paid together with a later run.
pub const MIN_PAYOUT: u64 = 1_000_000; // 0.001 SOL
/// Lamports kept by the authority to pay for its temporary wSOL account. Never spent or paid out.
pub const AUTHORITY_RESERVE: u64 = 5_000_000; // 0.005 SOL, paid by the creator in `enable`

/// Probation: while true, only the CREATORFUN test wallets below can enable buyback, so no outside
/// creator's money depends on the program while it can still be upgraded. The public version sets
/// this to false and is published together with the removal of the upgrade authority.
pub const PROBATION: bool = true;
#[cfg(not(feature = "devnet"))]
pub const PROBATION_CREATORS: [Pubkey; 1] = [pubkey!("DTmBFBxCNLsZgEsGqWBSVfKj1rrQtZQMT3WuTTGRPMwH")];
#[cfg(feature = "devnet")]
pub const PROBATION_CREATORS: [Pubkey; 1] = [pubkey!("5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF")]; // devnet test wallet

// ---------------------------------------------------------------------------
// External programs and accounts (Meteora, CREATORFUN)
// ---------------------------------------------------------------------------

pub const DBC_PROGRAM_ID: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
pub const DBC_POOL_AUTHORITY: Pubkey = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
pub const DAMM_PROGRAM_ID: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
pub const DAMM_POOL_AUTHORITY: Pubkey = pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");

/// The CREATORFUN bonding-curve config. Only pools made with it can enable buyback.
#[cfg(not(feature = "devnet"))]
pub const CREATORFUN_CONFIG: Pubkey = pubkey!("GRFxBcjZEcjMV8qAMsdiGu43gmqr8WgyyPJh1w3inBPo");
#[cfg(feature = "devnet")]
pub const CREATORFUN_CONFIG: Pubkey = pubkey!("6DNThd3xWokjwqVerywpKLqRASmt5ygfFnMz2iRuhv72"); // devnet copy of the same rules

/// DAMM v2 config Meteora uses when a CREATORFUN token graduates (migration fee option 2 = FixedBps100).
/// The graduated pool address is derived from it, so no other pool can be used.
pub const DAMM_MIGRATION_CONFIG: Pubkey = pubkey!("Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp");

/// Wallet the CREATORFUN server uses to trigger runs. It pays its own network fees and receives nothing.
pub const KEEPER: Pubkey = pubkey!("5KQ2oGJbnsJiQ8GXZ1w7QCro2sYZfMEPsmvmLter4irF");

// Meteora account layouts (bytemuck, repr C). Every offset was checked against live mainnet accounts.
const VIRTUAL_POOL_DISCRIMINATOR: [u8; 8] = [213, 224, 5, 209, 98, 69, 119, 92];
const POOL_CONFIG_OFFSET: usize = 72;
const POOL_CREATOR_OFFSET: usize = 104;
const POOL_BASE_MINT_OFFSET: usize = 136;
const POOL_SQRT_PRICE_OFFSET: usize = 280;
const POOL_IS_MIGRATED_OFFSET: usize = 305;
const POOL_MIGRATION_PROGRESS_OFFSET: usize = 308;
const DAMM_POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const DAMM_POOL_TOKEN_A_MINT_OFFSET: usize = 168;
const DAMM_POOL_TOKEN_B_MINT_OFFSET: usize = 200;
const DAMM_POOL_SQRT_PRICE_OFFSET: usize = 456;
const POSITION_DISCRIMINATOR: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];
const POSITION_POOL_OFFSET: usize = 8;
const POSITION_NFT_MINT_OFFSET: usize = 40;

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

    /// Turn on permanent buyback for one pool. Creator only, once, before graduation.
    /// The creator-fee rights move to this program's authority and can never come back.
    pub fn enable(ctx: Context<Enable>, buyback_bps: u16, threshold_lamports: u64) -> Result<()> {
        require!((MIN_BUYBACK_BPS..=MAX_BUYBACK_BPS).contains(&buyback_bps), BuybackError::BuybackShareOutOfRange);
        require!((MIN_THRESHOLD..=MAX_THRESHOLD).contains(&threshold_lamports), BuybackError::ThresholdOutOfRange);
        let a = &ctx.accounts;
        require!(!PROBATION || PROBATION_CREATORS.contains(&a.creator.key()), BuybackError::ProbationOnly);
        {
            // Read the Meteora pool directly: CREATORFUN pool, signer is its creator, right mint, not graduating.
            let data = curve_pool_data(&a.pool)?;
            require_keys_eq!(read_pubkey(&data, POOL_CONFIG_OFFSET), CREATORFUN_CONFIG, BuybackError::NotACreatorfunPool);
            require_keys_eq!(read_pubkey(&data, POOL_CREATOR_OFFSET), a.creator.key(), BuybackError::NotThePoolCreator);
            require_keys_eq!(read_pubkey(&data, POOL_BASE_MINT_OFFSET), a.base_mint.key(), BuybackError::WrongBaseMint);
            require!(data[POOL_IS_MIGRATED_OFFSET] == 0 && data[POOL_MIGRATION_PROGRESS_OFFSET] == 0, BuybackError::PoolAlreadyGraduating);
        }

        // Fund the authority's small reserve (rent for its temporary wSOL account).
        system_program::transfer(
            CpiContext::new(a.system_program.to_account_info(), SolTransfer { from: a.creator.to_account_info(), to: a.authority.to_account_info() }),
            AUTHORITY_RESERVE,
        )?;

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
        let vault = &mut ctx.accounts.vault;
        vault.pool = ctx.accounts.pool.key();
        vault.base_mint = ctx.accounts.base_mint.key();
        vault.creator = ctx.accounts.creator.key();
        vault.buyback_bps = buyback_bps;
        vault.threshold_lamports = threshold_lamports;
        vault.created_at = now;
        vault.last_run = now;
        vault.day_start = now;
        vault.bump = ctx.bumps.vault;
        vault.authority_bump = ctx.bumps.authority;

        emit!(BuybackEnabled { pool: vault.pool, base_mint: vault.base_mint, creator: vault.creator, buyback_bps, threshold_lamports });
        Ok(())
    }

    /// Anyone: move the creator trading fees from the bonding curve into this program (as SOL).
    pub fn claim_curve_fees(ctx: Context<ClaimCurve>) -> Result<()> {
        let a = &ctx.accounts;
        let pool_key = a.vault.pool;
        let bump = [a.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let before = a.authority.lamports();

        open_wsol(&a.authority, &a.authority_quote_ata, &a.quote_mint, &a.token_program, &a.associated_token_program, &a.system_program, signer)?;
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
                AccountMeta::new_readonly(a.token_program.key(), false),
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
                a.dbc_event_authority.to_account_info(),
                a.dbc_program.to_account_info(),
            ],
            signer,
        )?;
        close_wsol(&a.authority, &a.authority_quote_ata, &a.token_program, signer)?;

        let claimed = a.authority.lamports().saturating_sub(before);
        record_claim(&mut ctx.accounts.vault, 0, claimed);
        Ok(())
    }

    /// Anyone: after the curve completes, move the creator's share of the curve surplus into this program.
    pub fn claim_curve_surplus(ctx: Context<ClaimCurve>) -> Result<()> {
        let a = &ctx.accounts;
        let pool_key = a.vault.pool;
        let bump = [a.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let before = a.authority.lamports();

        open_wsol(&a.authority, &a.authority_quote_ata, &a.quote_mint, &a.token_program, &a.associated_token_program, &a.system_program, signer)?;
        let ix = Instruction {
            program_id: DBC_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                AccountMeta::new_readonly(CREATORFUN_CONFIG, false),
                AccountMeta::new(a.pool.key(), false),
                AccountMeta::new(a.authority_quote_ata.key(), false),
                AccountMeta::new(a.quote_vault.key(), false),
                AccountMeta::new_readonly(a.quote_mint.key(), false),
                AccountMeta::new_readonly(a.authority.key(), true),
                AccountMeta::new_readonly(a.token_program.key(), false),
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
                a.token_program.to_account_info(),
                a.dbc_event_authority.to_account_info(),
                a.dbc_program.to_account_info(),
            ],
            signer,
        )?;
        close_wsol(&a.authority, &a.authority_quote_ata, &a.token_program, signer)?;

        let claimed = a.authority.lamports().saturating_sub(before);
        record_claim(&mut ctx.accounts.vault, 1, claimed);
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
        let base_is_a = check_amm_pool(&a.amm_pool, a.vault.base_mint)?.1;
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
        let before = a.authority.lamports();

        open_wsol(&a.authority, &a.authority_quote_ata, &a.quote_mint, &a.token_program, &a.associated_token_program, &a.system_program, signer)?;
        let (acc_a, acc_b) = if base_is_a { (a.authority_base_ata.key(), a.authority_quote_ata.key()) } else { (a.authority_quote_ata.key(), a.authority_base_ata.key()) };
        let (mint_a, mint_b) = if base_is_a { (a.base_mint.key(), a.quote_mint.key()) } else { (a.quote_mint.key(), a.base_mint.key()) };
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
                AccountMeta::new_readonly(a.token_program.key(), false),
                AccountMeta::new_readonly(a.token_program.key(), false),
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
                a.damm_event_authority.to_account_info(),
                a.damm_program.to_account_info(),
            ],
            signer,
        )?;
        close_wsol(&a.authority, &a.authority_quote_ata, &a.token_program, signer)?;

        let claimed = a.authority.lamports().saturating_sub(before);
        record_claim(&mut ctx.accounts.vault, 2, claimed);
        Ok(())
    }

    /// Record the bonding-curve price. `execute_curve` must follow 2..150 slots later, and the price at
    /// that moment must not be more than about 1% worse. A same-slot price push before the buy therefore fails.
    pub fn prepare_curve(ctx: Context<PrepareCurve>) -> Result<()> {
        let a = &ctx.accounts;
        check_caller(&a.vault, a.caller.key(), Clock::get()?.unix_timestamp)?;
        let sqrt = {
            let data = curve_pool_data(&a.curve_pool)?;
            require!(data[POOL_IS_MIGRATED_OFFSET] == 0, BuybackError::AlreadyGraduated);
            read_u128(&data, POOL_SQRT_PRICE_OFFSET)
        };
        record_reference(&mut ctx.accounts.vault, VENUE_CURVE, sqrt)
    }

    /// Record the graduated DAMM v2 pool price. Same rules as `prepare_curve`.
    pub fn prepare_amm(ctx: Context<PrepareAmm>) -> Result<()> {
        let a = &ctx.accounts;
        check_caller(&a.vault, a.caller.key(), Clock::get()?.unix_timestamp)?;
        let (sqrt, _) = check_amm_pool(&a.amm_pool, a.vault.base_mint)?;
        record_reference(&mut ctx.accounts.vault, VENUE_AMM, sqrt)
    }

    /// Buyback on the bonding curve, burn everything bought, pay the creator the rest.
    pub fn execute_curve(ctx: Context<ExecuteCurve>, min_tokens_out: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let a = &ctx.accounts;
        let c = &a.common;
        require_keys_eq!(a.venue_pool.key(), c.vault.pool, BuybackError::WrongVenue);
        let sqrt_now = {
            let data = curve_pool_data(&a.venue_pool)?;
            read_u128(&data, POOL_SQRT_PRICE_OFFSET)
        };
        // Buying the token raises the curve's sqrt price, so a higher price than recorded is "worse".
        check_reference(&c.vault, VENUE_CURVE, sqrt_now, true)?;
        let plan = plan_run(&c.vault, c.authority.lamports(), now, c.caller.key())?;

        let pool_key = c.vault.pool;
        let bump = [c.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let fee_tokens = c.authority_base_ata.amount;

        if plan.buy > 0 {
            require!(min_tokens_out > 0, BuybackError::MinimumOutRequired);
            fund_wsol(c, plan.buy, signer)?;
            let ix = Instruction {
                program_id: DBC_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
                    AccountMeta::new_readonly(CREATORFUN_CONFIG, false),
                    AccountMeta::new(a.venue_pool.key(), false),
                    AccountMeta::new(c.authority_quote_ata.key(), false),
                    AccountMeta::new(c.authority_base_ata.key(), false),
                    AccountMeta::new(a.base_vault.key(), false),
                    AccountMeta::new(a.quote_vault.key(), false),
                    AccountMeta::new_readonly(c.base_mint.key(), false),
                    AccountMeta::new_readonly(c.quote_mint.key(), false),
                    AccountMeta::new_readonly(c.authority.key(), true),
                    AccountMeta::new_readonly(c.token_program.key(), false),
                    AccountMeta::new_readonly(c.token_program.key(), false),
                    AccountMeta::new_readonly(DBC_PROGRAM_ID, false), // no referral account
                    AccountMeta::new_readonly(a.dbc_event_authority.key(), false),
                    AccountMeta::new_readonly(DBC_PROGRAM_ID, false),
                ],
                data: ix_data_two_u64(IX_SWAP, plan.buy, min_tokens_out),
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
                    a.dbc_event_authority.to_account_info(),
                    a.dbc_program.to_account_info(),
                ],
                signer,
            )?;
            close_wsol(&c.authority, &c.authority_quote_ata, &c.token_program, signer)?;
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
        let (sqrt_now, base_is_a) = check_amm_pool(&a.amm_pool, c.vault.base_mint)?;
        // sqrt price is token B per token A. Buying the base raises it when base is A and lowers it when base is B.
        check_reference(&c.vault, VENUE_AMM, sqrt_now, base_is_a)?;
        let plan = plan_run(&c.vault, c.authority.lamports(), now, c.caller.key())?;

        let pool_key = c.vault.pool;
        let bump = [c.vault.authority_bump];
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, pool_key.as_ref(), &bump];
        let signer = &[seeds];
        let fee_tokens = c.authority_base_ata.amount;

        if plan.buy > 0 {
            require!(min_tokens_out > 0, BuybackError::MinimumOutRequired);
            fund_wsol(c, plan.buy, signer)?;
            let (mint_a, mint_b) = if base_is_a { (c.base_mint.key(), c.quote_mint.key()) } else { (c.quote_mint.key(), c.base_mint.key()) };
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
                    AccountMeta::new_readonly(c.token_program.key(), false),
                    AccountMeta::new_readonly(c.token_program.key(), false),
                    AccountMeta::new_readonly(DAMM_PROGRAM_ID, false), // no referral account
                    AccountMeta::new_readonly(a.damm_event_authority.key(), false),
                    AccountMeta::new_readonly(DAMM_PROGRAM_ID, false),
                ],
                data: ix_data_two_u64(IX_SWAP, plan.buy, min_tokens_out),
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
                    a.damm_event_authority.to_account_info(),
                    a.damm_program.to_account_info(),
                ],
                signer,
            )?;
            close_wsol(&c.authority, &c.authority_quote_ata, &c.token_program, signer)?;
        }
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
    /// Lamports spent on the buyback this run.
    pub buy: u64,
    /// Creator share produced by this run (paid now, or kept as owed if it is below MIN_PAYOUT).
    pub pay: u64,
}

/// Who may prepare/run now: the keeper whenever the rules allow, anyone once PUBLIC_RUN_DELAY has passed.
pub fn caller_allowed(caller_is_keeper: bool, last_run: i64, now: i64) -> bool {
    caller_is_keeper || now.saturating_sub(last_run) >= PUBLIC_RUN_DELAY
}

/// How much of `splittable` SOL this run buys back and how much it produces for the creator.
/// `spent_today` is what the vault already spent on buybacks in the current 24-hour window.
pub fn plan_amounts(splittable: u64, buyback_bps: u16, threshold: u64, last_run: i64, now: i64, spent_today: u64) -> Result<Plan> {
    let waited = now.saturating_sub(last_run);
    require!(waited >= MIN_RUN_GAP, BuybackError::TooSoon);
    let ready = splittable >= threshold || (waited >= TIMER_SECONDS && splittable >= MIN_TIMER_RUN);
    require!(ready, BuybackError::NotReady);

    let buy_cap = MAX_BUY_PER_RUN.min(MAX_BUY_PER_DAY.saturating_sub(spent_today));
    require!(buy_cap > 0, BuybackError::DailyCapReached);
    // Process at most the amount whose buyback share equals buy_cap; the rest waits for later runs.
    let used = splittable.min(((buy_cap as u128) * 10_000 / (buyback_bps as u128)) as u64);
    let buy = ((used as u128) * (buyback_bps as u128) / 10_000) as u64;
    Ok(Plan { buy, pay: used - buy })
}

/// Spend already recorded in the current 24-hour window.
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

fn plan_run(vault: &Vault, authority_lamports: u64, now: i64, caller: Pubkey) -> Result<Plan> {
    require!(caller_allowed(caller == KEEPER, vault.last_run, now), BuybackError::KeeperWindow);
    // SOL already owed to the creator is not split again.
    let splittable = authority_lamports.saturating_sub(AUTHORITY_RESERVE).saturating_sub(vault.creator_owed);
    plan_amounts(splittable, vault.buyback_bps, vault.threshold_lamports, vault.last_run, now, spent_in_window(vault.day_start, vault.day_spent, now))
}

fn check_caller(vault: &Vault, caller: Pubkey, now: i64) -> Result<()> {
    require!(caller_allowed(caller == KEEPER, vault.last_run, now), BuybackError::KeeperWindow);
    Ok(())
}

fn record_reference(vault: &mut Vault, venue: u8, sqrt: u128) -> Result<()> {
    vault.ref_venue = venue;
    vault.ref_sqrt_price = sqrt;
    vault.ref_slot = Clock::get()?.slot;
    Ok(())
}

fn check_reference(vault: &Vault, venue: u8, now_sqrt: u128, higher_is_worse: bool) -> Result<()> {
    let slot = Clock::get()?.slot;
    require!(vault.ref_venue == venue && vault.ref_slot > 0, BuybackError::NotPrepared);
    require!(slot >= vault.ref_slot + MIN_PREPARE_SLOTS && slot <= vault.ref_slot + MAX_PREPARE_SLOTS, BuybackError::NotPrepared);
    require!(price_ok(vault.ref_sqrt_price, now_sqrt, higher_is_worse), BuybackError::PriceMoved);
    Ok(())
}

fn record_claim(vault: &mut Vault, source: u8, lamports: u64) {
    if lamports == 0 {
        return;
    }
    vault.total_claimed_lamports = vault.total_claimed_lamports.saturating_add(lamports);
    emit!(FeesClaimed { pool: vault.pool, source, lamports });
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

fn ix_data_two_u64(disc: [u8; 8], a: u64, b: u64) -> Vec<u8> {
    let mut d = disc.to_vec();
    d.extend_from_slice(&a.to_le_bytes());
    d.extend_from_slice(&b.to_le_bytes());
    d
}

/// Borrow a Meteora bonding-curve pool's data after checking owner, discriminator and length.
fn curve_pool_data<'a>(pool: &'a AccountInfo) -> Result<std::cell::Ref<'a, &'a mut [u8]>> {
    require_keys_eq!(*pool.owner, DBC_PROGRAM_ID, BuybackError::NotABondingCurvePool);
    let data = pool.try_borrow_data()?;
    require!(data.len() > POOL_MIGRATION_PROGRESS_OFFSET && data[..8] == VIRTUAL_POOL_DISCRIMINATOR, BuybackError::NotABondingCurvePool);
    Ok(data)
}

/// The DAMM v2 pool Meteora creates at graduation: [b"pool", DAMM_MIGRATION_CONFIG, larger mint, smaller mint].
pub fn graduated_pool_address(base_mint: &Pubkey) -> Pubkey {
    let (first, second) = if base_mint.to_bytes() > native_mint::ID.to_bytes() { (*base_mint, native_mint::ID) } else { (native_mint::ID, *base_mint) };
    Pubkey::find_program_address(&[b"pool", DAMM_MIGRATION_CONFIG.as_ref(), first.as_ref(), second.as_ref()], &DAMM_PROGRAM_ID).0
}

/// Check that `pool` is the graduated pool of `base_mint` and return (sqrt price, base is token A).
fn check_amm_pool(pool: &AccountInfo, base_mint: Pubkey) -> Result<(u128, bool)> {
    require_keys_eq!(pool.key(), graduated_pool_address(&base_mint), BuybackError::WrongAmmPool);
    require_keys_eq!(*pool.owner, DAMM_PROGRAM_ID, BuybackError::WrongAmmPool);
    let data = pool.try_borrow_data()?;
    require!(data.len() >= DAMM_POOL_SQRT_PRICE_OFFSET + 16 && data[..8] == DAMM_POOL_DISCRIMINATOR, BuybackError::WrongAmmPool);
    let (mint_a, mint_b) = (read_pubkey(&data, DAMM_POOL_TOKEN_A_MINT_OFFSET), read_pubkey(&data, DAMM_POOL_TOKEN_B_MINT_OFFSET));
    let base_is_a = if mint_a == base_mint && mint_b == native_mint::ID {
        true
    } else if mint_a == native_mint::ID && mint_b == base_mint {
        false
    } else {
        return err!(BuybackError::WrongAmmPool);
    };
    Ok((read_u128(&data, DAMM_POOL_SQRT_PRICE_OFFSET), base_is_a))
}

/// Create the authority's temporary wSOL account (the authority pays the rent from its reserve).
fn open_wsol<'info>(
    authority: &SystemAccount<'info>,
    wsol: &UncheckedAccount<'info>,
    wsol_mint: &Account<'info, Mint>,
    token_program: &Program<'info, Token>,
    ata_program: &Program<'info, AssociatedToken>,
    system: &Program<'info, System>,
    signer: &[&[&[u8]]],
) -> Result<()> {
    associated_token::create_idempotent(CpiContext::new_with_signer(
        ata_program.to_account_info(),
        Create {
            payer: authority.to_account_info(),
            associated_token: wsol.to_account_info(),
            authority: authority.to_account_info(),
            mint: wsol_mint.to_account_info(),
            system_program: system.to_account_info(),
            token_program: token_program.to_account_info(),
        },
        signer,
    ))
}

/// Close the temporary wSOL account. All its lamports (rent + wSOL) return to the authority as plain SOL.
fn close_wsol<'info>(authority: &SystemAccount<'info>, wsol: &UncheckedAccount<'info>, token_program: &Program<'info, Token>, signer: &[&[&[u8]]]) -> Result<()> {
    token::close_account(CpiContext::new_with_signer(
        token_program.to_account_info(),
        CloseAccount { account: wsol.to_account_info(), destination: authority.to_account_info(), authority: authority.to_account_info() },
        signer,
    ))
}

/// Put exactly `lamports` of the authority's SOL into its temporary wSOL account, ready to swap.
fn fund_wsol(c: &RunAccounts<'_>, lamports: u64, signer: &[&[&[u8]]]) -> Result<()> {
    open_wsol(&c.authority, &c.authority_quote_ata, &c.quote_mint, &c.token_program, &c.associated_token_program, &c.system_program, signer)?;
    system_program::transfer(
        CpiContext::new_with_signer(c.system_program.to_account_info(), SolTransfer { from: c.authority.to_account_info(), to: c.authority_quote_ata.to_account_info() }, signer),
        lamports,
    )?;
    token::sync_native(CpiContext::new(c.token_program.to_account_info(), SyncNative { account: c.authority_quote_ata.to_account_info() }))
}

/// After the buy: burn every bought token, split base-token fees by the same share, pay the creator, record the run.
fn settle(c: &mut RunAccounts<'_>, plan: Plan, fee_tokens_before: u64, now: i64, signer: &[&[&[u8]]]) -> Result<BuybackExecuted> {
    c.authority_base_ata.reload()?;
    let bought = c.authority_base_ata.amount.saturating_sub(fee_tokens_before);
    let bps = c.vault.buyback_bps as u128;
    let fee_burn = ((fee_tokens_before as u128) * bps / 10_000) as u64;
    let fee_to_creator = fee_tokens_before - fee_burn;
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
    if fee_to_creator > 0 {
        associated_token::create_idempotent(CpiContext::new(
            c.associated_token_program.to_account_info(),
            Create {
                payer: c.caller.to_account_info(),
                associated_token: c.creator_base_ata.to_account_info(),
                authority: c.creator.to_account_info(),
                mint: c.base_mint.to_account_info(),
                system_program: c.system_program.to_account_info(),
                token_program: c.token_program.to_account_info(),
            },
        ))?;
        token::transfer(
            CpiContext::new_with_signer(
                c.token_program.to_account_info(),
                Transfer { from: c.authority_base_ata.to_account_info(), to: c.creator_base_ata.to_account_info(), authority: c.authority.to_account_info() },
                signer,
            ),
            fee_to_creator,
        )?;
    }

    // Creator SOL: pay this run's share plus anything owed, unless the total is still below MIN_PAYOUT.
    let due = c.vault.creator_owed.saturating_add(plan.pay);
    let paid = if due >= MIN_PAYOUT { due } else { 0 };
    if paid > 0 {
        system_program::transfer(
            CpiContext::new_with_signer(c.system_program.to_account_info(), SolTransfer { from: c.authority.to_account_info(), to: c.creator.to_account_info() }, signer),
            paid,
        )?;
    }

    let v = &mut c.vault;
    v.creator_owed = due - paid;
    if now.saturating_sub(v.day_start) >= 24 * 60 * 60 {
        v.day_start = now;
        v.day_spent = 0;
    }
    v.day_spent = v.day_spent.saturating_add(plan.buy);
    v.ref_slot = 0; // every run needs a fresh prepare
    v.runs = v.runs.saturating_add(1);
    v.last_run = now;
    v.total_buyback_lamports = v.total_buyback_lamports.saturating_add(plan.buy);
    v.total_paid_lamports = v.total_paid_lamports.saturating_add(paid);
    v.total_burned = v.total_burned.saturating_add(burned);
    v.total_fee_tokens_to_creator = v.total_fee_tokens_to_creator.saturating_add(fee_to_creator);
    Ok(BuybackExecuted {
        pool: v.pool,
        caller: c.caller.key(),
        run: v.runs,
        spent_lamports: plan.buy,
        bought,
        burned,
        paid_lamports: paid,
        creator_owed_lamports: v.creator_owed,
        fee_tokens_to_creator: fee_to_creator,
    })
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One per pool. The rule fields are written only by `enable`; no instruction changes them later.
#[account]
#[derive(InitSpace)]
pub struct Vault {
    // --- rules, fixed forever ---
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub creator: Pubkey,
    pub buyback_bps: u16,
    pub threshold_lamports: u64,
    pub created_at: i64,
    // --- run bookkeeping ---
    pub last_run: i64,
    pub day_start: i64,
    pub day_spent: u64,
    pub ref_venue: u8,
    pub ref_slot: u64,
    pub ref_sqrt_price: u128,
    /// Creator SOL produced by runs but not yet paid because it was below MIN_PAYOUT.
    pub creator_owed: u64,
    // --- running totals ---
    pub runs: u64,
    pub total_claimed_lamports: u64,
    pub total_buyback_lamports: u64,
    pub total_paid_lamports: u64,
    pub total_burned: u64,
    pub total_fee_tokens_to_creator: u64,
    pub bump: u8,
    pub authority_bump: u8,
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct Enable<'info> {
    /// The pool's current creator. Pays for the vault, its token account and the 0.005 SOL reserve.
    #[account(mut)]
    pub creator: Signer<'info>,
    /// CHECK: Meteora bonding-curve pool; owner, layout, config, creator, mint and stage are checked in `enable`.
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: must be the CREATORFUN config; Meteora also checks it belongs to the pool.
    #[account(address = CREATORFUN_CONFIG)]
    pub config: UncheckedAccount<'info>,
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(init, payer = creator, space = 8 + Vault::INIT_SPACE, seeds = [VAULT_SEED, pool.key().as_ref()], bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// Program-controlled wallet that becomes the pool creator. No private key exists for it.
    #[account(mut, seeds = [AUTHORITY_SEED, pool.key().as_ref()], bump)]
    pub authority: SystemAccount<'info>,
    #[account(init_if_needed, payer = creator, associated_token::mint = base_mint, associated_token::authority = authority)]
    pub authority_base_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: Meteora event authority; Meteora verifies it.
    pub dbc_event_authority: UncheckedAccount<'info>,
    /// CHECK: Meteora bonding-curve program.
    #[account(address = DBC_PROGRAM_ID)]
    pub dbc_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
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
    /// CHECK: the authority's wSOL account address; created and closed inside the instruction.
    #[account(mut, address = get_associated_token_address(&authority.key(), &native_mint::ID))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: Meteora checks it matches the pool.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: Meteora checks it matches the pool.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    #[account(address = vault.base_mint)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(address = native_mint::ID)]
    pub quote_mint: Box<Account<'info, Mint>>,
    /// CHECK: fixed CREATORFUN config.
    #[account(address = CREATORFUN_CONFIG)]
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
    /// CHECK: must be the graduated DAMM v2 pool address derived from the base mint (checked in the handler).
    pub amm_pool: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 position; owner, layout, pool and NFT are checked in the handler.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// The position NFT, which must be held by the authority.
    pub position_nft_account: Box<InterfaceAccount<'info, AnyTokenAccount>>,
    #[account(mut, associated_token::mint = base_mint, associated_token::authority = authority)]
    pub authority_base_ata: Box<Account<'info, TokenAccount>>,
    /// CHECK: the authority's wSOL account address; created and closed inside the instruction.
    #[account(mut, address = get_associated_token_address(&authority.key(), &native_mint::ID))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it matches the pool.
    #[account(mut)]
    pub token_a_vault: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 checks it matches the pool.
    #[account(mut)]
    pub token_b_vault: UncheckedAccount<'info>,
    #[account(address = vault.base_mint)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(address = native_mint::ID)]
    pub quote_mint: Box<Account<'info, Mint>>,
    /// CHECK: fixed Meteora address.
    #[account(address = DAMM_POOL_AUTHORITY)]
    pub damm_pool_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 verifies it.
    pub damm_event_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM v2 program.
    #[account(address = DAMM_PROGRAM_ID)]
    pub damm_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct PrepareCurve<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// CHECK: the vault's bonding-curve pool (address checked; layout checked in the handler).
    #[account(address = vault.pool)]
    pub curve_pool: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct PrepareAmm<'info> {
    pub caller: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.pool.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// CHECK: must be the graduated DAMM v2 pool address derived from the base mint (checked in the handler).
    pub amm_pool: UncheckedAccount<'info>,
}

/// Accounts shared by both buyback runs. The creator wallet and token accounts are pinned to the vault.
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
    /// CHECK: the authority's wSOL account address; created and closed inside the instruction.
    #[account(mut, address = get_associated_token_address(&authority.key(), &native_mint::ID))]
    pub authority_quote_ata: UncheckedAccount<'info>,
    /// CHECK: the creator wallet saved in `enable` (address checked). The only wallet that can ever receive SOL from this program.
    #[account(mut, address = vault.creator)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the creator's token account for the base mint (address checked); created if needed.
    #[account(mut, address = get_associated_token_address(&vault.creator, &vault.base_mint))]
    pub creator_base_ata: UncheckedAccount<'info>,
    #[account(mut, address = vault.base_mint)]
    pub base_mint: Box<Account<'info, Mint>>,
    #[account(address = native_mint::ID)]
    pub quote_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
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
    /// CHECK: fixed CREATORFUN config.
    #[account(address = CREATORFUN_CONFIG)]
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
    /// CHECK: must be the graduated DAMM v2 pool address derived from the base mint (checked in the handler).
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

// ---------------------------------------------------------------------------
// Events and errors
// ---------------------------------------------------------------------------

#[event]
pub struct BuybackEnabled {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub creator: Pubkey,
    pub buyback_bps: u16,
    pub threshold_lamports: u64,
}

#[event]
pub struct FeesClaimed {
    pub pool: Pubkey,
    /// 0 = curve trading fees, 1 = curve surplus, 2 = DAMM v2 LP fees
    pub source: u8,
    pub lamports: u64,
}

#[event]
pub struct BuybackExecuted {
    pub pool: Pubkey,
    pub caller: Pubkey,
    pub run: u64,
    pub spent_lamports: u64,
    pub bought: u64,
    pub burned: u64,
    pub paid_lamports: u64,
    pub creator_owed_lamports: u64,
    pub fee_tokens_to_creator: u64,
}

#[error_code]
pub enum BuybackError {
    #[msg("Buyback share must be between 5% and 100%.")]
    BuybackShareOutOfRange,
    #[msg("Run threshold must be between 0.1 and 10 SOL.")]
    ThresholdOutOfRange,
    #[msg("During probation only CREATORFUN test wallets can enable buyback.")]
    ProbationOnly,
    #[msg("Not a Meteora bonding-curve pool.")]
    NotABondingCurvePool,
    #[msg("This pool was not launched on CREATORFUN.")]
    NotACreatorfunPool,
    #[msg("Only the current pool creator can enable buyback.")]
    NotThePoolCreator,
    #[msg("Base mint does not match the pool.")]
    WrongBaseMint,
    #[msg("Buyback can only be enabled before the token graduates.")]
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
    #[msg("Wrong pool for this buyback.")]
    WrongVenue,
    #[msg("Too soon: runs must be at least 10 minutes apart.")]
    TooSoon,
    #[msg("Not ready: below the threshold and the 7-day timer has not passed.")]
    NotReady,
    #[msg("This vault already spent its 24-hour buyback limit.")]
    DailyCapReached,
    #[msg("Only the CREATORFUN keeper can do this now; anyone can after 8 days without a run.")]
    KeeperWindow,
    #[msg("Call prepare first; the buy must follow 2 to 150 slots later.")]
    NotPrepared,
    #[msg("The pool price moved against the buy since prepare. Try again.")]
    PriceMoved,
    #[msg("A minimum token amount is required to protect the buy.")]
    MinimumOutRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn below_threshold_and_timer_is_not_ready() {
        assert!(plan_amounts(50_000_000, 3_000, 500_000_000, 0, 6 * DAY, 0).is_err());
    }

    #[test]
    fn threshold_reached_splits_by_share() {
        let p = plan_amounts(500_000_000, 3_000, 500_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 150_000_000, pay: 350_000_000 });
    }

    #[test]
    fn runs_are_at_least_10_minutes_apart() {
        assert!(plan_amounts(500_000_000, 3_000, 100_000_000, 0, 9 * 60, 0).is_err());
        assert!(plan_amounts(500_000_000, 3_000, 100_000_000, 0, 10 * 60, 0).is_ok());
    }

    #[test]
    fn timer_allows_small_runs_after_7_days() {
        let p = plan_amounts(20_000_000, 5_000, 1_000_000_000, 0, 7 * DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 10_000_000, pay: 10_000_000 });
        assert!(plan_amounts(5_000_000, 5_000, 1_000_000_000, 0, 7 * DAY, 0).is_err());
    }

    #[test]
    fn buy_is_capped_per_run() {
        let p = plan_amounts(5_000_000_000, 10_000, 100_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p, Plan { buy: 1_000_000_000, pay: 0 });
        let p = plan_amounts(10_000_000_000, 3_000, 100_000_000, 0, DAY, 0).unwrap();
        assert_eq!(p.buy, 999_999_999);
        assert_eq!(p.buy + p.pay, 3_333_333_333);
    }

    #[test]
    fn buy_is_capped_per_day() {
        let p = plan_amounts(5_000_000_000, 10_000, 100_000_000, 0, DAY, 4_600_000_000).unwrap();
        assert_eq!(p.buy, 400_000_000);
        assert!(plan_amounts(5_000_000_000, 10_000, 100_000_000, 0, DAY, MAX_BUY_PER_DAY).is_err());
        assert_eq!(spent_in_window(0, MAX_BUY_PER_DAY, DAY), 0);
        assert_eq!(spent_in_window(0, MAX_BUY_PER_DAY, DAY - 1), MAX_BUY_PER_DAY);
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
        for bps in [MIN_BUYBACK_BPS, 1_234, 3_000, 9_999, MAX_BUYBACK_BPS] {
            for avail in [10_000_000u64, 123_456_789, 999_999_999, 3_000_000_000] {
                let p = plan_amounts(avail, bps, MIN_THRESHOLD, 0, 30 * DAY, 0).unwrap();
                assert!(p.buy + p.pay <= avail);
                assert!(p.buy <= MAX_BUY_PER_RUN);
            }
        }
    }

    #[test]
    fn graduated_pool_is_derived_not_chosen() {
        let a = graduated_pool_address(&pubkey!("CGDFGSBNNKcAdwdV4Q3WvkwgngRsiUSj5oEdAtymZd4Y"));
        let b = graduated_pool_address(&pubkey!("CGDFGSBNNKcAdwdV4Q3WvkwgngRsiUSj5oEdAtymZd4Y"));
        let c = graduated_pool_address(&pubkey!("DTmBFBxCNLsZgEsGqWBSVfKj1rrQtZQMT3WuTTGRPMwH"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
