use crate::{
    core::{
        token::{get_account_info, get_mint_info},
        tx,
    },
    engine::swap::{SwapDirection, SwapInType},
};
use amm_cli::AmmSwapInfoResult;
use anyhow::{anyhow, Context, Result};
use bytemuck;
use jito_json_rpc_client::jsonrpc_client::rpc_client::RpcClient as JitoRpcClient;
use raydium_amm::state::{AmmInfo, Loadable};
use serde::Deserialize;
use serde::Serialize;
use solana_client::rpc_filter::{Memcmp, RpcFilterType};
use solana_sdk::{
    instruction::Instruction, program_pack::Pack, pubkey::Pubkey, signature::Keypair,
    signer::Signer, system_instruction,
};
use spl_associated_token_account::{
    get_associated_token_address, get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use spl_token::{amount_to_ui_amount, state::Account, ui_amount_to_amount};
use spl_token_client::token::TokenError;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::time::Instant;

pub const AMM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const RAYDIUM_AUTHORITY_V4: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";

#[derive(Serialize)]
struct SwapRequest {
    quoteResponse: serde_json::Value, // You may deserialize it into a specific struct if known
    userPublicKey: String,
    wrapAndUnwrapSol: bool,
    dynamicComputeUnitLimit: bool,
    prioritizationFeeLamports: u64,
}

#[derive(Debug, Deserialize)]
pub struct PoolInfo {
    pub success: bool,
    pub data: PoolData,
}

#[derive(Debug, Deserialize)]
pub struct PoolData {
    // pub count: u32,
    pub data: Vec<Pool>,
}

impl PoolData {
    pub fn get_pool(&self) -> Option<Pool> {
        self.data.first().cloned()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Pool {
    pub id: String,
    #[serde(rename = "programId")]
    pub program_id: String,
    #[serde(rename = "mintA")]
    pub mint_a: Mint,
    #[serde(rename = "mintB")]
    pub mint_b: Mint,
    #[serde(rename = "marketId")]
    pub market_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Mint {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
}

pub struct Raydium {
    pub rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pub rpc_client: Option<Arc<solana_client::rpc_client::RpcClient>>,
    pub keypair: Arc<Keypair>,
    pub pool_id: Option<String>,
}

impl Raydium {
    pub fn new(
        rpc_nonblocking_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
        rpc_client: Arc<solana_client::rpc_client::RpcClient>,
        keypair: Arc<Keypair>,
    ) -> Self {
        Self {
            rpc_nonblocking_client,
            keypair,
            rpc_client: Some(rpc_client),
            pool_id: None,
        }
    }

    pub async fn swap_by_mint(
        &self,
        mint_str: &str,
        swap_direction: SwapDirection,
        amount_in: u64,
        pool_id: String,
        slippage: u64,
        start_time: Instant,
        jito_client: Arc<JitoRpcClient>,
    ) -> Result<Vec<String>> {
        let slippage_bps = slippage;
        let owner = self.keypair.pubkey();
        let program_id = spl_token::ID;
        let native_mint = spl_token::native_mint::ID;
        let (amm_pool_id, pool_state) = get_pool_state(
            self.rpc_client.clone().unwrap(),
            Some(&pool_id),
            Some(mint_str),
        )
        .await?;
        println!("pool_id: {:#?}", amm_pool_id.clone());
        println!("get pool info: {:#?}", start_time.elapsed());
        let mint = Pubkey::from_str(mint_str).unwrap();

        let (token_in, token_out, user_input_token, swap_base_in) = match (
            swap_direction.clone(),
            pool_state.coin_vault_mint == native_mint,
        ) {
            (SwapDirection::Buy, true) => (native_mint, mint, pool_state.coin_vault, true),
            (SwapDirection::Buy, false) => (native_mint, mint, pool_state.pc_vault, true),
            (SwapDirection::Sell, true) => (mint, native_mint, pool_state.pc_vault, true),
            (SwapDirection::Sell, false) => (mint, native_mint, pool_state.coin_vault, true),
        };

        let in_ata = get_associated_token_address(&owner, &token_in);
        let out_ata = get_associated_token_address(&owner, &token_out);
        println!("get ata: {:#?}", start_time.elapsed());
        let mut create_instruction = None;
        let mut close_instruction = None;

        let (amount_specified) = match swap_direction.clone() {
            SwapDirection::Buy => {
                create_instruction = Some(create_associated_token_account_idempotent(
                    &owner,
                    &owner,
                    &token_out,
                    &program_id,
                ));
                amount_in
            }
            SwapDirection::Sell => {
                let in_account =
                    get_account_info(self.rpc_nonblocking_client.clone(), &token_in, &in_ata)
                        .await?;
                if in_account.base.amount > amount_in {
                    amount_in
                } else {
                    in_account.base.amount
                }
            }
        };

        let amm_program = Pubkey::from_str(AMM_PROGRAM)?;
        println!("get ix info: {:#?}", start_time.elapsed());
        let swap_info_result = amm_cli::calculate_swap_info(
            &self.rpc_client.clone().unwrap(),
            amm_program,
            amm_pool_id,
            user_input_token,
            amount_specified,
            slippage_bps,
            swap_base_in,
        )?;
        let other_amount_threshold = swap_info_result.other_amount_threshold;

        // build instructions
        let mut instructions = vec![];
        // sol <-> wsol support
        let mut wsol_account = None;
        if token_in == native_mint || token_out == native_mint {
            // create wsol account
            let seed = &format!("{}", Keypair::new().pubkey())[..32];
            let wsol_pubkey = Pubkey::create_with_seed(&owner, seed, &spl_token::id())?;
            wsol_account = Some(wsol_pubkey);

            // LAMPORTS_PER_SOL / 100 // 0.01 SOL as rent
            // get rent
            let rent = 2039280;
            println!("specified: {:#?}", amount_specified.clone());
            // if buy add amount_specified
            let total_amount = if token_in == native_mint {
                rent + amount_specified
            } else {
                rent
            };
            println!("total amount: {:#?}", total_amount);
            // create tmp wsol account
            instructions.push(system_instruction::create_account_with_seed(
                &owner,
                &wsol_pubkey,
                &owner,
                seed,
                total_amount,
                Account::LEN as u64, // 165, // Token account size
                &spl_token::id(),
            ));

            // initialize account
            instructions.push(spl_token::instruction::initialize_account(
                &spl_token::id(),
                &wsol_pubkey,
                &native_mint,
                &owner,
            )?);
        }

        if let Some(create_instruction) = create_instruction {
            instructions.push(create_instruction);
        }
        if amount_specified > 0 {
            let mut close_wsol_account_instruction = None;
            // replace native mint with tmp wsol account
            let mut final_in_ata = in_ata;
            let mut final_out_ata = out_ata;

            if let Some(wsol_account) = wsol_account {
                match swap_direction.clone() {
                    SwapDirection::Buy => {
                        final_in_ata = wsol_account;
                    }
                    SwapDirection::Sell => {
                        final_out_ata = wsol_account;
                    }
                }
                close_wsol_account_instruction = Some(spl_token::instruction::close_account(
                    &program_id,
                    &wsol_account,
                    &owner,
                    &owner,
                    &[&owner],
                )?);
            }

            // build swap instruction
            println!("get swap ix info: {:#?}", start_time.elapsed());
            let build_swap_instruction = amm_swap(
                &amm_program,
                swap_info_result,
                &owner,
                &final_in_ata,
                &final_out_ata,
                amount_specified,
                other_amount_threshold,
                swap_base_in,
            )?;

            instructions.push(build_swap_instruction);
            // close wsol account
            if let Some(close_wsol_account_instruction) = close_wsol_account_instruction {
                instructions.push(close_wsol_account_instruction);
            }
        }
        if let Some(close_instruction) = close_instruction {
            instructions.push(close_instruction);
        }
        if instructions.is_empty() {
            return Err(anyhow!("instructions is empty, no tx required"));
        }
        println!("get info: {:#?}", start_time.elapsed());
        tx::new_signed_and_send(
            &self.rpc_client.clone().unwrap(),
            &self.keypair,
            instructions,
            jito_client.clone(),
            start_time.clone(),
        )
        .await
    }
}
pub fn amm_swap(
    amm_program: &Pubkey,
    result: AmmSwapInfoResult,
    user_owner: &Pubkey,
    user_source: &Pubkey,
    user_destination: &Pubkey,
    amount_specified: u64,
    other_amount_threshold: u64,
    swap_base_in: bool,
) -> Result<Instruction> {
    let swap_instruction = if swap_base_in {
        raydium_amm::instruction::swap_base_in(
            amm_program,
            &result.pool_id,
            &result.amm_authority,
            &result.amm_open_orders,
            &result.amm_coin_vault,
            &result.amm_pc_vault,
            &result.market_program,
            &result.market,
            &result.market_bids,
            &result.market_asks,
            &result.market_event_queue,
            &result.market_coin_vault,
            &result.market_pc_vault,
            &result.market_vault_signer,
            user_source,
            user_destination,
            user_owner,
            amount_specified,
            other_amount_threshold,
        )?
    } else {
        raydium_amm::instruction::swap_base_out(
            amm_program,
            &result.pool_id,
            &result.amm_authority,
            &result.amm_open_orders,
            &result.amm_coin_vault,
            &result.amm_pc_vault,
            &result.market_program,
            &result.market,
            &result.market_bids,
            &result.market_asks,
            &result.market_event_queue,
            &result.market_coin_vault,
            &result.market_pc_vault,
            &result.market_vault_signer,
            user_source,
            user_destination,
            user_owner,
            other_amount_threshold,
            amount_specified,
        )?
    };

    Ok(swap_instruction)
}

pub async fn get_pool_state(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    pool_id: Option<&str>,
    mint: Option<&str>,
) -> Result<(Pubkey, AmmInfo)> {
    if let Some(pool_id) = pool_id {
        // logger.log(format!("[FIND POOL STATE BY pool_id]: {}", pool_id));
        let amm_pool_id = Pubkey::from_str(pool_id)?;
        let pool_data = common::rpc::get_account(&rpc_client, &amm_pool_id)?
            .ok_or(anyhow!("NotFoundPool: pool state not found"))?;
        let pool_state: &AmmInfo =
            bytemuck::from_bytes(&pool_data[0..core::mem::size_of::<AmmInfo>()]);
        Ok((amm_pool_id, *pool_state))
    } else {
        Err(anyhow!("NotFoundPool: pool state not found"))
    }
}

pub async fn get_pool_state_by_mint(
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    mint: &str,
) -> Result<(Pubkey, AmmInfo)> {
    // logger.log(format!("[FIND POOL STATE BY mint]: {}", mint));
    let pairs = vec![
        // pump pool
        (
            Some(spl_token::native_mint::ID),
            Pubkey::from_str(mint).ok(),
        ),
        // general pool
        (
            Pubkey::from_str(mint).ok(),
            Some(spl_token::native_mint::ID),
        ),
    ];

    let pool_len = core::mem::size_of::<AmmInfo>() as u64;
    let amm_program = Pubkey::from_str(AMM_PROGRAM)?;
    // Find matching AMM pool from mint pairs by filter
    let mut found_pools = None;
    for (coin_mint, pc_mint) in pairs {
        // logger.log(format!(
        //     "get_pool_state_by_mint filter: coin_mint: {:?}, pc_mint: {:?}",
        //     coin_mint, pc_mint
        // ));
        let filters = match (coin_mint, pc_mint) {
            (None, None) => Some(vec![RpcFilterType::DataSize(pool_len)]),
            (Some(coin_mint), None) => Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(400, &coin_mint.to_bytes())),
                RpcFilterType::DataSize(pool_len),
            ]),
            (None, Some(pc_mint)) => Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(432, &pc_mint.to_bytes())),
                RpcFilterType::DataSize(pool_len),
            ]),
            (Some(coin_mint), Some(pc_mint)) => Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(400, &coin_mint.to_bytes())),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(432, &pc_mint.to_bytes())),
                RpcFilterType::DataSize(pool_len),
            ]),
        };
        let pools =
            common::rpc::get_program_accounts_with_filters(&rpc_client, amm_program, filters)
                .unwrap();
        if !pools.is_empty() {
            found_pools = Some(pools);
            break;
        }
    }

    match found_pools {
        Some(pools) => {
            let pool = &pools[0];
            let pool_state = AmmInfo::load_from_bytes(&pools[0].1.data)?;
            Ok((pool.0, *pool_state))
        }
        None => Err(anyhow!("NotFoundPool: pool state not found")),
    }
}

// get pool info
// https://api-v3.raydium.io/pools/info/mint?mint1=So11111111111111111111111111111111111111112&mint2=EzM2d8JVpzfhV7km3tUsR1U1S4xwkrPnWkM4QFeTpump&poolType=standard&poolSortField=default&sortType=desc&pageSize=10&page=1
pub async fn get_pool_info(mint1: &str, mint2: &str) -> Result<PoolData> {
    let client = reqwest::Client::new();

    let result = client
        .get("https://api-v3.raydium.io/pools/info/mint")
        .query(&[
            ("mint1", mint1),
            ("mint2", mint2),
            ("poolType", "standard"),
            ("poolSortField", "default"),
            ("sortType", "desc"),
            ("pageSize", "1"),
            ("page", "1"),
        ])
        .send()
        .await?
        .json::<PoolInfo>()
        .await
        .context("Failed to parse pool info JSON")?;
    Ok(result.data)
}
