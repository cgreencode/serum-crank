#![deny(unaligned_references)]

use std::borrow::Cow;
use std::cmp::{max, min};
use std::collections::BTreeSet;
use std::convert::identity;
use std::mem::size_of;

use std::sync::{Arc, Mutex};
use std::{thread, time};

use anyhow::{format_err, Result};
use clap::Clap;
use debug_print::debug_println;

use log::{error, info};
use rand::rngs::OsRng;
use safe_transmute::{
    guard::SingleManyGuard,
    to_bytes::{transmute_one_to_bytes, transmute_to_bytes},
    transmute_many, transmute_many_pedantic, transmute_one_pedantic,
};
use sloggers::file::FileLoggerBuilder;
use sloggers::types::Severity;
use sloggers::Build;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use warp::Filter;
pub mod config;
use serum_common::client::rpc::{
    send_txn, simulate_transaction,
};

use serum_dex::instruction::{
    cancel_order_by_client_order_id as cancel_order_by_client_order_id_ix,
    close_open_orders as close_open_orders_ix, init_open_orders as init_open_orders_ix,
    MarketInstruction, NewOrderInstructionV3,
};

use serum_dex::state::gen_vault_signer_key;
use serum_dex::state::Event;
use serum_dex::state::EventQueueHeader;
use serum_dex::state::QueueHeader;
use serum_dex::state::{AccountFlag, Market, MarketState, MarketStateV2};

pub mod crank;

pub fn with_logging<F: FnOnce()>(_to: &str, fnc: F) {
    fnc();
}

fn read_keypair_file(s: &str) -> Result<Keypair> {
    solana_sdk::signature::read_keypair_file(s)
        .map_err(|_| format_err!("failed to read keypair from {}", s))
}

#[derive(Clap, Debug)]
pub struct Opts {
    #[clap(short, long, default_value = "https://solana-api.projectserum.com")]
    pub url: String,
    #[clap(subcommand)]
    pub command: Command,
}

impl Opts {
    fn client(&self) -> RpcClient {
        RpcClient::new(self.url.to_string())
    }
}

#[derive(Clap, Debug)]
pub enum Command {
    ConsumeEvents {
        #[clap(long, short)]
        dex_program_id: Pubkey,
        #[clap(long)]
        payer: String,
        #[clap(long, short)]
        market: Pubkey,
        #[clap(long, short)]
        coin_wallet: Pubkey,
        #[clap(long, short)]
        pc_wallet: Pubkey,
        #[clap(long, short)]
        num_workers: usize,
        #[clap(long, short)]
        events_per_worker: usize,
        #[clap(long)]
        num_accounts: Option<usize>,
        #[clap(long)]
        log_directory: String,
        #[clap(long)]
        max_q_length: Option<u64>,
        #[clap(long)]
        max_wait_for_events_delay: Option<u64>,
    },
    PrintEventQueue {
        dex_program_id: Pubkey,
        market: Pubkey,
    },
}

