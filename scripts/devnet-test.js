#!/usr/bin/env node
/**
 * CREATORFUN buyback, burn & donation — devnet end-to-end test.
 *
 * Runs against the devnet build of the program (feature "devnet": test wallet 5KQ2... is the keeper and the
 * only wallet allowed to enable during probation). Each test token keeps its own state file, chosen with
 * TEST_TAG (default: devnet-state.json; TEST_TAG=a -> devnet-state-a.json).
 *
 *   node devnet-test.js pairsetup                         once: devnet mock stock token + pair config
 *   node devnet-test.js create [sol|pair]                 create a test token (SOL pair, or the devnet stock-pair mock)
 *   node devnet-test.js enable <buyback%> <donation%> <threshold> [donee|new]
 *                                                        turn it on; threshold in units (SOL coins: SOL)
 *   node devnet-test.js trade <rounds> <amount>           buy then sell <amount> of the pair asset per round
 *   node devnet-test.js claim                             claim_curve_fees (anyone can call)
 *   node devnet-test.js run                               prepare + execute (or distribute when buyback is 0%)
 *   node devnet-test.js status                            vault, balances, token supply
 *   node devnet-test.js negative                          actions that MUST fail
 *
 * Graduation path (use a separate TEST_TAG and the stock-pair mock, whose 85-MOCK graduation is cheap on devnet):
 *   node devnet-test.js graduate [amount]                 buy until the curve completes (default 95 of the pair asset)
 *   node devnet-test.js migrate                           Meteora migration to the DAMM v2 pool (anyone can call)
 *   node devnet-test.js surplus                           claim_curve_surplus
 *   node devnet-test.js ammtrade <rounds> <amount>        buy then sell on the graduated DAMM v2 pool (LP fees)
 *   node devnet-test.js ammclaim                          claim_amm_fees
 *   node devnet-test.js ammrun                            prepare_amm + execute_amm (or distribute)
 *   node devnet-test.js gradtest                          all of the above in order, with a pass/fail summary
 *
 * Needs: OPERATOR_PRIVATE_KEY and RPC_URL in the environment (or in the .env file set by ENV_FILE),
 * the program IDL (IDL_PATH) and the npm packages below.
 */
require('dotenv').config({ path: process.env.ENV_FILE || '.env', quiet: true });
const fs = require('fs');
const path = require('path');
const BN = require('bn.js');
const anchor = require('@coral-xyz/anchor');
const {
  Connection, Keypair, PublicKey, SystemProgram, Transaction, LAMPORTS_PER_SOL, sendAndConfirmTransaction,
} = require('@solana/web3.js');
const { DynamicBondingCurveClient } = require('@meteora-ag/dynamic-bonding-curve-sdk');
const bs58lib = require('bs58');
const bs58 = bs58lib.default || bs58lib;

const PROGRAM_ID = new PublicKey('eJGfjnQn4Gk7gNvyGNmDYPjBQBu6msUmSUr91fyUq2j');
const SOL_CONFIG = new PublicKey('6DNThd3xWokjwqVerywpKLqRASmt5ygfFnMz2iRuhv72'); // devnet CREATORFUN SOL config
const DBC = new PublicKey('dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN');
const DBC_POOL_AUTHORITY = new PublicKey('FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM');
const WSOL = new PublicKey('So11111111111111111111111111111111111111112');
const TOKEN = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
const ATA_PROGRAM = new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL');
const IDL_PATH = process.env.IDL_PATH || path.join(__dirname, '..', 'target', 'idl', 'creatorfun_buyback.json');
const STATE = path.join(__dirname, `devnet-state${process.env.TEST_TAG ? '-' + process.env.TEST_TAG : ''}.json`);
const PAIR_FILE = path.join(__dirname, 'devnet-pair.json'); // devnet stock-pair mock: { mint, config, decimals }
const RESERVE = 5_000_000;
const DAMM = new PublicKey('cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG');
const DAMM_POOL_AUTHORITY = new PublicKey('HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC');
const DAMM_MIGRATION_CONFIG = new PublicKey('Hv8Lmzmnju6m7kcokVKvwqz7QPmdX9XfKjJsXz8RXcjp');
const TOKEN_2022 = new PublicKey('TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb');
const IX_SWAP = Buffer.from([248, 198, 158, 145, 225, 117, 135, 200]);

const key = (process.env.RPC_URL || '').match(/api-key=([A-Za-z0-9-]+)/);
const RPC = key ? `https://devnet.helius-rpc.com/?api-key=${key[1]}` : 'https://api.devnet.solana.com';
const connection = new Connection(RPC, 'confirmed');
const dbc = new DynamicBondingCurveClient(connection, 'confirmed');

function loadOperator() {
  const raw = (process.env.OPERATOR_PRIVATE_KEY || '').trim();
  const secret = raw.startsWith('[') ? Uint8Array.from(JSON.parse(raw)) : bs58.decode(raw);
  return Keypair.fromSecretKey(secret);
}
const me = loadOperator();
const wallet = new anchor.Wallet(me);
const provider = new anchor.AnchorProvider(connection, wallet, { commitment: 'confirmed' });
const idl = JSON.parse(fs.readFileSync(IDL_PATH, 'utf8'));
idl.address = PROGRAM_ID.toBase58();
const program = new anchor.Program(idl, provider);

