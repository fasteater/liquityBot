use ethers::{
    prelude::{*, k256::{ecdsa::SigningKey, elliptic_curve::consts::U2}},
    contract::abigen,
    core::types::{Address, U256},
    core::types::transaction::eip2718::TypedTransaction,
    providers::{Provider, Middleware, StreamExt, Ws},
    signers::{Wallet},
};
use std::{sync::Arc, env, error::Error};
use std::time::{Duration, Instant};
use tokio;
use tokio::time::{timeout};
use ethers_flashbots::*;
use url::Url;


abigen!(
    TROVE_MANAGER,
    r#"[
        getTroveOwnersCount()(uint)
        TroveOwners(uint)(address)
        getCurrentICR(address, uint)(uint)
        getEntireDebtAndColl(address)(uint, uint, uint, uint)
        liquidateTroves(uint)
    ]"#,
);

abigen!(
    CHAINLINK_FEED_REGISTRY,
    r#"[
        latestRoundData(address, address)(uint,uint,uint,uint,uint)
    ]"#,
);

abigen!(
    SORTED_TROVE,
    r#"[
        getLast()(address)
        getPrev(address)(address)
    ]"#,
);

//approach
//1. listen to new blocks
//2. get Eth price from chainlink, scale it up to 18 decimals (chainlink default 8 decimals 
//3. get the tail addresses from sortedTroves.sol and check health, if found any unhealthy, check prev positions also, till we hit a healthy position
//4. liquidate all n unhealthy positions with manager.liquidateTroves(uint _n), via flashbot tx, offering 200 USD in gas fees, keep 0.5% eth collateral to myself

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>>{

    let provider = Provider::<Ws>::connect(&env::var("END_POINT").unwrap()).await.unwrap(); //TODO wrap provider in Arc for sharing? e.g. https://github.com/gakonst/ethers-rs/blob/master/examples/transactions/examples/gas_price_usd.rs
    let client = Arc::new(provider);

    let bot_wallet0 = Wallet::decrypt_keystore("./.cargo/mm3_bot_keystore.json",&env::var("BOT_ACC_KEYSTORE_PASS").unwrap()).unwrap(); //mm3 - 0xedA8f1dc3Deee0Af4d98066e7F398f7151CC2812
    dbg!(&bot_wallet0);

    let flashbot_reg_wallet0 = Wallet::decrypt_keystore("./.cargo/flashbot_reg_keystore.186649000Z--ff584ffe16f497a8aa3ef660424e8132905e538c",&env::var("FLASHBOT_REG_ACC_KEYSTORE_PASS").unwrap()).unwrap(); //mm12
    dbg!(&flashbot_reg_wallet0);
    
    let trove_manager_add: Address = "0xA39739EF8b0231DbFA0DcdA07d7e29faAbCf4bb2".parse().unwrap();
    let trove_manager_contract0 = TROVE_MANAGER::new(trove_manager_add, client.clone());
    
    //   //dev test
    //   liquidate_troves(2, &trove_manager_add, bot_wallet.clone(), flashbot_reg_wallet.clone(), &U256::from_dec_str("1200000000000000000000").unwrap()).await;

    let sorted_troves_add:Address = "0x8FdD3fbFEb32b28fb73555518f8b361bCeA741A6".parse().unwrap();
    let sorted_troves_contract0 = SORTED_TROVE::new(sorted_troves_add, client.clone());

    let chainlink_feed_add: Address = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf".parse().unwrap();
    let chainlink_feed_registry0 = CHAINLINK_FEED_REGISTRY::new(chainlink_feed_add, client.clone());
    
    let mcr:U256 = U256::from_dec_str("1100000000000000000")?; 

    loop { //use a loop to handle auto ws re-connection
        println!("new loop");
        //1. listen to new blocks
        let ws_result = Ws::connect(&env::var("END_POINT").unwrap()).await;
        match ws_result {
            Ok(ws) => {
                let ws = ws;
                let provider0 = Provider::new(ws).interval(Duration::from_millis(1000));

                let mut stream= provider0.watch_blocks().await?;
        
                loop {
                    match timeout(Duration::from_secs(20), stream.next()).await {
                        Ok(Some(block)) => {
                            // The future completed within the timeout
                            println!("Async function completed within the timeout.");
            
                            // while let Some(block) = stream.next().await {
                                let provider = provider0.clone();
                                let block_result = provider.get_block(block).await;
                    
                                match block_result {
                    
                                    Err(e) => println!("err getting block from provider"),
                                    Ok(block_option) => {
                                        match block_option {
                                            None => println!("got a none block"),
                                            Some(block) =>{
                    
                                                println!("========================== new block check {} ========================== ", block.number.unwrap());                    
                                                let trove_manager_contract = trove_manager_contract0.clone();
                                                let sorted_troves_contract = sorted_troves_contract0.clone();
                                                let chainlink_feed_registry = chainlink_feed_registry0.clone();
                                                let bot_wallet = bot_wallet0.clone();
                                                let flashbot_reg_wallet = flashbot_reg_wallet0.clone();
                                        
                                                let task = tokio::spawn(async move {  
                                                    println!("task spawned");
                                                    //2. get Eth price from chainlink, scale it up to 18 decimals (chainlink default 8 decimals 
                                                    // let current_eth_price:U256 = U256::from_dec_str("1000000000000000000000").unwrap(); //Dev only
                                                    let current_eth_price_result = get_asset_latest_usd_value_chainlink(chainlink_feed_registry.clone(), "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE".parse().unwrap()).await; //0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE is eth address 
                                                    match current_eth_price_result { 
                                                        Err(e) =>  println!("err getting eth price"),
                                                        Ok(price) => {
                                                            let current_eth_price = price;
                                                            println!("got eth price {}", current_eth_price);
                                                            //3. get the tail addresses from sortedTroves.sol and check health, if found any unhealthy, check prev positions also, till we hit a healthy position
                                                            let mut tail_user_result = sorted_troves_contract.get_last().call().await;
                                                            match tail_user_result {
                                                                Ok(mut tail_user) => {
                                                                    let mut unhealthy_position_count = 0;
                                                                    loop {
                                                            
                                                                        //check health
                                                                        let user_current_icr_result = trove_manager_contract.get_current_icr(tail_user, current_eth_price).call().await;
                                                                        match user_current_icr_result {
                                                                            Ok(user_current_icr) => {
                    
                                                                                if user_current_icr < mcr {
                                                                                    unhealthy_position_count += 1;
                                                                                    println!("found unhealthy position {}", unhealthy_position_count);
                                                                                    tail_user_result = sorted_troves_contract.get_prev(tail_user).call().await;
                                                                                    match tail_user_result {
                                                                                        Ok(new_tail_user) => { tail_user = new_tail_user}
                                                                                        Err(e) => {println!("err getting next tail user {:?}", e);}
                                                                                    }
                                                                                } else {
                                                                                    break;
                                                                                }
                    
                                                                            },
                                                                            Err(e) => {println!("err getting user icr {:?}", e)}
                                                                        }
                                                                    };
                                                        
                                                                    println!("got {} unhealthy positions", unhealthy_position_count);
                                                                    if unhealthy_position_count > 0 {
                                                            
                                                                        //4. liquidate all n unhealthy positions with manager.liquidateTroves(uint _n), via flashbot tx, offering 200 USD in gas fees, keep 0.5% eth collateral to myself
                                                                        liquidate_troves(unhealthy_position_count, &trove_manager_add, bot_wallet.clone(), flashbot_reg_wallet.clone(), &current_eth_price).await;
                                                                    }; 
                                                                }
                                                                Err(e) => {println!("err getting tail user {:?}", e)}
                                                            } 
                                                        },   
                                                    }             
                                                });
                                                let task_result = task.await;
                                                match task_result {
                                                    Ok(()) => {},
                                                    Err(e) => println!{"task err {:?}", e}
                                                }
                                            }
                                        }
                                    }
                                }
                                // Ok(())
                            // };
                        },
                        Ok(None) => {
                            println!("got empty stream");
                            break;
                        },
                        Err(e) => {
                            // The future timed out
                            println!("Async function timed out: {:?}", e);
                            break;
                        },
        
                    }
                }
            }

            Err(e) => {
                println!("err getting ws {:?}", e);
            }
        };
    }
}

