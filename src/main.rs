use bincode::Options;
use jito_json_rpc_client::jsonrpc_client::rpc_client::RpcClient as JitoRpcClient;
use temp::common::utils::{
    create_arc_rpc_client, create_nonblocking_rpc_client, import_arc_wallet, import_env_var,
    import_wallet, log_message, AppState,
};
use temp::core::token::get_account_info;
use temp::core::tx::jito_confirm;
use temp::engine::swap::{pump_swap, raydium_swap};
// use copy_trading_bot::dex::pump::pump_sdk_swap;
use dotenv::dotenv;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use spl_associated_token_account::get_associated_token_address;
use std::env;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use tokio::time::Instant;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

#[derive(Serialize)]
struct SwapRequest {
    quoteResponse: serde_json::Value, // You may deserialize it into a specific struct if known
    userPublicKey: String,
    wrapAndUnwrapSol: bool,
    dynamicComputeUnitLimit: bool,
    prioritizationFeeLamports: u64,
}
#[tokio::main]

async fn main() {
    dotenv().ok();
    let target = env::var("TARGET_PUBKEY").expect("TARGET not set");

    let rpc_client = create_arc_rpc_client().unwrap();
    let rpc_nonblocking_client = create_nonblocking_rpc_client().await.unwrap();
    let wallet = import_arc_wallet().unwrap();

    let state = AppState {
        rpc_client,
        rpc_nonblocking_client,
        wallet,
    };
    pub static BLOCK_ENGINE_URL: LazyLock<String> =
        LazyLock::new(|| import_env_var("JITO_BLOCK_ENGINE_URL"));
    let jito_client = Arc::new(JitoRpcClient::new(format!(
        "{}/api/v1/bundles",
        *BLOCK_ENGINE_URL
    )));
    let unwanted_key = env::var("JUP_PUBKEY").expect("JUP_PUBKEY not set");
    let ws_url = env::var("RPC_WEBSOCKET_ENDPOINT").expect("RPC_WEBSOCKET_ENDPOINT not set");

    let (ws_stream, _) = connect_async(ws_url)
        .await
        .expect("Failed to connect to WebSocket server");
    let (mut write, mut read) = ws_stream.split();
    // Subscribe to logs
    let subscription_message = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "transactionSubscribe",
        "params": [

            {
                "failed": false,
                "accountInclude": ["675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"],
                "accountExclude": [unwanted_key],
                // Optionally specify accounts of interest
            },
            {
                "commitment": "processed",
                "encoding": "jsonParsed",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0
            }
        ]
    });

    write
        .send(subscription_message.to_string().into())
        .await
        .expect("Failed to send subscription message");

    let _ = log_message("---------------------   Copy-trading-bot start!!!  ------------------\n")
        .await;

    // Listen for messages
    while let Some(Ok(msg)) = read.next().await {
        if let WsMessage::Text(text) = msg {
            let json: Value = serde_json::from_str(&text).unwrap();

            let sig = json["params"]["result"]["signature"]
                .as_str()
                .unwrap_or_default();
            let timestamp = Instant::now();
            if let Some(account_keys) = json["params"]["result"]["transaction"]["transaction"]
                ["message"]["accountKeys"]
                .as_array()
            {
                let mut flag = false;
                for account_key in account_keys.iter() {
                    if account_key["signer"] == true
                        && target == account_key["pubkey"].as_str().unwrap()
                    {
                        flag = true;
                        break;
                    } else {
                        flag = false;
                        break;
                    }
                }
                if flag == true {
                    let msg = "\nTarget Wallet: ".to_string()
                        + &target
                        + "  https://solscan.io/tx/"
                        + &sig;
                    let _ = log_message(&msg).await;

                    for key in account_keys.iter() {
                        if key["pubkey"] == "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8" {
                            tx_ray(
                                json.clone(),
                                target.clone(),
                                timestamp.clone(),
                                state.clone(),
                                jito_client.clone(),
                            )
                            .await;
                        }
                        if key["pubkey"] == "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P" {
                            tx_pump(
                                json.clone(),
                                target.clone(),
                                timestamp.clone(),
                                state.clone(),
                                jito_client.clone(),
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }
}

pub async fn tx_ray(
    json: Value,
    target: String,
    timestamp: Instant,
    state: AppState,
    jito_client: Arc<JitoRpcClient>,
) {
    let mut amount_in = 0_u64;
    let mut mint = "".to_string();
    let mut mint_post_amount = 0_u64;
    let mut mint_pre_amount = 0_u64;
    let mut sol_post_amount = 0_u64;
    let mut sol_pre_amount = 0_u64;
    let mut dirs = "".to_string();
    let percent = env::var("PERCENT")
        .expect("PERCENT not set")
        .parse::<u64>()
        .unwrap();
    let mut pool_id = "".to_string();

    if let Some(post_token_balances) =
        json["params"]["result"]["transaction"]["meta"]["postTokenBalances"].as_array()
    {
        for post_token_balance in post_token_balances.iter() {
            if post_token_balance["owner"] == target.to_string()
                && post_token_balance["mint"]
                    != "So11111111111111111111111111111111111111112".to_string()
            {
                mint = post_token_balance["mint"].as_str().unwrap().to_string();
                let mint_post_amount_str = post_token_balance["uiTokenAmount"]["amount"]
                    .as_str()
                    .unwrap()
                    .to_string();
                mint_post_amount = mint_post_amount_str.parse::<u64>().unwrap();
            }
            if post_token_balance["owner"]
                == "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1".to_string()
                && post_token_balance["mint"]
                    == "So11111111111111111111111111111111111111112".to_string()
            {
                let sol_post_amount_str = post_token_balance["uiTokenAmount"]["amount"]
                    .as_str()
                    .unwrap()
                    .to_string();
                sol_post_amount = sol_post_amount_str.parse::<u64>().unwrap();
            }
        }
    }
    if let Some(pre_token_balances) =
        json["params"]["result"]["transaction"]["meta"]["preTokenBalances"].as_array()
    {
        for pre_token_balance in pre_token_balances.iter() {
            if pre_token_balance["owner"] == target.to_string()
                && pre_token_balance["mint"]
                    != "So11111111111111111111111111111111111111112".to_string()
            {
                mint = pre_token_balance["mint"].as_str().unwrap().to_string();
                let mint_pre_amount_str = pre_token_balance["uiTokenAmount"]["amount"]
                    .as_str()
                    .unwrap()
                    .to_string();
                mint_pre_amount = mint_pre_amount_str.parse::<u64>().unwrap();
            }
            if pre_token_balance["owner"]
                == "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1".to_string()
                && pre_token_balance["mint"]
                    == "So11111111111111111111111111111111111111112".to_string()
            {
                let sol_pre_amount_str = pre_token_balance["uiTokenAmount"]["amount"]
                    .as_str()
                    .unwrap()
                    .to_string();
                sol_pre_amount = sol_pre_amount_str.parse::<u64>().unwrap();
            }
        }
    }
    if let Some(account_keys) =
        json["params"]["result"]["transaction"]["transaction"]["message"]["accountKeys"].as_array()
    {
        pool_id = account_keys[5]["pubkey"].as_str().unwrap().to_string();
    }

    println!(
        "pool_id: {:#?}, post: {:#?}, pre: {:#?}",
        pool_id.clone(),
        mint_post_amount.clone(),
        mint_pre_amount.clone()
    );
    if mint_pre_amount < mint_post_amount {
        dirs = "buy".to_string();
        amount_in = sol_post_amount - sol_pre_amount;
        swap_to_events_on_raydium(
            mint,
            amount_in * percent / 100,
            dirs,
            pool_id,
            timestamp.clone(),
            jito_client.clone(),
            state.clone(),
        )
        .await;
    } else {
        dirs = "sell".to_string();
        amount_in = mint_pre_amount - mint_post_amount;
        swap_to_events_on_raydium(
            mint,
            amount_in * percent / 100,
            dirs,
            pool_id,
            timestamp.clone(),
            jito_client.clone(),
            state.clone(),
        )
        .await;
    }
}

pub async fn tx_pump(
    json: Value,
    target: String,
    timestamp: Instant,
    state: AppState,
    jito_client: Arc<JitoRpcClient>,
) {
    // Iterate over logs and check for unwanted_key
    println!("1: {:#?}", timestamp.elapsed());
    let mut amount_in = 0_u64;
    let mut mint = "".to_string();
    let mut mint_post_amount = 0_f64;
    let mut mint_pre_amount = 0_f64;
    let mut dirs = "".to_string();
    let mut bonding_curve_account = "".to_string();
    let percent = env::var("PERCENT")
        .expect("PERCENT not set")
        .parse::<u64>()
        .unwrap();

    if let Some(post_token_balances) =
        json["params"]["result"]["transaction"]["meta"]["postTokenBalances"].as_array()
    {
        for post_token_balance in post_token_balances.iter() {
            if post_token_balance["owner"] == target.to_string() {
                mint = post_token_balance["mint"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                mint_post_amount = post_token_balance["uiTokenAmount"]["uiAmount"]
                    .as_f64()
                    .unwrap();
            } else {
                bonding_curve_account = post_token_balance["owner"].as_str().unwrap().to_string();
            }
        }
    }
    if let Some(pre_token_balances) =
        json["params"]["result"]["transaction"]["meta"]["preTokenBalances"].as_array()
    {
        for pre_token_balance in pre_token_balances.iter() {
            if pre_token_balance["owner"] == target.to_string() {
                mint_pre_amount = pre_token_balance["uiTokenAmount"]["uiAmount"]
                    .as_f64()
                    .unwrap();
            }
        }
    }

    if mint_pre_amount < mint_post_amount {
        if let Some(inner_instructions) =
            json["params"]["result"]["transaction"]["meta"]["innerInstructions"].as_array()
        {
            for inner_instruction in inner_instructions.iter() {
                // Try to extract the string representation of the log
                if let Some(instructions) = inner_instruction["instructions"].as_array() {
                    for instruction in instructions {
                        if instruction["parsed"]["type"] == "transfer".to_string()
                            && instruction["program"] == "system".to_string()
                            && instruction["parsed"]["info"]["destination"] == bonding_curve_account
                        {
                            amount_in = instruction["parsed"]["info"]["lamports"].as_u64().unwrap();
                        }
                    }
                }
            }
        }
        dirs = "buy".to_string();
        swap_to_events_on_pump(
            mint,
            amount_in * percent / 100,
            dirs,
            timestamp.clone(),
            jito_client.clone(),
            state.clone(),
        )
        .await;
    } else {
        dirs = "sell".to_string();
        amount_in = ((mint_pre_amount - mint_post_amount) * 1000000.0) as u64;

        swap_to_events_on_pump(
            mint,
            amount_in * percent / 100,
            dirs,
            timestamp.clone(),
            jito_client.clone(),
            state.clone(),
        )
        .await;
    }
}

pub async fn swap_on_jup(mint: String, dir: String, amount: u64) {
    let mut url = "".to_string();
    let input_mint = "So11111111111111111111111111111111111111112";
    let base_mint = &mint; // Replace with your actual base mint
    let wallet = import_wallet().unwrap();
    let percent = env::var("PERCENT")
        .expect("PERCENT not set")
        .parse::<u64>()
        .unwrap();
    // println!("dir: {:#?}, mint: {:#?}, amount: {:#?}", dir, mint, amount);
    // Construct the request URL
    if dir == "buy" {
        url = format!(
            "https://ultra-api.jup.ag/order?inputMint={}&outputMint={}&amount={}&slippageBps=10000",
            input_mint,
            base_mint, // You might need to convert this to base58 representation if it's not already
            amount * percent / 100
        );
    } else {
        let mint_pubkey = Pubkey::from_str(&base_mint).unwrap();
        let client = create_nonblocking_rpc_client().await.unwrap();
        let token_account = get_associated_token_address(&wallet.pubkey(), &mint_pubkey);
        if let Ok(token_account_info) = get_account_info(client, &mint_pubkey, &token_account).await
        {
            if token_account_info.base.amount <= amount * percent / 100 {
                url = format!(
                    "https://ultra-api.jup.ag/order?inputMint={}&outputMint={}&amount={}&slippageBps=10000",
                    base_mint, // You might need to convert this to base58 representation if it's not already
                    input_mint,
                    token_account_info.base.amount
                );
            } else {
                url = format!(
                    "https://ultra-api.jup.ag/order?inputMint={}&outputMint={}&amount={}&slippageBps=10000",
                    base_mint, // You might need to convert this to base58 representation if it's not already
                    input_mint,
                    amount * percent / 100
                );
            }
        };
    }
    let client = reqwest::Client::new();

    // Send the GET request

    if let Ok(response) = reqwest::get(&url).await {
        if response.status().is_success() {
            // Parse the response JSON if needed
            let json: serde_json::Value = response.json().await.unwrap_or_default();
            let swap_request = SwapRequest {
                quoteResponse: json,
                userPublicKey: wallet.pubkey().to_string(),
                wrapAndUnwrapSol: true,
                dynamicComputeUnitLimit: true,
                prioritizationFeeLamports: 52000,
            };

            if let Ok(response) = client
                .post("https://quote-api.jup.ag/v6/swap")
                .header("Content-Type", "application/json")
                .json(&swap_request)
                .send()
                .await
            {
                let res: serde_json::Value = response.json().await.unwrap();

                let tx = base64::decode(
                    res["swapTransaction"]
                        .as_str()
                        .unwrap_or("default")
                        .to_string(),
                )
                .unwrap_or_default();
                if let Ok(transaction) = bincode::options()
                    .with_fixint_encoding()
                    .reject_trailing_bytes()
                    .deserialize::<VersionedTransaction>(&tx)
                {
                    let signed_tx =
                        VersionedTransaction::try_new(transaction.message.clone(), &[&wallet])
                            .unwrap_or_default();

                    let recent_blockhash = VersionedMessage::recent_blockhash(&transaction.message);
                    // println!("!!!!!!!!!!!!: {:#?}", recent_blockhash);
                    jito_confirm(&wallet, signed_tx, recent_blockhash).await;
                };
            };
        } else {
            println!("Failed to fetch: {}", response.status());
            let _ = log_message("Failed: Not tradable token)\n").await;
        }
    }
}
pub async fn swap_to_events_on_pump(
    mint: String,
    amount_in: u64,
    dirs: String,
    timestamp: Instant,
    jito_client: Arc<JitoRpcClient>,
    state: AppState,
) {
    println!("2: {:#?}", timestamp.elapsed().clone());

    let slippage = 10000;
    println!("2.1: {:#?}", timestamp.elapsed());
    let res = pump_swap(
        state,
        amount_in,
        &dirs,
        slippage,
        &mint,
        jito_client,
        timestamp.clone(),
    )
    .await;
}

pub async fn swap_to_events_on_raydium(
    mint: String,
    amount_in: u64,
    dirs: String,
    pool_id: String,
    timestamp: Instant,
    jito_client: Arc<JitoRpcClient>,
    state: AppState,
) {
    println!("2: {:#?}", timestamp.elapsed().clone());

    let slippage = 10000;
    println!("2.1: {:#?}", timestamp.elapsed());
    let res = raydium_swap(
        state,
        amount_in,
        &dirs,
        pool_id,
        slippage,
        &mint,
        jito_client,
        timestamp.clone(),
    )
    .await;
}