const pda = (seeds, prog) => PublicKey.findProgramAddressSync(seeds, prog)[0];
const ata = (owner, mint, prog = TOKEN) => pda([owner.toBuffer(), prog.toBuffer(), mint.toBuffer()], ATA_PROGRAM);
const dbcEvent = pda([Buffer.from('__event_authority')], DBC);
const dammEvent = pda([Buffer.from('__event_authority')], DAMM);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const readState = () => (fs.existsSync(STATE) ? JSON.parse(fs.readFileSync(STATE, 'utf8')) : {});
const writeState = (s) => fs.writeFileSync(STATE, JSON.stringify(s, null, 2));
const link = (sig) => `https://solscan.io/tx/${sig}?cluster=devnet`;
const NO_DONEE = SystemProgram.programId; // Pubkey::default()

function ctx() {
  const s = readState();
  if (!s.mint) throw new Error('Run "create" first.');
  const mint = new PublicKey(s.mint);
  const pool = new PublicKey(s.pool);
  const config = new PublicKey(s.config);
  const quote = new PublicKey(s.quoteMint);
  const qprog = new PublicKey(s.quoteProgram);
  const vault = pda([Buffer.from('vault'), pool.toBuffer()], PROGRAM_ID);
  const authority = pda([Buffer.from('authority'), pool.toBuffer()], PROGRAM_ID);
  const isSol = quote.equals(WSOL);
  const dec = s.decimals ?? 9;
  const fmt = (a) => (Number(a) / 10 ** dec).toFixed(Math.min(dec, 6)) + (isSol ? ' SOL' : ' MOCK');
  return { s, mint, pool, config, quote, qprog, vault, authority, isSol, dec, fmt };
}

async function poolState(pool) {
  const raw = await dbc.state.getPool(pool);
  return raw.account || raw.poolState || raw;
}

async function sendTx(tx, signers) {
  const { blockhash } = await connection.getLatestBlockhash('confirmed');
  tx.recentBlockhash = blockhash;
  tx.feePayer = signers[0].publicKey;
  return sendAndConfirmTransaction(connection, tx, signers, { commitment: 'confirmed' });
}

/** Build an instruction and mark the donation wallet accounts writable (needed only when a donation is paid). */
async function sendIx(builder, signers = [me], writable = []) {
  const ix = await builder.instruction();
  ix.keys = ix.keys.map((k) => (writable.some((w) => w.equals(k.pubkey)) ? { ...k, isWritable: true } : k));
  return sendTx(new Transaction().add(ix), signers);
}

/** Pair asset held by the vault authority (same rule as income_balance in lib.rs). */
async function vaultBalance(c) {
  if (c.isSol) return Math.max(0, (await connection.getBalance(c.authority)) - RESERVE);
  const b = await connection.getTokenAccountBalance(ata(c.authority, c.quote, c.qprog)).catch(() => null);
  return b ? Number(b.value.amount) : 0;
}

async function walletQuote(c, owner) {
  if (c.isSol) return connection.getBalance(owner);
  const b = await connection.getTokenAccountBalance(ata(owner, c.quote, c.qprog)).catch(() => null);
  return b ? Number(b.value.amount) : 0;
}

async function unitOf(config) {
  const info = await connection.getAccountInfo(config);
  return Number(info.data.readBigUInt64LE(264) / 85n);
}

// ---------------------------------------------------------------------------

/**
 * Devnet stand-in for a stock token: a Token-2022 mint with 8 decimals (like TSLAx) and a DBC config built
 * with the same rules and code path as pairconfig.js (fee claimer = CREATORFUN fee wallet). 1 MOCK plays 1 SOL,
 * so graduation is 85 MOCK and one unit is 1 MOCK.
 */
async function pairsetup() {
  if (fs.existsSync(PAIR_FILE)) { console.log('Pair mock already exists:', fs.readFileSync(PAIR_FILE, 'utf8')); return; }
  const spl = require('@solana/spl-token');
  const { buildCurve, TokenType, TokenDecimal, TokenAuthorityOption, BaseFeeMode, CollectFeeMode, MigrationOption, MigrationFeeOption, ActivationType } = require('@meteora-ag/dynamic-bonding-curve-sdk');
  const T22 = spl.TOKEN_2022_PROGRAM_ID;
  const decimals = 8;
  const mint = await spl.createMint(connection, me, me.publicKey, null, decimals, Keypair.generate(), { commitment: 'confirmed' }, T22);
  const myAta = await spl.getOrCreateAssociatedTokenAccount(connection, me, mint, me.publicKey, false, 'confirmed', {}, T22);
  await spl.mintTo(connection, me, mint, myAta.address, me, BigInt(10_000) * 10n ** 8n, [], { commitment: 'confirmed' }, T22);
  const curve = buildCurve({
    token: { tokenType: TokenType.SPLToken, tokenBaseDecimal: TokenDecimal.SIX, tokenQuoteDecimal: decimals, tokenAuthorityOption: TokenAuthorityOption.Immutable, totalTokenSupply: 1_000_000_000, leftover: 0 },
    fee: {
      baseFeeParams: { baseFeeMode: BaseFeeMode.FeeSchedulerLinear, feeSchedulerParam: { startingFeeBps: 125, endingFeeBps: 125, numberOfPeriod: 0, totalDuration: 0 } },
      dynamicFeeEnabled: false, collectFeeMode: CollectFeeMode.QuoteToken, creatorTradingFeePercentage: 80, poolCreationFee: 0, enableFirstSwapWithMinFee: false,
    },
    migration: { migrationOption: MigrationOption.MET_DAMM_V2, migrationFeeOption: MigrationFeeOption.FixedBps100, migrationFee: { feePercentage: 0, creatorFeePercentage: 0 } },
    liquidityDistribution: { partnerPermanentLockedLiquidityPercentage: 20, partnerLiquidityPercentage: 0, creatorPermanentLockedLiquidityPercentage: 80, creatorLiquidityPercentage: 0 },
    lockedVesting: { totalLockedVestingAmount: 0, numberOfVestingPeriod: 0, cliffUnlockAmount: 0, totalVestingDuration: 0, cliffDurationFromMigrationTime: 0 },
    activationType: ActivationType.Timestamp,
    percentageSupplyOnMigration: 20,
    migrationQuoteThreshold: 85,
  });
  const feeWallet = new PublicKey('CooB38vtmMP4oLcSsLsmUn1YfLELG7NkfPXYTv21NcBx');
  const configKp = Keypair.generate();
  const tx = await dbc.partner.createConfig({ config: configKp.publicKey, feeClaimer: feeWallet, leftoverReceiver: feeWallet, quoteMint: mint, payer: me.publicKey, ...curve });
  const sig = await sendTx(tx, [me, configKp]);
  const rec = { mint: mint.toBase58(), config: configKp.publicKey.toBase58(), decimals, program: T22.toBase58(), createSig: sig };
  fs.writeFileSync(PAIR_FILE, JSON.stringify(rec, null, 2));
  console.log('Mock stock token (Token-2022, 8 decimals):', rec.mint, '| 10,000 minted to the test wallet');
  console.log('Pair config (fee claimer CooB38):', rec.config, link(sig));
}

