use ethers::{
    contract::abigen,
    core::types::{Address, U256},
    providers::{Provider, Middleware, StreamExt, Ws}, prelude::k256::elliptic_curve::consts::U2,
};
use eyre::Result;
use std::{sync::Arc, str::FromStr, env};
use std::time::Duration;
use tokio;

abigen!(
    TROVE_MANAGER,
    r#"[
        getTroveOwnersCount()(uint)
        TroveOwners(uint)(address)
        getCurrentICR(address, uint)(uint)
        getEntireDebtAndColl(address)(uint, uint, uint, uint)
    ]"#,
);

abigen!(
    CHAINLINK_FEED_REGISTRY,
    r#"[
        latestRoundData(address, address)(uint,uint,uint,uint,uint)
    ]"#,
);

const LIQUITY_TROVE_MANAGER_ADD: &str = "0xA39739EF8b0231DbFA0DcdA07d7e29faAbCf4bb2";

//approach
//1. get totalTroveAmt from getTroveOwnersCount(), loop through totalTroveAmt, get owners address from TroveOwners[i] also calculate liquidationEthPrice in eth from values from getEntireDebtAndColl() also save in vec, rank the vec based on liquidationEthPrice from high to low
//2. listen to new blocks
//3. get Eth price from chainlink, scale it up to 18 decimals (chainlink default 8 decimals 
//4. check health with getCurrentICR(userAdd), if < MCR, we liquidate user


#[tokio::main]
async fn main() -> Result<()> {

    let provider2 = Provider::<Ws>::connect(&env::var("ALCHEMY_END_POINT").unwrap()).await?;
    let client = Arc::new(provider2);
    let address: Address = LIQUITY_TROVE_MANAGER_ADD.parse()?;
    let contract = TROVE_MANAGER::new(address, client);
    
    let MCR:U256 = U256::from_dec_str("1100000000000000000")?; 
    
    //1. get totalTroveAmt from getTroveOwnersCount(), loop through totalTroveAmt, get owners address from TroveOwners[i] save in local vec
    let total_trove_amt = contract.get_trove_owners_count().call().await?; //TODO - turn on this for production to get all users
    // let total_trove_amt:U256 = U256::from(10); //Dev only

    let mut all_trove_owner_and_their_liqprice = vec![]; 
    let mut i:u32 = 0;
    loop {

        if i >= total_trove_amt.as_u32() {
            break;
        }

        //get owner add
        let owner:Address = contract.trove_owners(U256::from(i)).call().await?;
        
        //get owner's liquidation price, 
        let user_var:(U256, U256, U256, U256) = contract.get_entire_debt_and_coll(owner).call().await?; //returns (userDebt, userColl, , )
        let user_liquidation_price_eth = MCR * user_var.0 / user_var.1; //scaled by 10*18.  liq_price * coll / debt < 110 => liq_price = 110 * debt/coll. if Eth < this price, user is liquidatable
        
        //save owner info to vec
        all_trove_owner_and_their_liqprice.push((owner, user_liquidation_price_eth));
        println!("Total user tracked {}/{}", i,total_trove_amt);
        
        i += 1;
    };

    all_trove_owner_and_their_liqprice.sort_by(|a, b| a.1.cmp(&b.1)); //order local vec by liq price high to low
    println!("{:?}", &all_trove_owner_and_their_liqprice);

    let ws = Ws::connect(&env::var("ALCHEMY_END_POINT").unwrap()).await?;
    let provider = Provider::new(ws).interval(Duration::from_millis(1000));


    //2. listen to new blocks
    let mut stream = provider.watch_blocks().await?;
    while let Some(block) = stream.next().await {
        
        let block = provider.get_block(block).await?.unwrap();
        println!("========================== new block check {} ========================== ", block.number.unwrap());
        
        //3. get Eth price from chainlink, scale it up to 18 decimals (chainlink default 8 decimals 
        let current_eth_price:U256 = get_asset_latest_usd_value_chainlink("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE".parse().unwrap()).await; //0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE is eth address
        // let current_eth_price:U256 = U256::from_dec_str("500000000000000000000").unwrap(); //Dev only
        println!("got eth price {}", current_eth_price);
        
        //4. compare current_eth_price with locally saved liq price in vec, any position from the found index up can be liquidated. 
        if &all_trove_owner_and_their_liqprice[all_trove_owner_and_their_liqprice.len() - 1].1 < &current_eth_price {  //first check if the highest liq price is under water, if not -> nothing to liq.
            println!("nothing to liquidate");
        } else {

            let mut liq_index = find_liquidatable_position_starting_index(all_trove_owner_and_their_liqprice.clone(), current_eth_price);
   
            //TODO - send liquidation request 
        
        }
    }

    Ok(())
}


