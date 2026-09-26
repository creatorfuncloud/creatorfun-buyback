#!/usr/bin/env node
/**
 * CREATORFUN buyback — devnet end-to-end test.
 *
 * Runs against the devnet build of the program (feature "devnet": CREATORFUN config 6DNT..., test wallet 5KQ2...).
 * The test wallet is both the token creator and the keeper, like during mainnet probation.
 *
 *   node devnet-test.js create                 create a test token on the devnet CREATORFUN config
 *   node devnet-test.js enable <bps> <sol>     turn buyback on (e.g. enable 5000 0.1)
 *   node devnet-test.js trade <rounds> <sol>   buy then sell <sol> per round to generate fees
 *   node devnet-test.js claim                  claim_curve_fees (anyone can call)
 *   node devnet-test.js run                    prepare_curve, wait 2+ slots, execute_curve
 *   node devnet-test.js status                 vault, balances, token supply
 *   node devnet-test.js negative               actions that MUST fail
 *
 * Needs: OPERATOR_PRIVATE_KEY and RPC_URL in the environment (or in the .env file set by ENV_FILE),
 * the program IDL (IDL_PATH, default target/idl/creatorfun_buyback.json) and the npm packages below.
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
const CONFIG = new PublicKey('6DNThd3xWokjwqVerywpKLqRASmt5ygfFnMz2iRuhv72'); // devnet CREATORFUN config
const DBC = new PublicKey('dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN');
const DBC_POOL_AUTHORITY = new PublicKey('FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM');
const DAMM = new PublicKey('cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG');
const WSOL = new PublicKey('So11111111111111111111111111111111111111112');
const TOKEN = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA');
const ATA_PROGRAM = new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL');
const IDL_PATH = process.env.IDL_PATH || path.join(__dirname, '..', 'target', 'idl', 'creatorfun_buyback.json');
const STATE = path.join(__dirname, 'devnet-state.json');
const RESERVE = 5_000_000;

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
const ata = (owner, mint) => pda([owner.toBuffer(), TOKEN.toBuffer(), mint.toBuffer()], ATA_PROGRAM);
const dbcEvent = pda([Buffer.from('__event_authority')], DBC);
const sol = (l) => (Number(l) / LAMPORTS_PER_SOL).toFixed(6);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const readState = () => (fs.existsSync(STATE) ? JSON.parse(fs.readFileSync(STATE, 'utf8')) : {});
const writeState = (s) => fs.writeFileSync(STATE, JSON.stringify(s, null, 2));
const link = (sig) => `https://solscan.io/tx/${sig}?cluster=devnet`;

function ctx() {
  const s = readState();
  if (!s.mint) throw new Error('Run "create" first.');
  const mint = new PublicKey(s.mint);
  const pool = new PublicKey(s.pool);
  const vault = pda([Buffer.from('vault'), pool.toBuffer()], PROGRAM_ID);
  const authority = pda([Buffer.from('authority'), pool.toBuffer()], PROGRAM_ID);
  return { s, mint, pool, vault, authority };
}

async function poolState(pool) {
  const raw = await dbc.state.getPool(pool);
  return raw.account || raw.poolState || raw;
}

async function sendTx(tx, signers) {
  const { blockhash } = await connection.getLatestBlockhash('confirmed');
  tx.recentBlockhash = blockhash;
  tx.feePayer = me.publicKey;
  return sendAndConfirmTransaction(connection, tx, signers, { commitment: 'confirmed' });
}

// ---------------------------------------------------------------------------

async function create() {
  if (readState().mint) throw new Error(`Test token already exists: ${readState().mint}`);
  const mintKp = Keypair.generate();
  const tx = await dbc.creator.createPool({
    name: 'CF Buyback Test', symbol: 'CFBBT', uri: 'https://creatorfun.cloud/m/devnet-test.json',
    payer: me.publicKey, poolCreator: me.publicKey, config: CONFIG, baseMint: mintKp.publicKey,
  });
  const sig = await sendTx(tx, [me, mintKp]);
  // Find the pool: the DBC pool account that holds this base mint under our config.
  const [a, b] = [mintKp.publicKey, WSOL].sort((x, y) => Buffer.compare(y.toBuffer(), x.toBuffer()));
  const pool = pda([Buffer.from('pool'), CONFIG.toBuffer(), a.toBuffer(), b.toBuffer()], DBC);
  const info = await connection.getAccountInfo(pool);
  if (!info || !info.owner.equals(DBC)) throw new Error(`Pool not found at ${pool.toBase58()} (derivation mismatch)`);
  writeState({ mint: mintKp.publicKey.toBase58(), pool: pool.toBase58(), createSig: sig });
  console.log('Token :', mintKp.publicKey.toBase58());
  console.log('Pool  :', pool.toBase58());
  console.log('Tx    :', link(sig));
}

async function enable(bps, thresholdSol) {
  const { mint, pool, vault, authority } = ctx();
  const sig = await program.methods
    .enable(Number(bps), new BN(Math.round(Number(thresholdSol) * LAMPORTS_PER_SOL)))
    .accountsStrict({
      creator: me.publicKey, pool, config: CONFIG, baseMint: mint, vault, authority,
      authorityBaseAta: ata(authority, mint), dbcEventAuthority: dbcEvent, dbcProgram: DBC,
      tokenProgram: TOKEN, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
    })
    .rpc();
  const st = await poolState(pool);
  console.log('Enabled. Pool creator is now:', st.creator.toBase58(), st.creator.equals(authority) ? '(buyback authority ✓)' : '(✗ unexpected)');
  console.log('Tx:', link(sig));
}

async function trade(rounds, amountSol) {
  const { mint, pool } = ctx();
  const lamports = new BN(Math.round(Number(amountSol) * LAMPORTS_PER_SOL));
  const myAta = ata(me.publicKey, mint);
  for (let i = 1; i <= Number(rounds); i++) {
    const buy = await dbc.pool.swap({ owner: me.publicKey, pool, amountIn: lamports, minimumAmountOut: new BN(0), swapBaseForQuote: false, referralTokenAccount: null });
    await sendTx(buy, [me]);
    const bal = new BN((await connection.getTokenAccountBalance(myAta)).value.amount);
    const sell = await dbc.pool.swap({ owner: me.publicKey, pool, amountIn: bal, minimumAmountOut: new BN(0), swapBaseForQuote: true, referralTokenAccount: null });
    await sendTx(sell, [me]);
    console.log(`round ${i}: bought and sold ${amountSol} SOL`);
  }
  await status();
}

function claimAccounts(c, st) {
  return {
    caller: me.publicKey, vault: c.vault, authority: c.authority, pool: c.pool,
    authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, WSOL),
    baseVault: st.baseVault, quoteVault: st.quoteVault, baseMint: c.mint, quoteMint: WSOL,
    dbcConfig: CONFIG, dbcPoolAuthority: DBC_POOL_AUTHORITY, dbcEventAuthority: dbcEvent, dbcProgram: DBC,
    tokenProgram: TOKEN, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  };
}

async function claim() {
  const c = ctx();
  const st = await poolState(c.pool);
  const before = await connection.getBalance(c.authority);
  const sig = await program.methods.claimCurveFees().accountsStrict(claimAccounts(c, st)).rpc();
  const after = await connection.getBalance(c.authority);
  console.log(`Claimed ${sol(after - before)} SOL into the buyback authority. Tx: ${link(sig)}`);
}

function runAccounts(c, v) {
  return {
    caller: me.publicKey, vault: c.vault, authority: c.authority,
    authorityBaseAta: ata(c.authority, c.mint), authorityQuoteAta: ata(c.authority, WSOL),
    creator: v.creator, creatorBaseAta: ata(v.creator, c.mint), baseMint: c.mint, quoteMint: WSOL,
    tokenProgram: TOKEN, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  };
}

// Same arithmetic as plan_amounts in lib.rs, used only to size min_tokens_out.
function expectedBuy(v, authorityLamports, now) {
  const splittable = Math.max(0, authorityLamports - RESERVE - Number(v.creatorOwed));
  const spent = now - Number(v.dayStart) >= 86400 ? 0 : Number(v.daySpent);
  const cap = Math.min(1e9, 5e9 - spent);
  const used = Math.min(splittable, Math.floor((cap * 10000) / v.buybackBps));
  return Math.floor((used * v.buybackBps) / 10000);
}

async function run() {
  const c = ctx();
  const v = await program.account.vault.fetch(c.vault);
  const prepSig = await program.methods.prepareCurve().accountsStrict({ caller: me.publicKey, vault: c.vault, curvePool: c.pool }).rpc();
  const prepSlot = (await connection.getTransaction(prepSig, { commitment: 'confirmed', maxSupportedTransactionVersion: 0 })).slot;
  while ((await connection.getSlot('confirmed')) < prepSlot + 2) await sleep(300);

  const raw = await dbc.state.getPool(c.pool);
  const st = raw.account || raw.poolState || raw;
  const config = await dbc.state.getPoolConfig(CONFIG);
  const buy = expectedBuy(v, await connection.getBalance(c.authority), Math.floor(Date.now() / 1000));
  let minOut = new BN(1);
  if (buy > 0) {
    const q = dbc.pool.swapQuote({
      virtualPool: raw, config, swapBaseForQuote: false, amountIn: new BN(buy), slippageBps: 100,
      hasReferral: false, eligibleForFirstSwapWithMinFee: false, currentPoint: new BN(Math.floor(Date.now() / 1000)),
    });
    minOut = q.minimumAmountOut || (q.outputAmount || q.amountOut).muln(9900).divn(10000);
  }
  const supplyBefore = (await connection.getTokenSupply(c.mint)).value.amount;
  const creatorBefore = await connection.getBalance(v.creator);
  const sig = await program.methods.executeCurve(minOut)
    .accountsStrict({
      common: runAccounts(c, v), venuePool: c.pool, baseVault: st.baseVault, quoteVault: st.quoteVault,
      dbcConfig: CONFIG, dbcPoolAuthority: DBC_POOL_AUTHORITY, dbcEventAuthority: dbcEvent, dbcProgram: DBC,
    })
    .rpc();
  const supplyAfter = (await connection.getTokenSupply(c.mint)).value.amount;
  const creatorAfter = await connection.getBalance(v.creator);
  console.log(`Prepare : ${link(prepSig)}`);
  console.log(`Execute : ${link(sig)}`);
  console.log(`Spent on buyback : ${sol(buy)} SOL (min tokens out ${minOut.toString()})`);
  console.log(`Token supply     : ${supplyBefore} -> ${supplyAfter} (burned ${new BN(supplyBefore).sub(new BN(supplyAfter)).toString()})`);
  console.log(`Creator wallet   : ${creatorAfter - creatorBefore >= 0 ? '+' : ''}${sol(creatorAfter - creatorBefore)} SOL (the test wallet also paid the network fee)`);
  await status();
}

async function status() {
  const c = ctx();
  const v = await program.account.vault.fetch(c.vault).catch(() => null);
  const st = await poolState(c.pool);
  console.log('--- status ---');
  console.log('Token            :', c.mint.toBase58());
  console.log('Pool creator     :', st.creator.toBase58(), st.creator.equals(c.authority) ? '(buyback authority)' : '');
  console.log('Token supply     :', (await connection.getTokenSupply(c.mint)).value.uiAmountString);
  console.log('Authority SOL    :', sol(await connection.getBalance(c.authority)), `(reserve ${sol(RESERVE)})`);
  console.log('Test wallet SOL  :', sol(await connection.getBalance(me.publicKey)));
  if (v) {
    console.log('Rules            :', `${v.buybackBps / 100}% buyback, threshold ${sol(v.thresholdLamports)} SOL, creator ${v.creator.toBase58()}`);
    console.log('Runs             :', v.runs.toString(), '| claimed', sol(v.totalClaimedLamports), '| bought for', sol(v.totalBuybackLamports), '| paid', sol(v.totalPaidLamports), '| owed', sol(v.creatorOwed), '| burned', v.totalBurned.toString());
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

  await expectFail('enable a second time', () => program.methods.enable(5000, new BN(1e8)).accountsStrict({
    creator: me.publicKey, pool: c.pool, config: CONFIG, baseMint: c.mint, vault: c.vault, authority: c.authority,
    authorityBaseAta: ata(c.authority, c.mint), dbcEventAuthority: dbcEvent, dbcProgram: DBC,
    tokenProgram: TOKEN, associatedTokenProgram: ATA_PROGRAM, systemProgram: SystemProgram.programId,
  }).rpc(), 'already in use');

  await expectFail('stranger prepares before the 8-day public window', () => program.methods.prepareCurve()
    .accountsStrict({ caller: stranger.publicKey, vault: c.vault, curvePool: c.pool }).signers([stranger]).rpc(), 'KeeperWindow');

  await expectFail('execute without prepare', () => program.methods.executeCurve(new BN(1)).accountsStrict({
    common: runAccounts(c, v), venuePool: c.pool, baseVault: st.baseVault, quoteVault: st.quoteVault,
    dbcConfig: CONFIG, dbcPoolAuthority: DBC_POOL_AUTHORITY, dbcEventAuthority: dbcEvent, dbcProgram: DBC,
  }).rpc(), 'NotPrepared');

  await expectFail('send the creator share to a different wallet', () => program.methods.executeCurve(new BN(1)).accountsStrict({
    common: { ...runAccounts(c, v), creator: stranger.publicKey, creatorBaseAta: ata(stranger.publicKey, c.mint) },
    venuePool: c.pool, baseVault: st.baseVault, quoteVault: st.quoteVault,
    dbcConfig: CONFIG, dbcPoolAuthority: DBC_POOL_AUTHORITY, dbcEventAuthority: dbcEvent, dbcProgram: DBC,
  }).rpc(), 'ConstraintAddress');

  const fakeAmm = Keypair.generate().publicKey;
  await expectFail('use a pool that is not the derived graduated pool', () => program.methods.prepareAmm()
    .accountsStrict({ caller: me.publicKey, vault: c.vault, ammPool: fakeAmm }).rpc(), 'WrongAmmPool');

  await expectFail('stranger triggers a claim (allowed) but cannot receive the fees', async () => {
    const ix = await program.methods.claimCurveFees().accountsStrict(claimAccounts(c, st)).instruction();
    ix.keys = ix.keys.map((k) => (k.pubkey.equals(me.publicKey) ? { ...k, pubkey: stranger.publicKey } : k));
    // The program lets anyone trigger a claim, but the fees can only land in the buyback authority:
    const before = await connection.getBalance(stranger.publicKey);
    await sendAndConfirmTransaction(connection, new Transaction().add(ix), [stranger]);
    const after = await connection.getBalance(stranger.publicKey);
    if (after > before) throw new Error('stranger RECEIVED funds'); // would be a real bug
    throw new Error('ExpectedClaimOnlyToAuthority');
  }, 'ExpectedClaimOnlyToAuthority');
}

(async () => {
  const [cmd, a, b] = process.argv.slice(2);
  const cmds = { create, enable: () => enable(a, b), trade: () => trade(a || 1, b || 0.5), claim, run, status, negative };
  if (!cmds[cmd]) { console.log('usage: create | enable <bps> <sol> | trade <rounds> <sol> | claim | run | status | negative'); process.exit(1); }
  console.log(`Test wallet ${me.publicKey.toBase58()} on devnet`);
  await cmds[cmd]();
})().catch((e) => {
  console.error('ERROR:', e.message);
  if (e.logs) console.error(e.logs.slice(-15).join('\n'));
  process.exit(1);
});