async function create(kind = 'sol') {
  if (readState().mint) throw new Error(`Test token already exists: ${readState().mint}`);
  let config = SOL_CONFIG, quote = WSOL, qprog = TOKEN, decimals = 9;
  if (kind === 'pair') {
    const p = JSON.parse(fs.readFileSync(PAIR_FILE, 'utf8'));
    config = new PublicKey(p.config); quote = new PublicKey(p.mint); decimals = p.decimals;
    qprog = (await connection.getAccountInfo(quote)).owner;
  }
  const mintKp = Keypair.generate();
  const tx = await dbc.creator.createPool({
    name: 'CF Buyback Test', symbol: 'CFBBT', uri: 'https://creatorfun.cloud/m/devnet-test.json',
    payer: me.publicKey, poolCreator: me.publicKey, config, baseMint: mintKp.publicKey,
  });
  const sig = await sendTx(tx, [me, mintKp]);
  const [a, b] = [mintKp.publicKey, quote].sort((x, y) => Buffer.compare(y.toBuffer(), x.toBuffer()));
  const pool = pda([Buffer.from('pool'), config.toBuffer(), a.toBuffer(), b.toBuffer()], DBC);
  const info = await connection.getAccountInfo(pool);
  if (!info || !info.owner.equals(DBC)) throw new Error(`Pool not found at ${pool.toBase58()} (derivation mismatch)`);
  writeState({ mint: mintKp.publicKey.toBase58(), pool: pool.toBase58(), config: config.toBase58(), quoteMint: quote.toBase58(), quoteProgram: qprog.toBase58(), decimals, createSig: sig });
  console.log('Token :', mintKp.publicKey.toBase58(), kind === 'pair' ? '(stock-pair mock)' : '(SOL pair)');
  console.log('Pool  :', pool.toBase58());
  console.log('Tx    :', link(sig));
}

async function enable(bbPct, dbPct, thresholdUnits, doneeArg) {
  const c = ctx();
  const bb = Math.round(Number(bbPct) * 100), db = Math.round(Number(dbPct) * 100);
  let donee = NO_DONEE;
  if (db > 0) donee = doneeArg && doneeArg !== 'new' ? new PublicKey(doneeArg) : Keypair.generate().publicKey;
  const unit = await unitOf(c.config);
  const threshold = new BN(Math.round(Number(thresholdUnits) * unit));
  const builder = program.methods.enable(bb, db, threshold).accountsStrict({
    creator: me.publicKey, pool: c.pool, config: c.config, baseMint: c.mint, quoteMint: c.quote, vault: c.vault, authority: c.authority,
    authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, c.quote, c.qprog),
    creatorQuoteAta: ata(me.publicKey, c.quote, c.qprog), donee, doneeQuoteAta: ata(donee, c.quote, c.qprog),
    dbcEventAuthority: dbcEvent, dbcProgram: DBC, tokenProgram: TOKEN, quoteTokenProgram: c.qprog,
    associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  });
  const sig = await sendIx(builder, [me], db > 0 && !c.isSol ? [ata(donee, c.quote, c.qprog)] : []);
  writeState({ ...c.s, donee: donee.toBase58() });
  const st = await poolState(c.pool);
  console.log(`Enabled: buyback ${bb / 100}%, donation ${db / 100}%, threshold ${c.fmt(threshold)} (unit ${c.fmt(unit)})`);
  console.log('Donation wallet :', db > 0 ? donee.toBase58() : '(none)');
  console.log('Pool creator is now:', st.creator.toBase58(), st.creator.equals(c.authority) ? '(program authority ✓)' : '(✗ unexpected)');
  console.log('Tx:', link(sig));
}