async fn liquidate_troves(unhealthy_position_count:i32, trove_manager_add:&Address, bot_wallet:Wallet<SigningKey>, flashbot_reg_wallet:Wallet<SigningKey>, current_eth_price:&U256){

    println!("liquidate {} unhealthy positions", unhealthy_position_count);
    //send tx with flashbots

    let provider = Provider::<Ws>::connect(&env::var("END_POINT").unwrap()).await.unwrap();  
    let client2 = Arc::new(provider.clone());
    

    let trove_manager_contract = TROVE_MANAGER::new(*trove_manager_add, client2);

    // Add signer and Flashbots middleware
    let client = SignerMiddleware::new(
        FlashbotsMiddleware::new(
            provider.clone(),
            Url::parse("https://relay.flashbots.net").unwrap(),
            flashbot_reg_wallet,
        ),
        bot_wallet.clone(),
    );

    //build liquidate tx
    let tx_data = trove_manager_contract.encode("liquidateTroves", U256::from(unhealthy_position_count)).unwrap(); 
    // println!("tx data is {:?}", tx_data);

    //compute gas price
    //from past tx observation, the liquidate fn gas cost is around 450k base then each liquidation around 50k
    //bidding strategy, for every liquidation I get 200 LUSD + 0.5% ETH collateral. I pay all 200 LUSD as gas, just gain eth collateral
    let rough_gas_consumption = U256::from(450_000 + 50_000 * unhealthy_position_count);
    let gas_price:U256 = U256::from(200*unhealthy_position_count) * U256::from_dec_str("1_000000000000000000").unwrap() / (current_eth_price/U256::from_dec_str("1_000000000000000000").unwrap()) / rough_gas_consumption; 
    println!("gas price is {:?}", gas_price);

    let tx= TransactionRequest::new()
                                 .to(*trove_manager_add)
                                 .from(bot_wallet.clone().address())
                                //  .gas() 
                                 .gas_price(gas_price) //from past tx observation, the liquidate fn gas cost is around 500k for 1 tx and 50k for any extra tx
                                //  .value()
                                 .data(tx_data);
                                //  .nonce()
                                //  .chain_id()
                                //  .fee_currency()
                                //  .gateway_fee_recipient()
                                //  .gateway_Fee()
   
    //advanced --- put tx into a bundle we can simulate it
    // get last block number
    let block_number = client.get_block_number().await.unwrap();

    // Build a custom bundle 
    let tx = {
        let mut inner: TypedTransaction = tx.into();
        client.fill_transaction(&mut inner, None).await.unwrap(); //auto fill all the rest of the tx fields
        inner
    };
    let signature = client.signer().sign_transaction(&tx).await.unwrap(); // sign tx and put in a bundle
    let bundle = BundleRequest::new()
        .push_transaction(tx.rlp_signed(&signature))
        .set_block(block_number + 1)
        .set_simulation_block(block_number)
        .set_simulation_timestamp(0);

    // // Simulate it
    // let simulated_bundle = client.inner().simulate_bundle(&bundle).await.unwrap();
    // println!("Simulated bundle: {:?}", simulated_bundle);

    // Send it
    let pending_bundle = client.inner().send_bundle(&bundle).await.unwrap();

    // You can also optionally wait to see if the bundle was included
    match pending_bundle.await {
        Ok(bundle_hash) => println!(
            "Bundle with hash {:?} was included in target block",
            bundle_hash
        ),
        Err(PendingBundleError::BundleNotIncluded) => {
            println!("Bundle was not included in target block.")
        }
        Err(e) => println!("An error occured: {}", e),
    }

}

async fn get_asset_latest_usd_value_chainlink(chainlink_feed_registry:CHAINLINK_FEED_REGISTRY<Provider<Ws>>, mut asset_address:Address) -> Result<U256, Box<dyn Error + Send + Sync>>{
   
    //adjust collateral assets if it is weth and wbtc, chainlink doesn't like these two
    if asset_address == "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".parse().unwrap() {
        asset_address = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE".parse().unwrap();
    } else if asset_address == "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599".parse().unwrap() {
        asset_address = "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB".parse().unwrap();
    };

    let usd_address = "0x0000000000000000000000000000000000000348".parse().unwrap();

    let asset_price_usd:(U256, U256, U256, U256, U256) = chainlink_feed_registry.latest_round_data(asset_address, usd_address).call().await?;

    Ok(asset_price_usd.1 * U256::from_dec_str("10000000000").unwrap()) //chailink returns 10*8 scaled value, we want 10*18

}