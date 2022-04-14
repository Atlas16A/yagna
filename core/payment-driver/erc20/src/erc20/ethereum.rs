use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDateTime, Utc};
use ethabi::Token;
use lazy_static::lazy_static;
use tokio::sync::RwLock;
use uuid::Uuid;
use web3::contract::{Contract, Options};
use web3::transports::Http;
use web3::types::{
    BlockId, BlockNumber, Bytes, Transaction, TransactionId, TransactionReceipt, H160, H256, U256,
    U64,
};
use web3::Web3;

use ya_client_model::NodeId;
use ya_payment_driver::db::models::{Network, TransactionEntity, TransactionStatus, TxType};
use ya_payment_driver::utils::big_dec_to_u256;
use ya_payment_driver::{bus, model::GenericError};

use crate::dao::Erc20Dao;
use crate::erc20::transaction::YagnaRawTransaction;
use crate::erc20::wallet::get_next_nonce_info;
use crate::erc20::{config, eth_utils};
use num_traits::ToPrimitive;
use tokio::time::delay_for;

pub const FUND_WALLET_WAIT_TIME: u32 = 120;

pub const INIT_WALLET_WAIT_TIME: u32 = 500;
pub const APPROVE_CONTRACT_WAIT_TIME: u32 = INIT_WALLET_WAIT_TIME + 100;

pub enum PolygonPriority {
    PolygonPrioritySlow,
    PolygonPriorityFast,
    PolygonPriorityExpress,
}

pub enum PolygonGasPriceMethod {
    PolygonGasPriceStatic,
    PolygonGasPriceDynamic,
}

pub const POLYGON_PREFERRED_GAS_PRICES_SLOW: [f64; 6] = [0.0, 10.01, 15.01, 20.01, 25.01, 30.01];
pub const POLYGON_PREFERRED_GAS_PRICES_FAST: [f64; 3] = [0.0, 30.01, 40.01];
pub const POLYGON_PREFERRED_GAS_PRICES_EXPRESS: [f64; 3] = [0.0, 60.01, 100.01];

lazy_static! {
    pub static ref GLM_FAUCET_GAS: U256 = U256::from(90_000);
    pub static ref GLM_APPROVE_GAS: U256 = U256::from(200_000);
    pub static ref GLM_TRANSFER_GAS: U256 = U256::from(55_000);
    pub static ref CONTRACT_EXTRA_OVER_ESTIMATION: U256 = U256::from(20_000);
    pub static ref GLM_POLYGON_GAS_LIMIT: U256 = U256::from(100_000);
    static ref WEB3_CLIENT_MAP: Arc<RwLock<HashMap<String, Web3<Http>>>> = Default::default();
    pub static ref GLM_MINIMUM_ALLOWANCE: U256 = U256::max_value() / U256::from(2);
}
const CREATE_FAUCET_FUNCTION: &str = "create";
const BALANCE_ERC20_FUNCTION: &str = "balanceOf";
const TRANSFER_ERC20_FUNCTION: &str = "transfer";

pub fn get_polygon_starting_price() -> f64 {
    match get_polygon_priority() {
        PolygonPriority::PolygonPrioritySlow => POLYGON_PREFERRED_GAS_PRICES_SLOW[1],
        PolygonPriority::PolygonPriorityFast => POLYGON_PREFERRED_GAS_PRICES_FAST[1],
        PolygonPriority::PolygonPriorityExpress => POLYGON_PREFERRED_GAS_PRICES_EXPRESS[1],
    }
}