async function trade(rounds, amount) {
  const c = ctx();
  const amountIn = new BN(Math.round(Number(amount) * 10 ** c.dec));
  const myAta = ata(me.publicKey, c.mint);
  for (let i = 1; i <= Number(rounds); i++) {
    const buy = await dbc.pool.swap({ owner: me.publicKey, pool: c.pool, amountIn, minimumAmountOut: new BN(0), swapBaseForQuote: false, referralTokenAccount: null });
    await sendTx(buy, [me]);
    const bal = new BN((await connection.getTokenAccountBalance(myAta)).value.amount);
    const sell = await dbc.pool.swap({ owner: me.publicKey, pool: c.pool, amountIn: bal, minimumAmountOut: new BN(0), swapBaseForQuote: true, referralTokenAccount: null });
    await sendTx(sell, [me]);
    console.log(`round ${i}: bought and sold ${c.fmt(amountIn)}`);
  }
  await status();
}

function claimAccounts(c, st, caller = me.publicKey) {
  return {
    caller, vault: c.vault, authority: c.authority, pool: c.pool,
    authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, c.quote, c.qprog),
    baseVault: st.baseVault, quoteVault: st.quoteVault, baseMint: c.mint, quoteMint: c.quote,
    dbcConfig: c.config, dbcPoolAuthority: DBC_POOL_AUTHORITY, dbcEventAuthority: dbcEvent, dbcProgram: DBC,
    tokenProgram: TOKEN, quoteTokenProgram: c.qprog, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  };
}

async function claim() {
  const c = ctx();
  const st = await poolState(c.pool);
  const before = await vaultBalance(c);
  const sig = await program.methods.claimCurveFees().accountsStrict(claimAccounts(c, st)).rpc();
  const after = await vaultBalance(c);
  console.log(`Claimed ${c.fmt(after - before)} into the program authority. Tx: ${link(sig)}`);
}

function runAccounts(c, v, caller = me.publicKey) {
  return {
    caller, vault: c.vault, authority: c.authority,
    authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, c.quote, c.qprog),
    creator: v.creator, creatorBaseAta: ata(v.creator, c.mint), creatorQuoteAta: ata(v.creator, c.quote, c.qprog),
    donee: v.donee, doneeBaseAta: ata(v.donee, c.mint), doneeQuoteAta: ata(v.donee, c.quote, c.qprog),
    baseMint: c.mint, quoteMint: c.quote, tokenProgram: TOKEN, quoteTokenProgram: c.qprog,
    associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  };
}
const doneeWritable = (c, v) => (v.donationBps > 0 ? [v.donee, ata(v.donee, c.mint), ata(v.donee, c.quote, c.qprog)] : []);

// Same arithmetic as plan_amounts in lib.rs, used only to size min_tokens_out.
function expectedBuy(v, balance, now) {
  const unit = Number(v.unit);
  const splittable = Math.max(0, balance - Number(v.creatorOwed) - Number(v.doneeOwed));
  const spent = now - Number(v.dayStart) >= 86400 ? 0 : Number(v.daySpent);
  const cap = Math.min(unit, 5 * unit - spent);
  if (cap < unit / 100) return 0;
  const used = Math.min(splittable, Math.floor((cap * 10000) / v.buybackBps));
  return Math.floor((used * v.buybackBps) / 10000);
}

function curveAccounts(c, v, st, caller = me.publicKey) {
  return {
    common: runAccounts(c, v, caller), venuePool: c.pool, baseVault: st.baseVault, quoteVault: st.quoteVault,
    dbcConfig: c.config, dbcPoolAuthority: DBC_POOL_AUTHORITY, dbcEventAuthority: dbcEvent, dbcProgram: DBC,
  };
}

async function run() {
  const c = ctx();
  const v = await program.account.vault.fetch(c.vault);
  const supplyBefore = (await connection.getTokenSupply(c.mint)).value.amount;
  const creatorBefore = await walletQuote(c, v.creator);
  const doneeBefore = v.donationBps > 0 ? await walletQuote(c, v.donee) : 0;
  let sig, prepSig = null, buy = 0, minOut = new BN(0);
  if (v.buybackBps === 0) {
    sig = await sendIx(program.methods.distribute().accountsStrict({ common: runAccounts(c, v) }), [me], doneeWritable(c, v));
  } else {
    prepSig = await program.methods.prepareCurve().accountsStrict({ caller: me.publicKey, vault: c.vault, authority: c.authority, authorityQuoteAta: ata(c.authority, c.quote, c.qprog), curvePool: c.pool }).rpc();
    const prepSlot = (await connection.getTransaction(prepSig, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })).slot;
    while ((await connection.getSlot('confirmed')) < prepSlot + 26) await sleep(400);
    const raw = await dbc.state.getPool(c.pool);
    const st = raw.account || raw.poolState || raw;
    const config = await dbc.state.getPoolConfig(c.config);
    buy = expectedBuy(v, await vaultBalance(c), Math.floor(Date.now() / 1000));
    if (buy > 0) {
      const q = dbc.pool.swapQuote({
        virtualPool: raw, config, swapBaseForQuote: false, amountIn: new BN(buy), slippageBps: 100,
        hasReferral: false, eligibleForFirstSwapWithMinFee: false, currentPoint: new BN(Math.floor(Date.now() / 1000)),
      });
      minOut = q.minimumAmountOut || (q.outputAmount || q.amountOut).muln(9900).divn(10000);
    }
    sig = await sendIx(program.methods.executeCurve(minOut).accountsStrict(curveAccounts(c, v, st)), [me], doneeWritable(c, v));
  }
  const supplyAfter = (await connection.getTokenSupply(c.mint)).value.amount;
  const creatorAfter = await walletQuote(c, v.creator);
  if (prepSig) console.log(`Prepare : ${link(prepSig)}`);
  console.log(`${v.buybackBps === 0 ? 'Distribute' : 'Execute'} : ${link(sig)}`);
  if (v.buybackBps > 0) console.log(`Spent on buyback : ${c.fmt(buy)} (min tokens out ${minOut.toString()})`);
  console.log(`Token supply     : ${supplyBefore} -> ${supplyAfter} (burned ${new BN(supplyBefore).sub(new BN(supplyAfter)).toString()})`);
  if (v.donationBps > 0) console.log(`Donation wallet  : +${c.fmt((await walletQuote(c, v.donee)) - doneeBefore)}`);
  console.log(`Creator wallet   : ${creatorAfter - creatorBefore >= 0 ? '+' : ''}${c.fmt(creatorAfter - creatorBefore)}${c.isSol ? ' (the test wallet also paid the network fee)' : ''}`);
  await status();
}