pub async fn start(opts: Opts) -> Result<()> {
    let client = opts.client();

    match opts.command {
        Command::ConsumeEvents {
            ref dex_program_id,
            ref payer,
            ref market,
            ref coin_wallet,
            ref pc_wallet,
            num_workers,
            events_per_worker,
            ref num_accounts,
            ref log_directory,
            ref max_q_length,
            ref max_wait_for_events_delay,
        } => {
            init_logger(log_directory);
            consume_events_loop(
                &opts,
                &dex_program_id,
                &payer,
                &market,
                &coin_wallet,
                &pc_wallet,
                num_workers,
                events_per_worker,
                num_accounts.unwrap_or(32),
                max_q_length.unwrap_or(1),
                max_wait_for_events_delay.unwrap_or(60),
            )?;
        }
        Command::PrintEventQueue {
            ref dex_program_id,
            ref market,
        } => {
            let market_keys = get_keys_for_market(&client, dex_program_id, &market)?;
            let event_q_data = client.get_account_data(&market_keys.event_q)?;
            let inner: Cow<[u64]> = remove_dex_account_padding(&event_q_data)?;
            let (header, events_seg0, events_seg1) = parse_event_queue(&inner)?;
            println!("Header:\n{:#x?}", header);
            println!("Seg0:\n{:#x?}", events_seg0);
            println!("Seg1:\n{:#x?}", events_seg1);
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct MarketPubkeys {
    pub market: Box<Pubkey>,
    pub req_q: Box<Pubkey>,
    pub event_q: Box<Pubkey>,
    pub bids: Box<Pubkey>,
    pub asks: Box<Pubkey>,
    pub coin_vault: Box<Pubkey>,
    pub pc_vault: Box<Pubkey>,
    pub vault_signer_key: Box<Pubkey>,
}

#[cfg(target_endian = "little")]
fn remove_dex_account_padding<'a>(data: &'a [u8]) -> Result<Cow<'a, [u64]>> {
    use serum_dex::state::{ACCOUNT_HEAD_PADDING, ACCOUNT_TAIL_PADDING};
    let head = &data[..ACCOUNT_HEAD_PADDING.len()];
    if data.len() < ACCOUNT_HEAD_PADDING.len() + ACCOUNT_TAIL_PADDING.len() {
        return Err(format_err!(
            "dex account length {} is too small to contain valid padding",
            data.len()
        ));
    }
    if head != ACCOUNT_HEAD_PADDING {
        return Err(format_err!("dex account head padding mismatch"));
    }
    let tail = &data[data.len() - ACCOUNT_TAIL_PADDING.len()..];
    if tail != ACCOUNT_TAIL_PADDING {
        return Err(format_err!("dex account tail padding mismatch"));
    }
    let inner_data_range = ACCOUNT_HEAD_PADDING.len()..(data.len() - ACCOUNT_TAIL_PADDING.len());
    let inner: &'a [u8] = &data[inner_data_range];
    let words: Cow<'a, [u64]> = match transmute_many_pedantic::<u64>(inner) {
        Ok(word_slice) => Cow::Borrowed(word_slice),
        Err(transmute_error) => {
            let word_vec = transmute_error.copy().map_err(|e| e.without_src())?;
            Cow::Owned(word_vec)
        }
    };
    Ok(words)
}

#[cfg(target_endian = "little")]
fn get_keys_for_market<'a>(
    client: &'a RpcClient,
    program_id: &'a Pubkey,
    market: &'a Pubkey,
) -> Result<MarketPubkeys> {
    let account_data: Vec<u8> = client.get_account_data(&market)?;
    let words: Cow<[u64]> = remove_dex_account_padding(&account_data)?;
    let market_state: MarketState = {
        let account_flags = Market::account_flags(&account_data)?;
        if account_flags.intersects(AccountFlag::Permissioned) {
            let state = transmute_one_pedantic::<MarketStateV2>(transmute_to_bytes(&words))
                .map_err(|e| e.without_src())?;
            state.inner
        } else {
            transmute_one_pedantic::<MarketState>(transmute_to_bytes(&words))
                .map_err(|e| e.without_src())?
        }
    };
    market_state.check_flags()?;
    let vault_signer_key =
        gen_vault_signer_key(market_state.vault_signer_nonce, market, program_id)?;
    assert_eq!(
        transmute_to_bytes(&identity(market_state.own_address)),
        market.as_ref()
    );
    Ok(MarketPubkeys {
        market: Box::new(*market),
        req_q: Box::new(Pubkey::new(transmute_one_to_bytes(&identity(
            market_state.req_q,
        )))),
        event_q: Box::new(Pubkey::new(transmute_one_to_bytes(&identity(
            market_state.event_q,
        )))),
        bids: Box::new(Pubkey::new(transmute_one_to_bytes(&identity(
            market_state.bids,
        )))),
        asks: Box::new(Pubkey::new(transmute_one_to_bytes(&identity(
            market_state.asks,
        )))),
        coin_vault: Box::new(Pubkey::new(transmute_one_to_bytes(&identity(
            market_state.coin_vault,
        )))),
        pc_vault: Box::new(Pubkey::new(transmute_one_to_bytes(&identity(
            market_state.pc_vault,
        )))),
        vault_signer_key: Box::new(vault_signer_key),
    })
}

