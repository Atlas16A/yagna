/*
    Wallet functions on erc20.
*/

// External crates
use crate::erc20::ethereum::{
    get_polygon_gas_price_method, get_polygon_maximum_price, get_polygon_priority,
    get_polygon_starting_price, PolygonGasPriceMethod, PolygonPriority,
    POLYGON_PREFERRED_GAS_PRICES_EXPRESS, POLYGON_PREFERRED_GAS_PRICES_FAST,
    POLYGON_PREFERRED_GAS_PRICES_SLOW,
};
use bigdecimal::BigDecimal;
use chrono::Utc;
use num_bigint::BigUint;
use std::str::FromStr;
use web3::types::{H160, H256, U256, U64};

// Workspace uses
use ya_payment_driver::{
    db::models::{Network, TransactionEntity, TxType},
    model::{AccountMode, GenericError, Init, PaymentDetails},
};

// Local uses
use crate::erc20::gas_provider::get_network_gas_price;
use crate::erc20::transaction::YagnaRawTransaction;
use crate::{
    dao::Erc20Dao,
    erc20::{
        eth_utils, ethereum, faucet,
        utils::{
            big_dec_gwei_to_u256, big_dec_to_u256, big_uint_to_big_dec, convert_float_gas_to_u256,
            convert_u256_gas_to_float, str_to_addr, topic_to_str_address, u256_to_big_dec,
        },
    },
    RINKEBY_NETWORK,
};
use ya_payment_driver::db::models::TransactionStatus;

pub async fn account_balance(address: H160, network: Network) -> Result<BigDecimal, GenericError> {
    let balance_com = ethereum::get_glm_balance(address, network).await?;

    let balance = u256_to_big_dec(balance_com)?;
    log::debug!(
        "account_balance. address={}, network={}, balance={}",
        address,
        &network,
        &balance
    );
    Ok(balance)
}

pub async fn init_wallet(dao: &Erc20Dao, msg: &Init) -> Result<(), GenericError> {
    log::debug!("init_wallet. msg={:?}", msg);
    let mode = msg.mode();
    let address = msg.address();
    let network = msg.network().unwrap_or(RINKEBY_NETWORK.to_string());
    let network = Network::from_str(&network).map_err(|e| GenericError::new(e))?;

    if mode.contains(AccountMode::SEND) {
        let h160_addr = str_to_addr(&address)?;

        let glm_balance = ethereum::get_glm_balance(h160_addr, network).await?;
        if glm_balance == U256::zero() {
            return Err(GenericError::new("Insufficient GLM"));
        }

        let eth_balance = ethereum::get_balance(h160_addr, network).await?;
        if eth_balance == U256::zero() {
            return Err(GenericError::new("Insufficient ETH"));
        }

        ethereum::approve_multi_payment_contract(dao, h160_addr, network).await?;
    }
    Ok(())
}

pub async fn fund(dao: &Erc20Dao, address: H160, network: Network) -> Result<(), GenericError> {
    if network == Network::Mainnet {
        return Err(GenericError::new("Wallet can not be funded on mainnet."));
    }
    faucet::request_glm(dao, address, network).await?;
    Ok(())
}

#[derive(Debug)]
pub struct NextNonceInfo {
    pub network_nonce_pending: u64,
    pub network_nonce_latest: u64,
    pub db_nonce_pending: Option<u64>,
}

pub async fn get_next_nonce_info(
    dao: &Erc20Dao,
    address: H160,
    network: Network,
) -> Result<NextNonceInfo, GenericError> {
    let str_addr = format!("0x{:x}", &address);
    let network_nonce_pending = ethereum::get_transaction_count(address, network, true).await?;
    let network_nonce_latest = ethereum::get_transaction_count(address, network, false).await?;
    let db_nonce_pending = dao
        .get_last_db_nonce_pending(&str_addr, network)
        .await?
        .map(|last_db_nonce_pending| last_db_nonce_pending + 1);

    Ok(NextNonceInfo {
        network_nonce_pending,
        network_nonce_latest,
        db_nonce_pending,
    })
}