async function status() {
  const c = ctx();
  const v = await program.account.vault.fetch(c.vault).catch(() => null);
  const st = await poolState(c.pool);
  console.log('--- status ---');
  console.log('Token            :', c.mint.toBase58(), c.isSol ? '(SOL pair)' : `(pair asset ${c.quote.toBase58()})`);
  console.log('Pool creator     :', st.creator.toBase58(), st.creator.equals(c.authority) ? '(program authority)' : '');
  console.log('Token supply     :', (await connection.getTokenSupply(c.mint)).value.uiAmountString);
  console.log('Vault holds      :', c.fmt(await vaultBalance(c)));
  if (v) {
    console.log('Rules            :', `buyback ${v.buybackBps / 100}%, donation ${v.donationBps / 100}%, threshold ${c.fmt(v.threshold)}, unit ${c.fmt(v.unit)}`);
    console.log('Wallets          :', `creator ${v.creator.toBase58()}`, v.donationBps > 0 ? `| donation ${v.donee.toBase58()}` : '');
    console.log('Totals           :', `runs ${v.runs} | claimed ${c.fmt(v.totalClaimed)} | bought for ${c.fmt(v.totalSpent)} | donated ${c.fmt(v.totalDonated)} | paid ${c.fmt(v.totalPaid)} | owed ${c.fmt(v.creatorOwed)}/${c.fmt(v.doneeOwed)} | burned ${v.totalBurned}`);
    console.log('Last run         :', new Date(Number(v.lastRun) * 1000).toISOString());
  } else console.log('Vault            : not enabled yet');
}

async function expectFail(label, fn, want) {
  try {
    await fn();
    console.log(`✗ ${label}: SUCCEEDED but must fail`);
  } catch (e) {
    const msg = [e.error && e.error.errorCode && e.error.errorCode.code, e.message, ...(e.logs || e.transactionLogs || [])].filter(Boolean).join(' | ');
    console.log(`${msg.includes(want) ? '✓' : '?'} ${label}: failed with ${msg.slice(0, 160)}`);
  }
}

async function negative() {
  const c = ctx();
  const v = await program.account.vault.fetch(c.vault);
  const st = await poolState(c.pool);
  const stranger = Keypair.generate();
  await sendTx(new Transaction().add(SystemProgram.transfer({ fromPubkey: me.publicKey, toPubkey: stranger.publicKey, lamports: 20_000_000 })), [me]);
  const prepAcc = (caller) => ({ caller, vault: c.vault, authority: c.authority, authorityQuoteAta: ata(c.authority, c.quote, c.qprog), curvePool: c.pool });

  await expectFail('enable a second time', () => program.methods.enable(5000, 0, new BN(1e8)).accountsStrict({
    creator: me.publicKey, pool: c.pool, config: c.config, baseMint: c.mint, quoteMint: c.quote, vault: c.vault, authority: c.authority,
    authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, c.quote, c.qprog), creatorQuoteAta: ata(me.publicKey, c.quote, c.qprog),
    donee: NO_DONEE, doneeQuoteAta: ata(NO_DONEE, c.quote, c.qprog), dbcEventAuthority: dbcEvent, dbcProgram: DBC,
    tokenProgram: TOKEN, quoteTokenProgram: c.qprog, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  }).rpc(), 'already in use');

  await expectFail('send the creator share to a different wallet', () => sendIx(program.methods.executeCurve(new BN(1)).accountsStrict({
    ...curveAccounts(c, v, st), common: { ...runAccounts(c, v), creator: stranger.publicKey, creatorBaseAta: ata(stranger.publicKey, c.mint), creatorQuoteAta: ata(stranger.publicKey, c.quote, c.qprog) },
  })), 'ConstraintAddress');

  await expectFail('send the donation share to a different wallet', () => sendIx(program.methods.distribute().accountsStrict({
    common: { ...runAccounts(c, v), donee: stranger.publicKey, doneeBaseAta: ata(stranger.publicKey, c.mint), doneeQuoteAta: ata(stranger.publicKey, c.quote, c.qprog) },
  }), [me], [stranger.publicKey]), 'ConstraintAddress');

  if (v.buybackBps === 0) {
    await expectFail('prepare a vault with 0% buyback', () => program.methods.prepareCurve().accountsStrict(prepAcc(me.publicKey)).rpc(), 'UseDistribute');
    await expectFail('stranger distributes before the 8-day public window', () => sendIx(program.methods.distribute().accountsStrict({ common: runAccounts(c, v, stranger.publicKey) }), [stranger], doneeWritable(c, v)), 'KeeperWindow');
    return;
  }
  await expectFail('distribute a vault that has a buyback share', () => sendIx(program.methods.distribute().accountsStrict({ common: runAccounts(c, v) }), [me], doneeWritable(c, v)), 'UseExecute');
  await expectFail('stranger prepares before the 8-day public window', () => program.methods.prepareCurve().accountsStrict(prepAcc(stranger.publicKey)).signers([stranger]).rpc(), 'KeeperWindow');
  await expectFail('execute without prepare', () => sendIx(program.methods.executeCurve(new BN(1)).accountsStrict(curveAccounts(c, v, st)), [me], doneeWritable(c, v)), 'NotPrepared');

  // The next two need a ready vault (claimed fees above the threshold); otherwise prepare stops with NotReady.
  const ready = await program.methods.prepareCurve().accountsStrict(prepAcc(me.publicKey)).rpc().then(() => true, (e) => { console.log(`  (skip early/other-caller checks: ${String(e.message || e).slice(0, 80)})`); return false; });
  if (ready) {
    await expectFail('execute right after prepare (under 25 slots)', () => sendIx(program.methods.executeCurve(new BN(0)).accountsStrict(curveAccounts(c, v, st)), [me], doneeWritable(c, v)), 'NotPrepared');
    await expectFail('another wallet executes the keeper prepare', () => sendIx(program.methods.executeCurve(new BN(0)).accountsStrict(curveAccounts(c, v, st, stranger.publicKey)), [stranger], doneeWritable(c, v)), 'NotYourPrepare');
  }
  const fakeAmm = Keypair.generate().publicKey;
  await expectFail('use a pool that is not the derived graduated pool', () => program.methods.prepareAmm()
    .accountsStrict({ caller: me.publicKey, vault: c.vault, authority: c.authority, authorityQuoteAta: ata(c.authority, c.quote, c.qprog), ammPool: fakeAmm }).rpc(), 'WrongAmmPool');
}