fn parse_event_queue(data_words: &[u64]) -> Result<(EventQueueHeader, &[Event], &[Event])> {
    let (header_words, event_words) = data_words.split_at(size_of::<EventQueueHeader>() >> 3);
    let header: EventQueueHeader =
        transmute_one_pedantic(transmute_to_bytes(header_words)).map_err(|e| e.without_src())?;
    let events: &[Event] = transmute_many::<_, SingleManyGuard>(transmute_to_bytes(event_words))
        .map_err(|e| e.without_src())?;
    let (tail_seg, head_seg) = events.split_at(header.head() as usize);
    let head_len = head_seg.len().min(header.count() as usize);
    let tail_len = header.count() as usize - head_len;
    Ok((header, &head_seg[..head_len], &tail_seg[..tail_len]))
}

fn hash_accounts(val: &[u64; 4]) -> u64 {
    val.iter().fold(0, |a, b| b.wrapping_add(a))
}

fn init_logger(log_directory: &str) {
    let path = std::path::Path::new(log_directory);
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    let mut builder = FileLoggerBuilder::new(log_directory);
    builder.level(Severity::Info).rotate_size(8 * 1024 * 1024);
    let log = builder.build().unwrap();
    let _guard = slog_scope::set_global_logger(log);
    _guard.cancel_reset();
    slog_stdlog::init().unwrap();
}

fn consume_events_loop(
    opts: &Opts,
    program_id: &Pubkey,
    payer_path: &String,
    market: &Pubkey,
    coin_wallet: &Pubkey,
    pc_wallet: &Pubkey,
    num_workers: usize,
    events_per_worker: usize,
    num_accounts: usize,
    max_q_length: u64,
    max_wait_for_events_delay: u64,
) -> Result<()> {
    info!("Getting market keys ...");
    let client = opts.client();
    let market_keys = get_keys_for_market(&client, &program_id, &market)?;
    info!("{:#?}", market_keys);
    let pool = threadpool::ThreadPool::new(num_workers);
    let max_slot_height_mutex = Arc::new(Mutex::new(0_u64));
    let mut last_cranked_at = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(max_wait_for_events_delay))
        .unwrap_or(std::time::Instant::now());

    loop {
        thread::sleep(time::Duration::from_millis(1000));

        let loop_start = std::time::Instant::now();
        let start_time = std::time::Instant::now();
        let event_q_value_and_context =
            client.get_account_with_commitment(&market_keys.event_q, CommitmentConfig::recent())?;
        let event_q_slot = event_q_value_and_context.context.slot;
        let max_slot_height = max_slot_height_mutex.lock().unwrap();
        if event_q_slot <= *max_slot_height {
            info!(
                "Skipping crank. Already cranked for slot. Event queue slot: {}, Max seen slot: {}",
                event_q_slot, max_slot_height
            );
            continue;
        }
        drop(max_slot_height);
        let event_q_data = event_q_value_and_context
            .value
            .ok_or(format_err!("Failed to retrieve account"))?
            .data;
        let req_q_data = client
            .get_account_with_commitment(&market_keys.req_q, CommitmentConfig::recent())?
            .value
            .ok_or(format_err!("Failed to retrieve account"))?
            .data;
        let inner: Cow<[u64]> = remove_dex_account_padding(&event_q_data)?;
        let (_header, seg0, seg1) = parse_event_queue(&inner)?;
        let req_inner: Cow<[u64]> = remove_dex_account_padding(&req_q_data)?;
        let (_req_header, req_seg0, req_seg1) = parse_event_queue(&req_inner)?;
        let event_q_len = seg0.len() + seg1.len();
        let req_q_len = req_seg0.len() + req_seg1.len();
        info!(
            "Size of request queue is {}, market {}, coin {}, pc {}",
            req_q_len, market, coin_wallet, pc_wallet
        );

        if event_q_len == 0 {
            continue;
        } else if std::time::Duration::from_secs(max_wait_for_events_delay)
            .gt(&last_cranked_at.elapsed())
            && (event_q_len as u64) < max_q_length
        {
            info!(
                "Skipping crank. Last cranked {} seconds ago and queue only has {} events. \
                Event queue slot: {}",
                last_cranked_at.elapsed().as_secs(),
                event_q_len,
                event_q_slot
            );
            continue;
        } else {
            info!(
                "Total event queue length: {}, market {}, coin {}, pc {}",
                event_q_len, market, coin_wallet, pc_wallet
            );
            let accounts = seg0.iter().chain(seg1.iter()).map(|event| event.owner);
            let mut used_accounts = BTreeSet::new();
            for account in accounts {
                used_accounts.insert(account);
                if used_accounts.len() >= num_accounts {
                    break;
                }
            }
            let orders_accounts: Vec<_> = used_accounts.into_iter().collect();
            info!(
                "Number of unique order accounts: {}, market {}, coin {}, pc {}",
                orders_accounts.len(),
                market,
                coin_wallet,
                pc_wallet
            );
            info!(
                "First 5 accounts: {:?}",
                orders_accounts
                    .iter()
                    .take(5)
                    .map(hash_accounts)
                    .collect::<Vec::<_>>()
            );

            let mut account_metas = Vec::with_capacity(orders_accounts.len() + 4);
            for pubkey_words in orders_accounts {
                let pubkey = Pubkey::new(transmute_to_bytes(&pubkey_words));
                account_metas.push(AccountMeta::new(pubkey, false));
            }
            for pubkey in [
                &market_keys.market,
                &market_keys.event_q,
                coin_wallet,
                pc_wallet,
            ]
            .iter()
            {
                account_metas.push(AccountMeta::new(**pubkey, false));
            }
            debug_println!("Number of workers: {}", num_workers);
            let end_time = std::time::Instant::now();
            info!(
                "Fetching {} events from the queue took {}",
                event_q_len,
                end_time.duration_since(start_time).as_millis()
            );
            for thread_num in 0..min(num_workers, 2 * event_q_len / events_per_worker + 1) {
                let payer = read_keypair_file(&payer_path)?;
                let program_id = program_id.clone();
                let client = opts.client();
                let account_metas = account_metas.clone();
                let event_q = *market_keys.event_q;
                let max_slot_height_mutex_clone = Arc::clone(&max_slot_height_mutex);
                pool.execute(move || {
                    consume_events_wrapper(
                        &client,
                        &program_id,
                        &payer,
                        account_metas,
                        thread_num,
                        events_per_worker,
                        event_q,
                        max_slot_height_mutex_clone,
                        event_q_slot,
                    )
                });
            }
            pool.join();
            last_cranked_at = std::time::Instant::now();
            info!(
                "Total loop time took {}",
                last_cranked_at.duration_since(loop_start).as_millis()
            );
        }
    }
}

