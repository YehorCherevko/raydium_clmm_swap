use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use bincode;
use dotenv::dotenv;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::{
    env,
    fs::File,
    io::Read,
    path::PathBuf,
};

 
const PRIORITY_FEE_URL: &str = "https://api-v3.raydium.io/main/auto-fee";
const SWAP_BASE:        &str = "https://transaction-v1.raydium.io";

 
const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

const WRAP_SOL:   bool = true;
const UNWRAP_SOL: bool = false;

#[derive(Deserialize)]
struct PriorityFeeResponse {
    data: PriorityFeeDataWrapper,
}

#[derive(Deserialize)]
struct PriorityFeeDataWrapper {
    default: FeeTiers,
}

#[derive(Deserialize)]
struct FeeTiers {
    vh: u64,
    h:  u64,
    m:  u64,
}

#[derive(Deserialize)]
struct SwapTransactionResponse {
    data: Vec<SwapTxObject>,
}

#[derive(Deserialize)]
struct SwapTxObject {
    transaction: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    
    dotenv().ok();

    let keypair_path = env::var("KEYPAIR_PATH")
        .context("KEYPAIR_PATH must be set in .env")?;
    let input_mint = env::var("INPUT_MINT")
        .context("INPUT_MINT must be set in .env")?;
    let output_mint = env::var("OUTPUT_MINT")
        .context("OUTPUT_MINT must be set in .env")?;
    let amount: u64 = env::var("AMOUNT")
        .context("AMOUNT must be set in .env")?
        .parse()
        .context("AMOUNT must be a valid u64")?;
    let slippage_bps: u64 = env::var("SLIPPAGE_BPS")
        .context("SLIPPAGE_BPS must be set in .env")?
        .parse()
        .context("SLIPPAGE_BPS must be a valid u64")?;
    let tx_version = env::var("TX_VERSION")
        .context("TX_VERSION must be set in .env")?;
 
    let owner = read_keypair_from_file(&keypair_path)
        .with_context(|| format!("Failed to read keypair from {}", keypair_path))?;

    let rpc_client = RpcClient::new_with_commitment(
        RPC_URL.to_string(),
        CommitmentConfig::confirmed(),
    );

   
    let http_client = Client::new();

    println!("Calling priority-fee at: {}", PRIORITY_FEE_URL);
    let fee_resp = http_client
        .get(PRIORITY_FEE_URL)
        .send()
        .await
        .context("Failed to call priority-fee endpoint")?;
    if !fee_resp.status().is_success() {
        return Err(anyhow!("priority-fee endpoint returned HTTP {}", fee_resp.status()));
    }
    let fee_json: PriorityFeeResponse = fee_resp
        .json()
        .await
        .context("Failed to parse priority-fee JSON")?;
    let high_fee: u64 = fee_json.data.default.h;
    println!("Using 'high' fee tier = {} micro-lamports", high_fee);

    let quote_url = format!(
        "{}/compute/swap-base-in?inputMint={}&outputMint={}&amount={}&slippageBps={}&txVersion={}",
        SWAP_BASE, input_mint, output_mint, amount, slippage_bps, tx_version
    );
    println!("Fetching swap quote from: {}", quote_url);

    let quote_resp = http_client
        .get(&quote_url)
        .send()
        .await
        .context("Failed to call compute/swap-base-in")?;
    if !quote_resp.status().is_success() {
        return Err(anyhow!("compute/swap-base-in returned HTTP {}", quote_resp.status()));
    }

    let swap_response_json: serde_json::Value = quote_resp
        .json()
        .await
        .context("Failed to parse swap quote JSON")?;

   
    if let Some(route) = swap_response_json.get("route") {
        println!("–––––––––––––––––––––––––––––––");
        println!("Detailed `marketKeys` for each leg in `route`:");
        if let Some(array) = route.as_array() {
            for (i, step) in array.iter().enumerate() {
                if let Some(market_keys) = step.get("marketKeys") {
 
                    let pretty = serde_json::to_string_pretty(market_keys)
                        .unwrap_or_else(|_| "\"<invalid JSON>\"".to_string());
                    println!(" Leg {} marketKeys:\n{}\n", i + 1, pretty);
                }
            }
        }
        println!("–––––––––––––––––––––––––––––––");
    }
    // ──────────────────────────────────────────────────────────────────────────

 
    let tx_request_body = json!({
        "computeUnitPriceMicroLamports": high_fee.to_string(),
        "swapResponse": swap_response_json,
        "txVersion": tx_version,
        "wallet": owner.pubkey().to_string(),
        "wrapSol": WRAP_SOL,
        "unwrapSol": UNWRAP_SOL
    });
    let tx_url = format!("{}/transaction/swap-base-in", SWAP_BASE);
    println!("Building swap transaction via: {}", tx_url);
    let resp = http_client
        .post(&tx_url)
        .json(&tx_request_body)
        .send()
        .await
        .context("Failed to call transaction/swap-base-in")?;
    if !resp.status().is_success() {
        return Err(anyhow!("transaction/swap-base-in returned HTTP {}", resp.status()));
    }

 
    let raw_json = resp
        .text()
        .await
        .context("Failed to read response text from transaction/swap-base-in")?;

 
    println!("Raw /transaction/swap-base-in response JSON:\n{}", raw_json);

   
    let swap_tx_json: SwapTransactionResponse = serde_json::from_str(&raw_json)
        .context("Failed to deserialize SwapTransactionResponse from raw JSON")?;

  
    let mut versioned_transactions = Vec::new();
    for (i, obj) in swap_tx_json.data.iter().enumerate() {
        let raw_bytes = base64::engine::general_purpose::STANDARD
            .decode(&obj.transaction)
            .with_context(|| format!("Leg {}: failed to Base64-decode transaction", i + 1))?;

        let vtx: VersionedTransaction = bincode::deserialize(&raw_bytes)
            .with_context(|| format!("Leg {}: failed to bincode-deserialize VersionedTransaction", i + 1))?;
        versioned_transactions.push(vtx);
    }
    println!("total {} transactions", versioned_transactions.len());

 
    for (i, vtx) in versioned_transactions.into_iter().enumerate() {
 
        let signed_vtx = VersionedTransaction::try_new(vtx.message.clone(), &[&owner])
            .context("Failed to rebuild VersionedTransaction with signature")?;

        println!("{} transaction sending...", i + 1);
        let signature = rpc_client
            .send_transaction_with_config(
                &signed_vtx,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..RpcSendTransactionConfig::default()
                },
            )
            .context("Failed to send VersionedTransaction")?;

        rpc_client
            .confirm_transaction_with_commitment(&signature, CommitmentConfig::finalized())
            .context("Failed to confirm transaction")?;

        println!("{} transaction confirmed, txId: {}", i + 1, signature);
        println!("🔍 http://solscan.io/tx/{}", signature);
    }

    Ok(())
}

fn read_keypair_from_file(path: &str) -> Result<Keypair> {
    let path_buf = PathBuf::from(path);
    let mut file = File::open(&path_buf)
        .with_context(|| format!("Failed to open keypair file: {:?}", path_buf))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .context("Failed to read keypair file as string")?;
    let raw: Vec<u8> = serde_json::from_str(&buf).context("Keypair JSON is not a `Vec<u8>`")?;
    let kp = Keypair::from_bytes(&raw).context("Invalid Keypair bytes (must be 64 bytes)")?;
    Ok(kp)
}