// ---------------------------------------------------------------------------
// Graduation path: DBC completion -> DAMM v2 migration -> surplus, LP fees and buybacks on the graduated pool
// ---------------------------------------------------------------------------

const { ComputeBudgetProgram, TransactionInstruction } = require('@solana/web3.js');
const cu = (n) => ComputeBudgetProgram.setComputeUnitLimit({ units: n });

async function unitsUsed(sig) {
  const t = await connection.getTransaction(sig, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 });
  return t && t.meta ? t.meta.computeUnitsConsumed : null;
}

/** The DAMM v2 pool Meteora creates at graduation (same derivation as graduated_pool_address in lib.rs). */
function ammInfo(c) {
  const [first, second] = [c.mint, c.quote].sort((x, y) => Buffer.compare(y.toBuffer(), x.toBuffer()));
  const pool = pda([Buffer.from('pool'), DAMM_MIGRATION_CONFIG.toBuffer(), first.toBuffer(), second.toBuffer()], DAMM);
  return { pool };
}

async function ammState(c) {
  const { pool } = ammInfo(c);
  const info = await connection.getAccountInfo(pool);
  if (!info) throw new Error(`Graduated pool ${pool.toBase58()} does not exist yet (run "migrate").`);
  const mintA = new PublicKey(info.data.subarray(168, 200)), mintB = new PublicKey(info.data.subarray(200, 232));
  const baseIsA = mintA.equals(c.mint);
  const progOf = (m) => (m.equals(c.mint) ? TOKEN : c.qprog);
  const vaultA = pda([Buffer.from('token_vault'), mintA.toBuffer(), pool.toBuffer()], DAMM);
  const vaultB = pda([Buffer.from('token_vault'), mintB.toBuffer(), pool.toBuffer()], DAMM);
  return { pool, mintA, mintB, baseIsA, progA: progOf(mintA), progB: progOf(mintB), vaultA, vaultB, owner: info.owner };
}

/** Position NFTs (Token-2022, amount 1) held by the vault authority, with their DAMM v2 position accounts. */
async function authorityPositions(c) {
  const res = await connection.getParsedTokenAccountsByOwner(c.authority, { programId: TOKEN_2022 });
  return res.value.filter((a) => a.account.data.parsed.info.tokenAmount.amount === '1').map((a) => {
    const nft = new PublicKey(a.account.data.parsed.info.mint);
    return { nftAccount: a.pubkey, nft, position: pda([Buffer.from('position'), nft.toBuffer()], DAMM) };
  });
}

async function graduate(amount = 95) {
  const c = ctx();
  const sdk = require('@meteora-ag/dynamic-bonding-curve-sdk');
  const config = await dbc.state.getPoolConfig(c.config);
  const threshold = new BN(config.migrationQuoteThreshold.toString());
  let st = await poolState(c.pool);
  console.log(`Curve: ${c.fmt(st.quoteReserve)} of ${c.fmt(threshold)} raised`);
  const amountIn = new BN(Math.round(Number(amount) * 10 ** c.dec));
  let sig;
  if (typeof dbc.pool.swap2 === 'function') {
    const mode = sdk.SwapMode ? sdk.SwapMode.PartialFill : 1;
    const tx = await dbc.pool.swap2({ owner: me.publicKey, pool: c.pool, amountIn, minimumAmountOut: new BN(0), swapMode: mode, swapBaseForQuote: false, referralTokenAccount: null, payer: me.publicKey });
    sig = await sendTx(tx, [me]);
  } else {
    const left = threshold.sub(new BN(st.quoteReserve.toString()));
    const tx = await dbc.pool.swap({ owner: me.publicKey, pool: c.pool, amountIn: left.muln(1020).divn(1000), minimumAmountOut: new BN(0), swapBaseForQuote: false, referralTokenAccount: null });
    sig = await sendTx(tx, [me]);
  }
  st = await poolState(c.pool);
  const done = new BN(st.quoteReserve.toString()).gte(threshold);
  console.log(`Bought: ${link(sig)}`);
  console.log(`Curve: ${c.fmt(st.quoteReserve)} of ${c.fmt(threshold)} | migration progress ${st.migrationProgress} | ${done ? 'COMPLETE ✓' : 'not complete ✗'}`);
  return done;
}