fn consume_events_wrapper(
    client: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    account_metas: Vec<AccountMeta>,
    thread_num: usize,
    to_consume: usize,
    event_q: Pubkey,
    max_slot_height_mutex: Arc<Mutex<u64>>,
    slot: u64,
) {
    let start = std::time::Instant::now();
    let result = consume_events_once(
        &client,
        program_id,
        &payer,
        account_metas,
        to_consume,
        thread_num,
        event_q,
    );
    match result {
        Ok(signature) => {
            info!(
                "[thread {}] Successfully consumed events after {:?}: {}.",
                thread_num,
                start.elapsed(),
                signature
            );
            let mut max_slot_height = max_slot_height_mutex.lock().unwrap();
            *max_slot_height = max(slot, *max_slot_height);
        }
        Err(err) => {
            error!("[thread {}] Received error: {:?}", thread_num, err);
        }
    };
}

fn consume_events_once(
    client: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    account_metas: Vec<AccountMeta>,
    to_consume: usize,
    _thread_number: usize,
    _event_q: Pubkey,
) -> Result<Signature> {
    let _start = std::time::Instant::now();
    let instruction_data: Vec<u8> = MarketInstruction::ConsumeEvents(to_consume as u16).pack();
    let instruction = Instruction {
        program_id: *program_id,
        accounts: account_metas,
        data: instruction_data,
    };
    let random_instruction = solana_sdk::system_instruction::transfer(
        &payer.pubkey(),
        &payer.pubkey(),
        rand::random::<u64>() % 10000 + 1,
    );
    let (recent_hash, _fee_calc) = client.get_recent_blockhash()?;
    let txn = Transaction::new_signed_with_payer(
        &[instruction, random_instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_hash,
    );

    info!("Consuming events ...");
    let signature = client.send_transaction_with_config(
        &txn,
        RpcSendTransactionConfig {
            skip_preflight: true,
            ..RpcSendTransactionConfig::default()
        },
    )?;
    Ok(signature)
}

#[cfg(target_endian = "little")]
fn consume_events(
    client: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    state: &MarketPubkeys,
    coin_wallet: &Pubkey,
    pc_wallet: &Pubkey,
) -> Result<()> {
    let instruction = {
        let i = consume_events_instruction(client, program_id, state, coin_wallet, pc_wallet)?;
        match i {
            None => return Ok(()),
            Some(i) => i,
        }
    };
    let (recent_hash, _fee_calc) = client.get_recent_blockhash()?;
    info!("Consuming events ...");
    let txn = Transaction::new_signed_with_payer(
        std::slice::from_ref(&instruction),
        Some(&payer.pubkey()),
        &[payer],
        recent_hash,
    );
    info!("Consuming events ...");
    send_txn(client, &txn, false)?;
    Ok(())
}

pub fn consume_events_instruction(
    client: &RpcClient,
    program_id: &Pubkey,
    state: &MarketPubkeys,
    coin_wallet: &Pubkey,
    pc_wallet: &Pubkey,
) -> Result<Option<Instruction>> {
    let event_q_data = client.get_account_data(&state.event_q)?;
    let inner: Cow<[u64]> = remove_dex_account_padding(&event_q_data)?;
    let (_header, seg0, seg1) = parse_event_queue(&inner)?;

    if seg0.len() + seg1.len() == 0 {
        info!("Total event queue length: 0, returning early");
        return Ok(None);
    } else {
        info!("Total event queue length: {}", seg0.len() + seg1.len());
    }
    let accounts = seg0.iter().chain(seg1.iter()).map(|event| event.owner);
    let mut orders_accounts: Vec<_> = accounts.collect();
    orders_accounts.sort_unstable();
    orders_accounts.dedup();
    // todo: Shuffle the accounts before truncating, to avoid favoring low sort order accounts
    orders_accounts.truncate(32);
    info!("Number of unique order accounts: {}", orders_accounts.len());

    let mut account_metas = Vec::with_capacity(orders_accounts.len() + 4);
    for pubkey_words in orders_accounts {
        let pubkey = Pubkey::new(transmute_to_bytes(&pubkey_words));
        account_metas.push(AccountMeta::new(pubkey, false));
    }
    for pubkey in [&state.market, &state.event_q, coin_wallet, pc_wallet].iter() {
        account_metas.push(AccountMeta::new(**pubkey, false));
    }

    let instruction_data: Vec<u8> =
        MarketInstruction::ConsumeEvents(account_metas.len() as u16).pack();

    let instruction = Instruction {
        program_id: *program_id,
        accounts: account_metas,
        data: instruction_data,
    };

    Ok(Some(instruction))
}

pub fn cancel_order_by_client_order_id(
    client: &RpcClient,
    program_id: &Pubkey,
    owner: &Keypair,
    state: &MarketPubkeys,
    orders: &Pubkey,
    client_order_id: u64,
) -> Result<()> {
    let ixs = &[cancel_order_by_client_order_id_ix(
        program_id,
        &state.market,
        &state.bids,
        &state.asks,
        orders,
        &owner.pubkey(),
        &state.event_q,
        client_order_id,
    )?];
    let (recent_hash, _fee_calc) = client.get_recent_blockhash()?;
    let txn = Transaction::new_signed_with_payer(ixs, Some(&owner.pubkey()), &[owner], recent_hash);

    debug_println!("Canceling order by client order id instruction ...");
    let result = simulate_transaction(client, &txn, true, CommitmentConfig::confirmed())?;
    if let Some(e) = result.value.err {
        debug_println!("{:#?}", result.value.logs);
        return Err(format_err!("simulate_transaction error: {:?}", e));
    }

    send_txn(client, &txn, false)?;
    Ok(())
}

pub fn close_open_orders(
    client: &RpcClient,
    program_id: &Pubkey,
    owner: &Keypair,
    state: &MarketPubkeys,
    orders: &Pubkey,
) -> Result<()> {
    debug_println!("Closing open orders...");
    let ixs = &[close_open_orders_ix(
        program_id,
        orders,
        &owner.pubkey(),
        &owner.pubkey(),
        &state.market,
    )?];
    let (recent_hash, _fee_calc) = client.get_recent_blockhash()?;
    let txn = Transaction::new_signed_with_payer(ixs, Some(&owner.pubkey()), &[owner], recent_hash);

    debug_println!("Simulating close open orders instruction ...");
    let result = simulate_transaction(client, &txn, true, CommitmentConfig::confirmed())?;
    if let Some(e) = result.value.err {
        debug_println!("{:#?}", result.value.logs);
        return Err(format_err!("simulate_transaction error: {:?}", e));
    }

    send_txn(client, &txn, false)?;
    Ok(())
}

pub fn init_open_orders(
    client: &RpcClient,
    program_id: &Pubkey,
    owner: &Keypair,
    state: &MarketPubkeys,
    orders: &mut Option<Pubkey>,
) -> Result<()> {
    let mut instructions = Vec::new();
    let orders_keypair;
    let mut signers = Vec::new();
    let orders_pubkey = match *orders {
        Some(pk) => pk,
        None => {
            let (orders_key, instruction) = create_dex_account(
                client,
                program_id,
                &owner.pubkey(),
                size_of::<serum_dex::state::OpenOrders>(),
            )?;
            orders_keypair = orders_key;
            signers.push(&orders_keypair);
            instructions.push(instruction);
            orders_keypair.pubkey()
        }
    };
    *orders = Some(orders_pubkey);
    instructions.push(init_open_orders_ix(
        program_id,
        &orders_pubkey,
        &owner.pubkey(),
        &state.market,
        None,
    )?);
    signers.push(owner);

    let (recent_hash, _fee_calc) = client.get_recent_blockhash()?;
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&owner.pubkey()),
        &signers,
        recent_hash,
    );
    send_txn(client, &txn, false)?;
    Ok(())
}