pub async fn get_next_nonce(
    dao: &Erc20Dao,
    address: H160,
    network: Network,
) -> Result<u64, GenericError> {
    let nonce_info = get_next_nonce_info(dao, address, network).await?;

    if let Some(db_nonce_pending) = nonce_info.db_nonce_pending {
        if nonce_info.network_nonce_pending > db_nonce_pending {
            warn!(
                "Network nonce higher than db nonce: {} != {}",
                nonce_info.network_nonce_pending, db_nonce_pending
            )
        };
        Ok(db_nonce_pending)
    } else {
        Ok(nonce_info.network_nonce_pending)
    }
}

pub async fn has_enough_eth_for_gas(
    db_tx: &TransactionEntity,
    network: Network,
) -> Result<BigDecimal, GenericError> {
    let sender_h160 = str_to_addr(&db_tx.sender)?;
    let eth_balance = ethereum::get_balance(sender_h160, network).await?;
    let gas_costs = ethereum::get_max_gas_costs(db_tx)?;
    let gas_price = ethereum::get_gas_price_from_db_tx(db_tx)?;
    let human_gas_cost = u256_to_big_dec(gas_costs)?;
    let human_gas_price = convert_u256_gas_to_float(gas_price);
    if gas_costs > eth_balance {
        return Err(GenericError::new(format!(
            "Not enough ETH balance for gas. balance={}, gas_cost={}, gas_price={} Gwei, address={}, network={}",
            u256_to_big_dec(eth_balance)?,
            &human_gas_cost,
            &human_gas_price,
            &db_tx.sender,
            &db_tx.network
        )));
    }
    Ok(human_gas_cost)
}

pub async fn get_block_number(network: Network) -> Result<U64, GenericError> {
    ethereum::block_number(network).await
}

pub async fn make_transfer(
    details: &PaymentDetails,
    nonce: u64,
    network: Network,
    gas_price: Option<BigDecimal>,
    max_gas_price: Option<BigDecimal>,
    gas_limit: Option<u32>,
) -> Result<TransactionEntity, GenericError> {
    log::debug!(
        "make_transfer(). network={}, nonce={}, details={:?}",
        &network,
        &nonce,
        &details
    );
    let amount_big_dec = details.amount.clone();
    let amount = big_dec_to_u256(&amount_big_dec)?;

    let (gas_price, max_gas_price) = match network {
        Network::Polygon => match get_polygon_gas_price_method() {
            PolygonGasPriceMethod::PolygonGasPriceStatic => (
                Some(match gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_starting_price()),
                }),
                Some(match max_gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_maximum_price()),
                }),
            ),
            PolygonGasPriceMethod::PolygonGasPriceDynamic => (
                Some(match gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_starting_price()),
                }),
                Some(match max_gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_maximum_price()),
                }),
            ),
        },
        _ => (
            match gas_price {
                None => None,
                Some(v) => Some(big_dec_gwei_to_u256(v)?),
            },
            match max_gas_price {
                None => None,
                Some(v) => Some(big_dec_gwei_to_u256(v)?),
            },
        ),
    };

    let address = str_to_addr(&details.sender)?;
    let recipient = str_to_addr(&details.recipient)?;
    // TODO: Implement token
    //let token = get_network_token(network, None);
    let mut raw_tx = ethereum::prepare_erc20_transfer(
        address,
        recipient,
        amount,
        network,
        U256::from(nonce),
        gas_price,
        gas_limit,
    )
    .await?;

    if let Some(max_gas_price) = max_gas_price {
        if raw_tx.gas_price > max_gas_price {
            raw_tx.gas_price = max_gas_price;
        }
    }

    Ok(ethereum::create_dao_entity(
        U256::from(nonce),
        address,
        raw_tx.gas_price.to_string(),
        max_gas_price.map(|v| v.to_string()),
        raw_tx.gas.as_u32() as i32,
        serde_json::to_string(&raw_tx).map_err(GenericError::new)?,
        network,
        Utc::now(),
        TxType::Transfer,
        Some(amount_big_dec),
    ))
}