async function migrate() {
  const c = ctx();
  const st0 = await poolState(c.pool);
  if (Number(st0.isMigrated) === 1) { console.log('Already migrated.'); return true; }
  const m = dbc.migration;
  if (!m || typeof m.migrateToDammV2 !== 'function') throw new Error('This SDK version has no migration.migrateToDammV2: ' + Object.keys(m || {}).join(', '));
  if (typeof m.createDammV2MigrationMetadata === 'function') {
    try {
      const mt = await m.createDammV2MigrationMetadata({ payer: me.publicKey, virtualPool: c.pool, config: c.config });
      console.log('Migration metadata:', link(await sendTx(mt, [me])));
    } catch (e) { console.log('  (metadata step skipped:', String(e.message || e).slice(0, 90) + ')'); }
  }
  const r = await m.migrateToDammV2({ payer: me.publicKey, pool: c.pool, dammConfig: DAMM_MIGRATION_CONFIG });
  const tx = r.transaction || r;
  const extra = [r.firstPositionNftKeypair, r.secondPositionNftKeypair].filter(Boolean);
  tx.instructions = tx.instructions.filter((i) => i.programId.toBase58() !== 'ComputeBudget111111111111111111111111111111');
  tx.instructions.unshift(cu(1_000_000));
  const sig = await sendTx(tx, [me, ...extra]);
  const st = await poolState(c.pool);
  const a = await ammState(c).catch(() => null);
  console.log(`Migrated: ${link(sig)}`);
  console.log(`isMigrated ${st.isMigrated} | graduated pool ${a ? a.pool.toBase58() + ' ✓ (derived address exists, owner DAMM v2)' : 'NOT FOUND ✗'}`);
  const pos = await authorityPositions(c);
  console.log(`Position NFTs held by the program authority: ${pos.length}${pos.length ? ' ✓' : ' ✗ (creator LP did not go to the authority)'}`);
  pos.forEach((p) => console.log(`  NFT ${p.nft.toBase58()} -> position ${p.position.toBase58()}`));
  return !!a && pos.length > 0;
}

async function surplus() {
  const c = ctx();
  const st = await poolState(c.pool);
  const before = await vaultBalance(c);
  const sig = await program.methods.claimCurveSurplus().accountsStrict(claimAccounts(c, st)).rpc();
  const after = await vaultBalance(c);
  console.log(`Curve surplus claimed: ${c.fmt(after - before)} | ${link(sig)}`);
  return true;
}

function ammSwapIx(a, user, amountIn, baseToQuote) {
  const c = ctx();
  const userBase = ata(user, c.mint), userQuote = ata(user, c.quote, c.qprog);
  const [inAcc, outAcc] = baseToQuote ? [userBase, userQuote] : [userQuote, userBase];
  const data = Buffer.concat([IX_SWAP, new BN(amountIn).toArrayLike(Buffer, 'le', 8), new BN(0).toArrayLike(Buffer, 'le', 8)]);
  const k = (pubkey, w = false, s = false) => ({ pubkey, isWritable: w, isSigner: s });
  return new TransactionInstruction({ programId: DAMM, data, keys: [
    k(DAMM_POOL_AUTHORITY), k(a.pool, true), k(inAcc, true), k(outAcc, true), k(a.vaultA, true), k(a.vaultB, true),
    k(a.mintA), k(a.mintB), k(user, false, true), k(a.progA), k(a.progB), k(DAMM), k(dammEvent), k(DAMM),
  ] });
}

async function ammtrade(rounds = 2, amount = 5) {
  const c = ctx();
  const a = await ammState(c);
  const amountIn = Math.round(Number(amount) * 10 ** c.dec);
  const userBase = ata(me.publicKey, c.mint);
  for (let i = 1; i <= Number(rounds); i++) {
    await sendTx(new Transaction().add(cu(400_000), ammSwapIx(a, me.publicKey, amountIn, false)), [me]);
    const bal = Number((await connection.getTokenAccountBalance(userBase)).value.amount);
    const sig = await sendTx(new Transaction().add(cu(400_000), ammSwapIx(a, me.publicKey, Math.floor(bal / 2), true)), [me]);
    console.log(`AMM round ${i}: bought with ${c.fmt(amountIn)}, sold half | ${link(sig)}`);
  }
  return true;
}