pub fn place_order(
    client: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    wallet: &Pubkey,
    state: &MarketPubkeys,
    orders: &mut Option<Pubkey>,

    new_order: NewOrderInstructionV3,
) -> Result<()> {
    let mut instructions = Vec::new();
    let orders_keypair;
    let mut signers = Vec::new();
    let orders_pubkey = match *orders {
        Some(pk) => pk,
        None => {
            let (orders_key, instruction) = create_dex_account(
                client,
                program_id,
                &payer.pubkey(),
                size_of::<serum_dex::state::OpenOrders>(),
            )?;
            orders_keypair = orders_key;
            signers.push(&orders_keypair);
            instructions.push(instruction);
            orders_keypair.pubkey()
        }
    };
    *orders = Some(orders_pubkey);
    let _side = new_order.side;
    let data = MarketInstruction::NewOrderV3(new_order).pack();
    let instruction = Instruction {
        program_id: *program_id,
        data,
        accounts: vec![
            AccountMeta::new(*state.market, false),
            AccountMeta::new(orders_pubkey, false),
            AccountMeta::new(*state.req_q, false),
            AccountMeta::new(*state.event_q, false),
            AccountMeta::new(*state.bids, false),
            AccountMeta::new(*state.asks, false),
            AccountMeta::new(*wallet, false),
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new(*state.coin_vault, false),
            AccountMeta::new(*state.pc_vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::ID, false),
        ],
    };
    instructions.push(instruction);
    signers.push(payer);

    let (recent_hash, _fee_calc) = client.get_recent_blockhash()?;
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&payer.pubkey()),
        &signers,
        recent_hash,
    );
    send_txn(client, &txn, false)?;
    Ok(())
}

fn create_dex_account(
    client: &RpcClient,
    program_id: &Pubkey,
    payer: &Pubkey,
    unpadded_len: usize,
) -> Result<(Keypair, Instruction)> {
    let len = unpadded_len + 12;
    let key = Keypair::generate(&mut OsRng);
    let create_account_instr = solana_sdk::system_instruction::create_account(
        payer,
        &key.pubkey(),
        client.get_minimum_balance_for_rent_exemption(len)?,
        len as u64,
        program_id,
    );
    Ok((key, create_account_instr))
}