async fn get_asset_latest_usd_value_chainlink(mut asset_address:Address) -> U256{
   
    let provider = Provider::<Ws>::connect(&env::var("ALCHEMY_END_POINT").unwrap()).await.unwrap();
    let client = Arc::new(provider);
    let chainlink_feed_add: Address = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf".parse().unwrap();
    let chainlink_feed_registery = CHAINLINK_FEED_REGISTRY::new(chainlink_feed_add, client);

    //adjust collateral assets if it is weth and wbtc, chainlink doesn't like these two
    if asset_address == "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".parse().unwrap() {
        asset_address = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE".parse().unwrap();
    } else if asset_address == "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599".parse().unwrap() {
        asset_address = "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB".parse().unwrap();
    } else { 
    };

    let usd_address = "0x0000000000000000000000000000000000000348".parse().unwrap();

    let asset_price_usd:(U256, U256, U256, U256, U256) = chainlink_feed_registery.latest_round_data(asset_address, usd_address).call().await.unwrap();

    asset_price_usd.1 * U256::from_dec_str("10000000000").unwrap()

}

fn find_liquidatable_position_starting_index( all_trove_owner_and_their_liqprice:Vec<(Address, U256)>, current_eth_price:U256) -> usize {
       
    //find the liq_index point, from this point to the all_trove_owner_and_their_liqprice.len() are all liquidable
       let mut i = all_trove_owner_and_their_liqprice.len()/2;  
       let mut upper_bound = all_trove_owner_and_their_liqprice.len();     //we set an upper and lower bound, and compare current price to the middle point in vec for fast indexing.
       let mut lower_bound = 0;
       let mut liq_index = 0;

       loop {
           println!("checking index {}", i);
           if i == 0 {
               liq_index = 0;
               break;
           } else if i >= all_trove_owner_and_their_liqprice.len() - 1 {
               liq_index = all_trove_owner_and_their_liqprice.len() - 1;
               break;
           } else if lower_bound == upper_bound -1 {
               liq_index = upper_bound;
               break;
           } else {
               if all_trove_owner_and_their_liqprice[i].1 > current_eth_price {
                   println!("index {} price {} is bigger than eth price {}", i, all_trove_owner_and_their_liqprice[i].1, current_eth_price );
                   liq_index = i;
                   upper_bound = i;
                  
                   if i - lower_bound < 2  {
                       i = i - 1;
                   } else {
                       i = (i - lower_bound) / 2;
                   }
                   
                   println!("lowing i to {}", i);
               } else {
                   println!("index {} price {} is less than eth price {}", i, all_trove_owner_and_their_liqprice[i].1, current_eth_price );
                  
                   lower_bound = i;
                   
                   if upper_bound - i < 2  {
                       i = i + 1;
                   } else {
                       i = (upper_bound - i )/2 + i;
                   }
                  
                   println!("increasing i to {}", i);
               }
           }
       }

       println!("found liquidation index from {} {:?} to {} {:?}",liq_index, all_trove_owner_and_their_liqprice[liq_index], all_trove_owner_and_their_liqprice.len() - 1, all_trove_owner_and_their_liqprice[all_trove_owner_and_their_liqprice.len() - 1]);

       liq_index
}