pub async fn make_multi_transfer(
    details_array: Vec<PaymentDetails>,
    nonce: u64,
    network: Network,
    gas_price: Option<BigDecimal>,
    max_gas_price: Option<BigDecimal>,
    gas_limit: Option<u32>,
) -> Result<TransactionEntity, GenericError> {
    log::debug!(
        "make_transfer(). network={}, nonce={}, details={:?}",
        &network,
        &nonce,
        &details_array
    );
    let amounts = details_array
        .iter()
        .map(|details| big_dec_to_u256(&details.amount))
        .collect::<Result<Vec<U256>, GenericError>>()?;

    let (gas_price, max_gas_price) = match network {
        Network::Polygon => match get_polygon_gas_price_method() {
            PolygonGasPriceMethod::PolygonGasPriceStatic => (
                Some(match gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_starting_price()),
                }),
                Some(match max_gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_maximum_price()),
                }),
            ),
            PolygonGasPriceMethod::PolygonGasPriceDynamic => (
                Some(match gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_starting_price()),
                }),
                Some(match max_gas_price {
                    Some(v) => big_dec_gwei_to_u256(v)?,
                    None => convert_float_gas_to_u256(get_polygon_maximum_price()),
                }),
            ),
        },
        _ => (
            match gas_price {
                None => None,
                Some(v) => Some(big_dec_gwei_to_u256(v)?),
            },
            match max_gas_price {
                None => None,
                Some(v) => Some(big_dec_gwei_to_u256(v)?),
            },
        ),
    };

    let senders = details_array
        .iter()
        .map(|details| str_to_addr(&details.sender))
        .collect::<Result<Vec<web3::types::Address>, GenericError>>()?;
    let address = senders
        .get(0)
        .ok_or(GenericError::new("Senders cannot be empty"))?;
    for (index, sender) in senders.iter().enumerate() {
        if address != sender {
            return Err(GenericError::new(format!(
                "Senders have to be the same idx:{} left:{} right:{}",
                index, address, sender
            )));
        }
    }

    let amount_sum = amounts.iter().fold(U256::from(0), |sum, e| sum + e);

    let recipients = details_array
        .iter()
        .map(|details| str_to_addr(&details.recipient))
        .collect::<Result<Vec<web3::types::Address>, GenericError>>()?;
    // TODO: Implement token
    //let token = get_network_token(network, None);
    let mut raw_tx = ethereum::prepare_erc20_multi_transfer(
        *address,
        recipients,
        amounts,
        network,
        U256::from(nonce),
        gas_price,
        gas_limit,
    )
    .await?;

    if let Some(max_gas_price) = max_gas_price {
        if raw_tx.gas_price > max_gas_price {
            raw_tx.gas_price = max_gas_price;
        }
    }

    Ok(ethereum::create_dao_entity(
        U256::from(nonce),
        *address,
        raw_tx.gas_price.to_string(),
        max_gas_price.map(|v| v.to_string()),
        raw_tx.gas.as_u32() as i32,
        serde_json::to_string(&raw_tx).map_err(GenericError::new)?,
        network,
        Utc::now(),
        TxType::Transfer,
        Some(u256_to_big_dec(amount_sum)?),
    ))
}

fn bump_gas_price(gas_in_gwei: U256) -> U256 {
    let min_bump_num: U256 = U256::from(111u64);
    let min_bump_den: U256 = U256::from(100u64);
    let min_gas = gas_in_gwei * min_bump_num / min_bump_den;

    match get_polygon_gas_price_method() {
        PolygonGasPriceMethod::PolygonGasPriceDynamic => {
            //ignore maximum gas price, because we have to bump at least 10% so the transaction will be accepted
            min_gas
        }
        PolygonGasPriceMethod::PolygonGasPriceStatic => {
            let polygon_prices = get_polygon_priority();

            let gas_prices: &[f64] = match polygon_prices {
                PolygonPriority::PolygonPriorityExpress => {
                    &POLYGON_PREFERRED_GAS_PRICES_EXPRESS[..]
                }
                PolygonPriority::PolygonPriorityFast => &POLYGON_PREFERRED_GAS_PRICES_FAST[..],
                PolygonPriority::PolygonPrioritySlow => &POLYGON_PREFERRED_GAS_PRICES_SLOW[..],
            };

            gas_prices
                .iter()
                .map(|&f| convert_float_gas_to_u256(f))
                .find(|&gas_price_step| gas_price_step > min_gas)
                .unwrap_or(min_gas)
        }
    }
}