async function ammclaim() {
  const c = ctx();
  const a = await ammState(c);
  const pos = await authorityPositions(c);
  if (!pos.length) throw new Error('The program authority holds no DAMM v2 position NFT.');
  const before = await vaultBalance(c);
  const baseBefore = Number((await connection.getTokenAccountBalance(ata(c.authority, c.mint))).value.amount);
  let ok = true;
  for (const p of pos) {
    const b = program.methods.claimAmmFees().accountsStrict({
      caller: me.publicKey, vault: c.vault, authority: c.authority, curvePool: c.pool, ammPool: a.pool, position: p.position, positionNftAccount: p.nftAccount,
      authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, c.quote, c.qprog), tokenAVault: a.vaultA, tokenBVault: a.vaultB,
      baseMint: c.mint, quoteMint: c.quote, dammPoolAuthority: DAMM_POOL_AUTHORITY, dammEventAuthority: dammEvent, dammProgram: DAMM,
      tokenProgram: TOKEN, quoteTokenProgram: c.qprog, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
    });
    try {
      const ix = await b.instruction();
      const sig = await sendTx(new Transaction().add(cu(600_000), ix), [me]);
      console.log(`claim_amm_fees (position ${p.position.toBase58().slice(0, 6)}…): ${link(sig)} | CU ${await unitsUsed(sig)}`);
    } catch (e) { ok = false; console.log(`claim_amm_fees FAILED for ${p.position.toBase58()}: ${e.message}`); if (e.logs) console.log(e.logs.slice(-8).join('\n')); }
  }
  const after = await vaultBalance(c);
  const baseAfter = Number((await connection.getTokenAccountBalance(ata(c.authority, c.mint))).value.amount);
  console.log(`LP fees claimed: ${c.fmt(after - before)} of the pair asset, ${baseAfter - baseBefore} base-token units`);
  return ok;
}

async function ammrun() {
  const c = ctx();
  const a = await ammState(c);
  let v = await program.account.vault.fetch(c.vault);
  const wait = Number(v.lastRun) + 600 - Math.floor(Date.now() / 1000);
  if (wait > 0) { console.log(`Waiting ${wait}s for the 10-minute gap since the last run…`); await sleep((wait + 5) * 1000); }
  v = await program.account.vault.fetch(c.vault);
  const supplyBefore = (await connection.getTokenSupply(c.mint)).value.amount;
  const common = runAccounts(c, v);
  let sig;
  if (v.buybackBps === 0) {
    sig = await sendIx(program.methods.distribute().accountsStrict({ common }), [me], doneeWritable(c, v));
  } else {
    const prep = await program.methods.prepareAmm().accountsStrict({ caller: me.publicKey, vault: c.vault, authority: c.authority, authorityQuoteAta: ata(c.authority, c.quote, c.qprog), ammPool: a.pool }).rpc();
    const prepSlot = (await connection.getTransaction(prep, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })).slot;
    console.log(`prepare_amm: ${link(prep)}`);
    while ((await connection.getSlot('confirmed')) < prepSlot + 26) await sleep(400);
    const ix = await program.methods.executeAmm(new BN(0)).accountsStrict({
      common, ammPool: a.pool, tokenAVault: a.vaultA, tokenBVault: a.vaultB,
      dammPoolAuthority: DAMM_POOL_AUTHORITY, dammEventAuthority: dammEvent, dammProgram: DAMM,
    }).instruction();
    const w = doneeWritable(c, v);
    ix.keys = ix.keys.map((k) => (w.some((x) => x.equals(k.pubkey)) ? { ...k, isWritable: true } : k));
    sig = await sendTx(new Transaction().add(cu(1_400_000), ix), [me]);
  }
  const used = await unitsUsed(sig);
  const supplyAfter = (await connection.getTokenSupply(c.mint)).value.amount;
  console.log(`${v.buybackBps === 0 ? 'distribute' : 'execute_amm'}: ${link(sig)} | CU used ${used} of 1,400,000`);
  console.log(`Token supply: ${supplyBefore} -> ${supplyAfter} (burned ${new BN(supplyBefore).sub(new BN(supplyAfter)).toString()})`);
  await status();
  return true;
}

async function gradtest() {
  const steps = [
    ['graduate (curve completes)', () => graduate()],
    ['claim_curve_fees', async () => { await claim(); return true; }],
    ['migrate to DAMM v2 (+ position NFT held by authority)', migrate],
    ['claim_curve_surplus', surplus],
    ['trade on the graduated pool', () => ammtrade(2, 5)],
    ['claim_amm_fees (read-only pool, position check)', ammclaim],
    ['prepare_amm + execute_amm (CU budget, burn)', ammrun],
  ];
  const out = [];
  for (const [name, fn] of steps) {
    console.log(`\n=== ${name} ===`);
    try { const ok = await fn(); out.push([name, ok !== false]); } catch (e) {
      out.push([name, false]); console.log('FAILED:', e.message); if (e.logs) console.log(e.logs.slice(-10).join('\n'));
    }
  }
  console.log('\n=== summary ===');
  out.forEach(([n, ok]) => console.log(`${ok ? '✓' : '✗'} ${n}`));
}

(async () => {
  const [cmd, a, b, d, e] = process.argv.slice(2);
  const cmds = { pairsetup, create: () => create(a), enable: () => enable(a, b, d, e), trade: () => trade(a || 1, b || 0.5), claim, run, status, negative, graduate: () => graduate(a || 95), migrate, surplus, ammtrade: () => ammtrade(a || 2, b || 5), ammclaim, ammrun, gradtest };
  if (!cmds[cmd]) { console.log('usage: pairsetup | create [sol|pair] | enable <buyback%> <donation%> <threshold> [donee|new] | trade <rounds> <amount> | claim | run | status | negative | graduate [amount] | migrate | surplus | ammtrade <rounds> <amount> | ammclaim | ammrun | gradtest'); process.exit(1); }
  console.log(`Test wallet ${me.publicKey.toBase58()} on devnet${process.env.TEST_TAG ? ` [${process.env.TEST_TAG}]` : ''}`);
  await cmds[cmd]();
})().catch((e) => {
  console.error('ERROR:', e.message);
  if (e.logs) console.error(e.logs.slice(-15).join('\n'));
  process.exit(1);
});