pub fn get_polygon_maximum_price() -> f64 {
    match get_polygon_gas_price_method() {
        PolygonGasPriceMethod::PolygonGasPriceStatic => match get_polygon_priority() {
            PolygonPriority::PolygonPrioritySlow => {
                POLYGON_PREFERRED_GAS_PRICES_SLOW[POLYGON_PREFERRED_GAS_PRICES_SLOW.len() - 1]
            }
            PolygonPriority::PolygonPriorityFast => {
                POLYGON_PREFERRED_GAS_PRICES_FAST[POLYGON_PREFERRED_GAS_PRICES_FAST.len() - 1]
            }
            PolygonPriority::PolygonPriorityExpress => {
                POLYGON_PREFERRED_GAS_PRICES_EXPRESS[POLYGON_PREFERRED_GAS_PRICES_EXPRESS.len() - 1]
            }
        },
        PolygonGasPriceMethod::PolygonGasPriceDynamic => get_polygon_max_gas_price_dynamic(),
    }
}

pub fn get_polygon_max_gas_price_dynamic() -> f64 {
    return std::env::var("POLYGON_MAX_GAS_PRICE_DYNAMIC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000.0f64);
}

pub fn get_polygon_gas_price_method() -> PolygonGasPriceMethod {
    match std::env::var("POLYGON_GAS_PRICE_METHOD")
        .ok()
        .map(|v| v.to_lowercase())
        .as_ref()
        .map(AsRef::as_ref) // Option<&str>
    {
        Some("static") => PolygonGasPriceMethod::PolygonGasPriceStatic,
        Some("dynamic") => PolygonGasPriceMethod::PolygonGasPriceDynamic,
        _ => PolygonGasPriceMethod::PolygonGasPriceDynamic,
    }
}

pub fn get_polygon_priority() -> PolygonPriority {
    match std::env::var("POLYGON_PRIORITY")
        .unwrap_or("default".to_string())
        .to_lowercase()
        .as_str()
    {
        "slow" => PolygonPriority::PolygonPrioritySlow,
        "fast" => PolygonPriority::PolygonPriorityFast,
        "express" => PolygonPriority::PolygonPriorityExpress,
        _ => PolygonPriority::PolygonPrioritySlow,
    }
}

pub async fn approve_multi_payment_contract(
    dao: &Erc20Dao,
    address: H160,
    network: Network,
) -> Result<(), GenericError> {
    let client = get_client(network).await?;
    let env = get_env(network);

    if let Some(contract_address) = env.glm_multi_transfer_contract_address {
        log::debug!("Checking multi payment contract for allowance...");
        let glm_contract = prepare_erc20_contract(&client, &env)?;
        let allowance: U256 = glm_contract
            .query(
                "allowance",
                (address, contract_address),
                None,
                Options::default(),
                None,
            )
            .await
            .map_err(GenericError::new)?;

        if allowance < *GLM_MINIMUM_ALLOWANCE {
            log::info!(
                "Allowance to low, calling approve: Address: {} Contract: {} Allowance: {:#x}",
                address,
                contract_address,
                allowance
            );
            //we have to approve multi payment contract to use our address
            let data: Vec<u8> = eth_utils::contract_encode(
                &glm_contract,
                "approve",
                (contract_address, U256::max_value()),
            )
            .map_err(GenericError::new)?;

            let gas_price = client.eth().gas_price().await.map_err(GenericError::new)?;
            //increase gas price by 100% to make sure transaction will proceed without issues
            let gas_price = gas_price * U256::from(15) / U256::from(10);
            let nonce_info = get_next_nonce_info(dao, address, network).await?;

            let tx = YagnaRawTransaction {
                nonce: U256::from(nonce_info.network_nonce_latest),
                to: Some(glm_contract.address()),
                value: U256::from(0),
                gas_price,
                gas: *GLM_APPROVE_GAS,
                data,
            };
            let dao_entity: TransactionEntity = create_dao_entity(
                U256::from(nonce_info.network_nonce_latest),
                address,
                gas_price.to_string(),
                Some(gas_price.to_string()),
                GLM_APPROVE_GAS.as_u32() as i32,
                serde_json::to_string(&tx).map_err(GenericError::new)?,
                network,
                Utc::now(),
                TxType::Approve,
                None,
            );
            dao.insert_raw_transaction(dao_entity)
                .await
                .map_err(GenericError::new)?;
            let start_time = SystemTime::now();
            log::info!("Wait until transaction is proceeded 30s...");
            delay_for(Duration::from_secs(30)).await;
            loop {
                log::info!("Checking allowance ...");
                let allowance: U256 = glm_contract
                    .query(
                        "allowance",
                        (address, contract_address),
                        None,
                        Options::default(),
                        None,
                    )
                    .await
                    .map_err(GenericError::new)?;

                if allowance >= *GLM_MINIMUM_ALLOWANCE {
                    break;
                }
                if start_time.elapsed().map_err(GenericError::new)?.as_secs() > 500 {
                    log::warn!("Waiting too long, transaction probably failed or is still waiting on chain");
                    break;
                }
                log::info!("Wait until transaction is proceeded 10s...");
                delay_for(Duration::from_secs(10)).await;
            }
        } else {
            log::debug!(
                "Allowance OK: Address: {} Contract: {} Allowance: {:#x}",
                address,
                contract_address,
                allowance
            );
        }
    }
    Ok(())
}

pub async fn get_glm_balance(address: H160, network: Network) -> Result<U256, GenericError> {
    let client = get_client(network).await?;
    let env = get_env(network);

    let glm_contract = prepare_erc20_contract(&client, &env)?;
    glm_contract
        .query(
            BALANCE_ERC20_FUNCTION,
            (address,),
            None,
            Options::default(),
            None,
        )
        .await
        .map_err(GenericError::new)
}

pub async fn get_balance(address: H160, network: Network) -> Result<U256, GenericError> {
    let client = get_client(network).await?;
    Ok(client
        .eth()
        .balance(address, None)
        .await
        .map_err(GenericError::new)?)
}

pub async fn get_transaction_count(
    address: H160,
    network: Network,
    pending: bool,
) -> Result<u64, GenericError> {
    let nonce_type = match pending {
        true => web3::types::BlockNumber::Pending,
        false => web3::types::BlockNumber::Latest,
    };
    let client = get_client(network).await?;
    let nonce = client
        .eth()
        .transaction_count(address, Some(nonce_type))
        .await
        .map_err(GenericError::new)?;
    Ok(nonce.as_u64())
}

pub async fn get_last_block_date(
    network: Network,
    block_number: u64,
) -> Result<DateTime<Utc>, GenericError> {
    let client = get_client(network).await?;
    let block_info = client
        .eth()
        .block(BlockId::Number(BlockNumber::Number(U64::from(
            block_number,
        ))))
        .await
        .map_err(GenericError::new)?
        .ok_or(GenericError::new("No latest block info returned"))?;
    let dt = DateTime::<Utc>::from_utc(
        NaiveDateTime::from_timestamp(
            block_info
                .timestamp
                .as_u64()
                .to_i64()
                .ok_or(GenericError::new("Failed timestamp convertion"))?,
            0,
        ),
        Utc,
    );
    Ok(dt)
}

pub async fn block_number(network: Network) -> Result<U64, GenericError> {
    let client = get_client(network).await?;
    Ok(client
        .eth()
        .block_number()
        .await
        .map_err(GenericError::new)?)
}

pub async fn sign_faucet_tx(
    address: H160,
    network: Network,
    nonce: u64,
) -> Result<TransactionEntity, GenericError> {
    let env = get_env(network);
    let client = get_client(network).await?;
    let contract = prepare_glm_faucet_contract(&client, &env)?;
    let contract = contract.ok_or(GenericError::new(
        "Failed to get faucet fn, are you on the right network?",
    ))?;

    let data = eth_utils::contract_encode(&contract, CREATE_FAUCET_FUNCTION, ()).unwrap();
    let gas_price = client.eth().gas_price().await.map_err(GenericError::new)?;
    //bump gas to prevent stuck transaction
    let gas_price = gas_price * U256::from(15) / U256::from(10);
    let tx = YagnaRawTransaction {
        nonce: U256::from(nonce),
        to: Some(contract.address()),
        value: U256::from(0),
        gas_price,
        gas: *GLM_FAUCET_GAS,
        data,
    };

    //let chain_id = network as u64;
    //let node_id = NodeId::from(address.as_ref());
    //let signature = bus::sign(node_id, eth_utils::get_tx_hash(&tx, chain_id)).await?;

    Ok(create_dao_entity(
        U256::from(nonce),
        address,
        gas_price.to_string(),
        Some(gas_price.to_string()),
        GLM_FAUCET_GAS.as_u32() as i32,
        serde_json::to_string(&tx).map_err(GenericError::new)?,
        network,
        Utc::now(),
        TxType::Faucet,
        None,
    ))
}

pub async fn sign_raw_transfer_transaction(
    address: H160,
    network: Network,
    tx: &YagnaRawTransaction,
) -> Result<Vec<u8>, GenericError> {
    let chain_id = network as u64;
    let node_id = NodeId::from(address.as_ref());
    let signature = bus::sign(node_id, eth_utils::get_tx_hash(&tx, chain_id)).await?;
    Ok(signature)
}

pub async fn prepare_erc20_transfer(
    _address: H160,
    recipient: H160,
    amount: U256,
    network: Network,
    nonce: U256,
    gas_price_override: Option<U256>,
    gas_limit_override: Option<u32>,
) -> Result<YagnaRawTransaction, GenericError> {
    let env = get_env(network);
    let client = get_client(network).await?;
    let contract = prepare_erc20_contract(&client, &env)?;

    let data = eth_utils::contract_encode(&contract, TRANSFER_ERC20_FUNCTION, (recipient, amount))
        .map_err(GenericError::new)?;

    //get gas price from network in not provided
    let gas_price = match gas_price_override {
        Some(gas_price_new) => gas_price_new,
        None => {
            let small_gas_bump = U256::from(1000);
            let mut gas_price_from_network =
                client.eth().gas_price().await.map_err(GenericError::new)?;

            //add small amount of gas to be first in queue
            if gas_price_from_network / 1000 > small_gas_bump {
                gas_price_from_network += small_gas_bump;
            }
            if network == Network::Rinkeby {
                //for testnet bump gas by 20% to not allow transactions to be stuck
                gas_price_from_network *= U256::from(1200);
                gas_price_from_network /= U256::from(1000);
            }
            gas_price_from_network
        }
    };

    let gas_limit = match network {
        Network::Polygon => gas_limit_override.map_or(*GLM_POLYGON_GAS_LIMIT, |v| U256::from(v)),
        _ => gas_limit_override.map_or(*GLM_TRANSFER_GAS, |v| U256::from(v)),
    };

    let tx = YagnaRawTransaction {
        nonce,
        to: Some(contract.address()),
        value: U256::from(0),
        gas_price,
        gas: gas_limit,
        data,
    };
    Ok(tx)
}

pub async fn prepare_erc20_multi_transfer(
    _address: H160,
    receivers: Vec<H160>,
    amounts: Vec<U256>,
    network: Network,
    nonce: U256,
    gas_price_override: Option<U256>,
    gas_limit_override: Option<u32>,
) -> Result<YagnaRawTransaction, GenericError> {
    let env = get_env(network);
    let client = get_client(network).await?;
    let contract = prepare_erc20_multi_contract(&client, &env)?;

    let packed: Vec<[u8; 32]> = receivers
        .iter()
        .zip(amounts.iter())
        .map(|(&receiver, &amount)| {
            let mut packet2 = [0u8; 32];
            amount.to_big_endian(&mut packet2[..]);
            packet2[..20].copy_from_slice(&receiver[..20]);
            packet2
        })
        .collect();

    let amount_sum = amounts.iter().fold(U256::from(0), |sum, e| sum + e);
    //for know use both methods interchangeably
    let direct = nonce.as_u64() % 2 == 0;
    let gas_estimation: U256;
    let data: Vec<u8>;
    if direct {
        let method = "golemTransferDirectPacked";

        gas_estimation = contract
            .estimate_gas(method, packed.clone(), _address, Options::default())
            .await
            .map_err(|err| GenericError::new(format!("Error when trying estimate gas {}", err)))?;
        //add some gas to increase gas limit just to be sure

        log::debug!("Gas estimation {}", gas_estimation);
        data = eth_utils::contract_encode(&contract, method, packed).map_err(GenericError::new)?;
    } else {
        let method = "golemTransferIndirectPacked";

        gas_estimation = contract
            .estimate_gas(
                method,
                (packed.clone(), amount_sum),
                _address,
                Options::default(),
            )
            .await
            .map_err(|err| GenericError::new(format!("Error when trying estimate gas {}", err)))?;
        //add some gas to increase gas limit just to be sure

        log::debug!("Gas estimation {}", gas_estimation);
        data = eth_utils::contract_encode(&contract, method, (packed, amount_sum))
            .map_err(GenericError::new)?;
    }

    //get gas price from network in not provided
    let gas_price = match gas_price_override {
        Some(gas_price_new) => gas_price_new,
        None => {
            let small_gas_bump = U256::from(1000);
            let mut gas_price_from_network =
                client.eth().gas_price().await.map_err(GenericError::new)?;

            //add small amount of gas to be first in queue
            if gas_price_from_network / 1000 > small_gas_bump {
                gas_price_from_network += small_gas_bump;
            }
            if network == Network::Rinkeby {
                //for testnet bump gas by 20% to not allow transactions to be stuck
                gas_price_from_network *= U256::from(1200);
                gas_price_from_network /= U256::from(1000);
            }
            gas_price_from_network
        }
    };

    let gas_estimation = gas_estimation + *CONTRACT_EXTRA_OVER_ESTIMATION;
    let gas_limit = gas_limit_override.map_or(gas_estimation, |v| U256::from(v));

    let tx = YagnaRawTransaction {
        nonce,
        to: Some(contract.address()),
        value: U256::from(0),
        gas_price,
        gas: gas_limit,
        data,
    };
    Ok(tx)
}

pub async fn send_tx(signed_tx: Vec<u8>, network: Network) -> Result<H256, GenericError> {
    let client = get_client(network).await?;
    let tx_hash = client
        .eth()
        .send_raw_transaction(Bytes::from(signed_tx))
        .await
        .map_err(GenericError::new)?;
    Ok(tx_hash)
}

pub struct TransactionChainStatus {
    pub exists_on_chain: bool,
    pub pending: bool,
    pub confirmed: bool,
    pub succeeded: bool,
    pub gas_used: Option<U256>,
    pub gas_price: Option<U256>,
}

pub async fn get_tx_on_chain_status(
    tx_hash: H256,
    current_block: u64,
    network: Network,
) -> Result<TransactionChainStatus, GenericError> {
    let mut res = TransactionChainStatus {
        exists_on_chain: false,
        pending: false,
        confirmed: false,
        succeeded: false,
        gas_price: None,
        gas_used: None,
    };
    let env = get_env(network);
    let tx = get_tx_receipt(tx_hash, network).await?;
    if let Some(tx) = tx {
        res.exists_on_chain = true;
        res.gas_used = tx.gas_used;
        const TRANSACTION_STATUS_SUCCESS: u64 = 1;
        if tx.status == Some(ethereum_types::U64::from(TRANSACTION_STATUS_SUCCESS)) {
            res.succeeded = true;
        }
        if let Some(tx_bn) = tx.block_number {
            // TODO: Store tx.block_number in DB and check only once after required_confirmations.
            log::trace!(
                "is_tx_confirmed? tb + rq - 1 <= cb. tb={}, rq={}, cb={}",
                tx_bn,
                env.required_confirmations,
                current_block
            );
            // tx.block_number is the first confirmation, so we need to - 1
            if tx_bn.as_u64() + env.required_confirmations - 1 <= current_block {
                res.confirmed = true;
            }
            let transaction = get_tx_from_network(tx_hash, network).await?;
            if let Some(t) = transaction {
                res.gas_price = Some(t.gas_price);
            }
        } else {
        }
    } else {
        let transaction = get_tx_from_network(tx_hash, network).await?;
        if let Some(_transaction) = transaction {
            res.exists_on_chain = true;
            res.pending = true;
        }
    }
    Ok(res)
}

//unused but tested that it is working for transfers
pub async fn decode_encoded_transaction_data(
    network: Network,
    encoded: &str,
) -> Result<(ethereum_types::Address, ethereum_types::U256), GenericError> {
    let env = get_env(network);
    let client = get_client(network).await?;
    let contract = prepare_erc20_contract(&client, &env)?;

    let raw_tx: YagnaRawTransaction = serde_json::from_str(encoded).map_err(GenericError::new)?;

    let tokens = eth_utils::contract_decode(&contract, TRANSFER_ERC20_FUNCTION, raw_tx.data)
        .map_err(GenericError::new)?;
    let mut address: Option<H160> = None;
    let mut amount: Option<U256> = None;
    for token in tokens {
        match token {
            Token::Address(val) => address = Some(val),
            Token::Uint(am) => amount = Some(am),
            _ => {}
        };
    }
    if let Some(add) = address {
        if let Some(am) = amount {
            return Ok((add, am));
        }
    }
    Err(GenericError::new("Failed to parse tokens"))
}

pub async fn get_tx_from_network(
    tx_hash: H256,
    network: Network,
) -> Result<Option<Transaction>, GenericError> {
    let client = get_client(network).await?;
    let result = client
        .eth()
        .transaction(TransactionId::from(tx_hash))
        .await
        .map_err(GenericError::new)?;
    Ok(result)
}

pub async fn get_tx_receipt(
    tx_hash: H256,
    network: Network,
) -> Result<Option<TransactionReceipt>, GenericError> {
    let client = get_client(network).await?;
    let result = client
        .eth()
        .transaction_receipt(tx_hash)
        .await
        .map_err(GenericError::new)?;
    Ok(result)
}

fn get_rpc_addr_from_env(network: Network) -> String {
    match network {
        Network::Mainnet => std::env::var("MAINNET_GETH_ADDR")
            .unwrap_or("https://geth.golem.network:55555".to_string()),
        Network::Rinkeby => std::env::var("RINKEBY_GETH_ADDR")
            .unwrap_or("http://geth.testnet.golem.network:55555".to_string()),
        Network::Goerli => {
            std::env::var("GOERLI_GETH_ADDR").unwrap_or("https://rpc.goerli.mudit.blog".to_string())
        }
        Network::Polygon => {
            std::env::var("POLYGON_GETH_ADDR").unwrap_or("https://bor.golem.network".to_string())
        }
        Network::Mumbai => std::env::var("MUMBAI_GETH_ADDR")
            .unwrap_or("https://matic-mumbai.chainstacklabs.com".to_string()),
    }
}

async fn get_client(network: Network) -> Result<Web3<Http>, GenericError> {
    let geth_addr = get_rpc_addr_from_env(network);

    {
        let client_map = WEB3_CLIENT_MAP.read().await;
        if let Some(client) = client_map.get(&geth_addr).cloned() {
            return Ok(client);
        }
    }

    let transport = web3::transports::Http::new(&geth_addr).map_err(GenericError::new)?;
    let client = Web3::new(transport);

    let mut client_map = WEB3_CLIENT_MAP.write().await;
    client_map.insert(geth_addr, client.clone());

    Ok(client)
}

pub fn get_env(network: Network) -> config::EnvConfiguration {
    match network {
        Network::Mainnet => *config::MAINNET_CONFIG,
        Network::Rinkeby => *config::RINKEBY_CONFIG,
        Network::Goerli => *config::GOERLI_CONFIG,
        Network::Mumbai => *config::MUMBAI_CONFIG,
        Network::Polygon => *config::POLYGON_MAINNET_CONFIG,
    }
}

fn prepare_contract(
    ethereum_client: &Web3<Http>,
    address: H160,
    json_abi: &[u8],
) -> Result<Contract<Http>, GenericError> {
    let contract =
        Contract::from_json(ethereum_client.eth(), address, json_abi).map_err(GenericError::new)?;

    Ok(contract)
}

fn prepare_erc20_contract(
    ethereum_client: &Web3<Http>,
    env: &config::EnvConfiguration,
) -> Result<Contract<Http>, GenericError> {
    prepare_contract(
        ethereum_client,
        env.glm_contract_address,
        include_bytes!("../contracts/ierc20.json"),
    )
}

fn prepare_erc20_multi_contract(
    ethereum_client: &Web3<Http>,
    env: &config::EnvConfiguration,
) -> Result<Contract<Http>, GenericError> {
    prepare_contract(
        ethereum_client,
        env.glm_multi_transfer_contract_address
            .ok_or(GenericError::new(
                "No multipayment contract defined for this environment",
            ))?,
        include_bytes!("../contracts/multi_transfer_erc20.json"),
    )
}

fn prepare_glm_faucet_contract(
    ethereum_client: &Web3<Http>,
    env: &config::EnvConfiguration,
) -> Result<Option<Contract<Http>>, GenericError> {
    if let Some(glm_faucet_address) = env.glm_faucet_address {
        Ok(Some(prepare_contract(
            ethereum_client,
            glm_faucet_address,
            include_bytes!("../contracts/faucet.json"),
        )?))
    } else {
        Ok(None)
    }
}

pub fn create_dao_entity(
    nonce: U256,
    sender: H160,
    starting_gas_price: String,
    max_gas_price: Option<String>,
    gas_limit: i32,
    encoded_raw_tx: String,
    network: Network,
    timestamp: DateTime<Utc>,
    tx_type: TxType,
    amount: Option<BigDecimal>,
) -> TransactionEntity {
    let current_naive_time = timestamp.naive_utc();
    TransactionEntity {
        tx_id: Uuid::new_v4().to_string(),
        sender: format!("0x{:x}", sender),
        nonce: nonce.as_u32() as i32,
        time_created: current_naive_time,
        time_last_action: current_naive_time,
        time_sent: None,
        time_confirmed: None,
        max_gas_price,
        final_gas_used: None,
        amount_base: Some("0".to_string()),
        amount_erc20: amount.as_ref().map(|a| big_dec_to_u256(a).to_string()),
        gas_limit: Some(gas_limit),
        starting_gas_price: Some(starting_gas_price),
        current_gas_price: None,
        encoded: encoded_raw_tx,
        status: TransactionStatus::Created as i32,
        tx_type: tx_type as i32,
        signature: None,
        tmp_onchain_txs: None,
        final_tx: None,
        network,
        last_error_msg: None,
        resent_times: 0,
    }
}

pub fn get_max_gas_costs(db_tx: &TransactionEntity) -> Result<U256, GenericError> {
    let raw_tx: YagnaRawTransaction =
        serde_json::from_str(&db_tx.encoded).map_err(GenericError::new)?;
    Ok(raw_tx.gas_price * raw_tx.gas)
}

pub fn get_gas_price_from_db_tx(db_tx: &TransactionEntity) -> Result<U256, GenericError> {
    let raw_tx: YagnaRawTransaction =
        serde_json::from_str(&db_tx.encoded).map_err(GenericError::new)?;
    Ok(raw_tx.gas_price)
}

pub async fn get_network_gas_price_eth(network: Network) -> Result<U256, GenericError> {
    let _env = get_env(network);
    let client = get_client(network).await?;

    let small_gas_bump = U256::from(1000);
    let mut gas_price_from_network = client.eth().gas_price().await.map_err(GenericError::new)?;

    //add small amount of gas to be first in queue
    if gas_price_from_network / 1000 > small_gas_bump {
        gas_price_from_network += small_gas_bump;
    }
    Ok(gas_price_from_network)
}