pub async fn send_transactions(
    dao: &Erc20Dao,
    txs: Vec<TransactionEntity>,
    network: Network,
) -> Result<(), GenericError> {
    // TODO: Use batch sending?
    let mut current_max_gas_price = U256::from(0);
    for tx in txs {
        let mut raw_tx: YagnaRawTransaction =
            match serde_json::from_str::<YagnaRawTransaction>(&tx.encoded) {
                Ok(raw_tx) => raw_tx,
                Err(err) => {
                    log::error!("Error during serialization of json: {:?}", err);
                    log::error!(
                        "send_transactions - YagnaRawTransaction serialization failed: {:?}",
                        err
                    );
                    //handle problem when deserializing transaction
                    dao.transaction_confirmed_and_failed(
                        &tx.tx_id,
                        "",
                        None,
                        "Json parse failed, unrecoverable error",
                    )
                    .await;
                    continue;
                }
            };

        let address = str_to_addr(&tx.sender)?;

        let new_gas_price = if let Some(current_gas_price) = tx.current_gas_price {
            //***************************************
            // resolve gas bump transaction here
            //***************************************

            if tx.status == TransactionStatus::ResendAndBumpGas as i32 {
                let gas_u256 = U256::from_dec_str(&current_gas_price).map_err(GenericError::new)?;

                let max_gas_u256 = match tx.max_gas_price {
                    Some(max_gas_price) => {
                        Some(U256::from_dec_str(&max_gas_price).map_err(GenericError::new)?)
                    }
                    None => None,
                };
                let new_gas = bump_gas_price(gas_u256);
                if let Some(max_gas_u256) = max_gas_u256 {
                    if gas_u256 > max_gas_u256 {
                        log::warn!(
                            "bump gas ({}) larger than max gas ({}) price",
                            gas_u256,
                            max_gas_u256
                        )
                    }
                }
                new_gas
            } else {
                U256::from_dec_str(&current_gas_price).map_err(GenericError::new)?
            }
        } else if let Some(starting_gas_price) = tx.starting_gas_price {
            //*******************************************
            // resolve first transaction gas price here
            //*******************************************

            let network_price = get_network_gas_price(network).await?;
            let minimum_price =
                U256::from_dec_str(&starting_gas_price).map_err(GenericError::new)?;

            let mut new_gas_price = if network_price > minimum_price {
                network_price
            } else {
                minimum_price
            };

            let max_gas_price = match tx.max_gas_price {
                Some(max_gas_price) => {
                    Some(U256::from_dec_str(&max_gas_price).map_err(GenericError::new)?)
                }
                None => None,
            };
            //first transaction gas_price cannot be bigger than max_gas_price set
            if let Some(max_gas_price) = max_gas_price {
                new_gas_price = if max_gas_price < new_gas_price {
                    max_gas_price
                } else {
                    new_gas_price
                }
            }
            new_gas_price
        } else {
            convert_float_gas_to_u256(get_polygon_starting_price())
        };
        raw_tx.gas_price = new_gas_price;
        if new_gas_price > current_max_gas_price && current_max_gas_price != U256::from(0) {
            // Do not send transaction with gas higher than previously sent transaction.
            // This is preventing bumping gas for future transaction over existing ones
            log::debug!(
                "Skipping transaction send, because transaction with lower gas is already waiting"
            );
            continue;
        }
        current_max_gas_price = new_gas_price;

        let encoded = serde_json::to_string(&raw_tx).map_err(GenericError::new)?;
        let signature = ethereum::sign_raw_transfer_transaction(address, network, &raw_tx).await?;

        //save new parameters to db before proceeding. Maybe we should change status to sending
        dao.update_tx_fields(
            &tx.tx_id,
            encoded,
            hex::encode(&signature),
            Some(new_gas_price.to_string()),
        )
        .await;

        let signed = eth_utils::encode_signed_tx(&raw_tx, signature, network as u64);

        match ethereum::send_tx(signed, network).await {
            Ok(tx_hash) => {
                let str_tx_hash = format!("0x{:x}", &tx_hash);
                let str_tx_hash = if let Some(tmp_onchain_txs) = tx.tmp_onchain_txs {
                    tmp_onchain_txs + ";" + str_tx_hash.as_str()
                } else {
                    str_tx_hash
                };
                dao.transaction_sent(&tx.tx_id, &str_tx_hash, Some(raw_tx.gas_price.to_string()))
                    .await;
                log::info!("Send transaction. hash={}", &tx_hash);
                log::debug!("id={}", &tx.tx_id);
            }
            Err(e) => {
                log::error!("Error sending transaction: {:?}", e);
                if e.to_string().contains("nonce too low") {
                    if tx.tmp_onchain_txs.filter(|v| !v.is_empty()).is_some() && tx.resent_times < 5
                    {
                        //if tmp on-chain tx transactions exist give it a chance but marking it as failed sent
                        dao.transaction_failed_send(
                            &tx.tx_id,
                            tx.resent_times + 1,
                            e.to_string().as_str(),
                        )
                        .await;
                        continue;
                    } else {
                        //if trying to sent transaction too much times just end with unrecoverable error
                        log::error!("Nonce too low: {:?}", e);
                        dao.transaction_failed_with_nonce_too_low(
                            &tx.tx_id,
                            e.to_string().as_str(),
                        )
                        .await;
                        continue;
                    }
                }
                if e.to_string().contains("already known") {
                    log::error!("Already known: {:?}. Send transaction with higher gas to get from this error loop. (resent won't fix anything)", e);
                    dao.retry_send_transaction(&tx.tx_id, true).await;
                    continue;
                }

                dao.transaction_failed_send(&tx.tx_id, tx.resent_times, e.to_string().as_str())
                    .await;
            }
        }
    }
    Ok(())
}

// TODO: calculate fee. Below commented out reference to zkSync implementation
// pub async fn get_tx_fee(address: &str, network: Network) -> Result<BigDecimal, GenericError> {
//     // let token = get_network_token(network, None);
//     // let wallet = get_wallet(&address, network).await?;
//     // let tx_fee = wallet
//     //     .provider
//     //     .get_tx_fee(TxFeeTypes::Transfer, wallet.address(), token.as_str())
//     //     .await
//     //     .map_err(GenericError::new)?
//     //     .total_fee;
//     // let tx_fee_bigdec = utils::big_uint_to_big_dec(tx_fee);
//     //
//     // log::debug!("Transaction fee {:.5} {}", tx_fee_bigdec, token.as_str());
//     // Ok(tx_fee_bigdec)
//     todo!();
// }

pub async fn verify_tx(tx_hash: &str, network: Network) -> Result<PaymentDetails, GenericError> {
    log::debug!("verify_tx. hash={}", tx_hash);
    let hex_hash = H256::from_str(&tx_hash[2..]).map_err(|err| {
        log::warn!("tx hash failed to parse: {}", tx_hash);
        GenericError::new(err)
    })?;
    let tx = ethereum::get_tx_receipt(hex_hash, network)
        .await
        .map_err(|err| {
            log::warn!(
                "Failed to obtain tx receipt from blockchain network: {}",
                hex_hash
            );
            err
        })?;

    if let Some(tx) = tx {
        // TODO: Properly parse logs after https://github.com/tomusdrw/rust-web3/issues/208
        // let tx_log = tx.logs.get(0).unwrap_or_else(|| GenericError::new(format!("Failure when parsing tx: {} ", tx_hash)))?;

        let tx_log = tx.logs.get(0).ok_or_else(|| {
            GenericError::new(format!("Failure when parsing tx.logs.get(0): {} ", tx_hash))
        })?;
        let (topic1, topic2) = match tx_log.topics.as_slice() {
            [_, t1, t2] => (t1, t2),
            _ => {
                return Err(GenericError::new(format!(
                    "Failure when parsing tx_log.topics.get(1): {} ",
                    tx_hash
                )))
            }
        };

        let sender = topic_to_str_address(topic1);
        let recipient = topic_to_str_address(topic2);

        let amount = big_uint_to_big_dec(BigUint::from_bytes_be(&tx_log.data.0));

        if let Some(_block_number) = tx_log.block_number {
            // TODO: Get date from block
        }
        let date = Some(chrono::Utc::now());

        let details = PaymentDetails {
            recipient,
            sender,
            amount,
            date,
        };
        log::debug!("PaymentDetails from blockchain: {:?}", &details);

        Ok(details)
    } else {
        Err(GenericError::new(format!(
            "Transaction {} not found on chain",
            tx_hash
        )))
    }
}
