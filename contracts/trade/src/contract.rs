use std::ops::Add;
use std::str::FromStr;

use cosmwasm_std::{
    entry_point, to_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env,
    MessageInfo, QueryRequest, Response, StdResult, SubMsg, Uint128, WasmMsg, WasmQuery,
};

use localterra_protocol::factory::{Config as FactoryConfig, QueryMsg as FactoryQuery};
use localterra_protocol::factory_util::get_factory_config;
use localterra_protocol::offer::{
    Config as OfferConfig, Offer, OfferType, QueryMsg as OfferQueryMsg,
};
use localterra_protocol::trade::{ExecuteMsg, InstantiateMsg, QueryMsg, State, TradeState};
use localterra_protocol::trading_incentives::ExecuteMsg as TradingIncentivesMsg;

use crate::errors::TradeError;
use crate::state::{state as state_storage, state_read};
use crate::taxation::deduct_tax;

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, TradeError> {
    //Load Offer
    let offer_contract = deps.api.addr_validate(msg.offers_addr.as_str()).unwrap();
    let offer_id = msg.offer_id;
    let load_offer_result: StdResult<Offer> =
        deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: offer_contract.clone().into_string(),
            msg: to_binary(&OfferQueryMsg::Offer { id: offer_id }).unwrap(),
        }));
    if load_offer_result.is_err() {
        return Err(TradeError::OfferNotFound { offer_id });
    }
    let offer = load_offer_result.unwrap();

    //Load Offer Contract Config
    let load_offer_config_result: StdResult<OfferConfig> =
        deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: offer_contract.clone().into_string(),
            msg: to_binary(&OfferQueryMsg::Config {}).unwrap(),
        }));
    let offer_cfg = load_offer_config_result.unwrap();

    //TODO: it's probably a good idea to store this kind of configuration in a Gov contract.
    let expire_height = env.block.height + 600; //Roughly 1h.

    //Check that ust_amount is inside Offer limits
    let amount = Uint128::new(u128::from_str(msg.ust_amount.as_str()).unwrap());
    if amount > offer.max_amount || amount < offer.min_amount {
        return Err(TradeError::AmountError {
            amount,
            min_amount: offer.min_amount,
            max_amount: offer.max_amount,
        });
    }

    //Instantiate recipient and sender addresses according to Offer type (buy, sell)
    let recipient: Addr;
    let sender: Addr;
    let counterparty = deps.api.addr_validate(msg.counterparty.as_str()).unwrap();
    let _taker_is_buying: bool;

    if offer.offer_type == OfferType::Buy {
        _taker_is_buying = false;
        recipient = offer.owner;
        sender = counterparty.clone();
    } else {
        _taker_is_buying = true;
        recipient = counterparty.clone();
        sender = offer.owner;
    }

    //Instantiate Trade state
    let mut state = State {
        factory_addr: offer_cfg.factory_addr.clone(),
        recipient,
        sender,
        offer_contract: offer_contract.clone(),
        offer_id,
        state: TradeState::Created,
        expire_height,
        ust_amount: amount,
    };

    //Set state to EscrowFunded if enough UST was sent in the message.
    if !info.funds.is_empty() {
        //TODO: Check for Luna or other Terra native tokens.
        let ust_amount = get_ust_amount(info.clone());
        if ust_amount >= amount {
            state.state = TradeState::EscrowFunded
        }
    }

    //Save state.
    let save_state_result = state_storage(deps.storage).save(&state);
    if save_state_result.is_err() {
        return Err(TradeError::InstantiationError {
            message: "Couldn't save state.".to_string(),
        });
    }

    Ok(Response::default())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, TradeError> {
    let state = state_storage(deps.storage).load().unwrap();
    match msg {
        ExecuteMsg::FundEscrow {} => try_fund_escrow(deps, env, info, state),
        ExecuteMsg::Refund {} => try_refund(deps, env, state),
        ExecuteMsg::Release {} => try_release(deps, env, info, state),
    }
}

#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::State {} => to_binary(&query_config(deps)?),
    }
}

fn query_config(deps: Deps) -> StdResult<State> {
    let state = state_read(deps.storage).load()?;
    Ok(state)
}

fn try_fund_escrow(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    mut state: State,
) -> Result<Response, TradeError> {
    let ust_amount = if !info.funds.is_empty() {
        get_ust_amount(info.clone())
    } else {
        let ust_balance = deps
            .querier
            .query_balance(env.contract.address, "uusd".to_string());
        ust_balance
            .unwrap_or(Coin {
                denom: "uusd".to_string(),
                amount: Uint128::zero(),
            })
            .amount
    };
    if ust_amount >= state.ust_amount {
        state.state = TradeState::EscrowFunded;
    } else {
        return Err(TradeError::ExecutionError {
            message: "UST amount is less than required to fund the escrow.".to_string(),
        });
    }
    state_storage(deps.storage).save(&state).unwrap();
    let res = Response::new()
        .add_attribute("action", "fund_escrow")
        .add_attribute("ust_amount", ust_amount.to_string())
        .add_attribute("sender", info.sender);

    Ok(res)
}

fn get_offer(deps: &Deps, state: &State) -> Offer {
    deps.querier
        .query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: state.offer_contract.to_string(),
            msg: to_binary(&OfferQueryMsg::Offer {
                id: state.offer_id.clone(),
            })
            .unwrap(),
        }))
        .unwrap()
}

fn try_release(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    state: State,
) -> Result<Response, TradeError> {
    //Check if sender can release
    if info.sender != state.sender {
        return Err(TradeError::Unauthorized {
            owner: state.sender,
            caller: info.sender,
        });
    }

    // throws error if state is expired
    if env.block.height > state.expire_height {
        return Err(TradeError::Expired {
            expire_height: state.expire_height,
            current_height: env.block.height,
        });
    }

    //Load and check balance
    let balance_result = deps.querier.query_all_balances(&env.contract.address);
    if balance_result.is_err() {
        return Err(TradeError::ReleaseError {
            message: "Contract has no funds.".to_string(),
        });
    }

    //Update trade State to TradeState::Closed
    let mut state: State = state_storage(deps.storage).load().unwrap();
    state.state = TradeState::Closed;
    state_storage(deps.storage).save(&state).unwrap();

    //Calculate fees and final release amount
    let mut send_msgs: Vec<SubMsg> = Vec::new();
    let balance = balance_result.unwrap();

    let mut final_balance: Vec<Coin> = Vec::new();
    let offer = get_offer(&deps.as_ref(), &state);

    let factory_cfg: FactoryConfig = deps
        .querier
        .query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: state.factory_addr.to_string(),
            msg: to_binary(&FactoryQuery::Config {}).unwrap(),
        }))
        .unwrap();

    if offer.offer_type.eq(&OfferType::Buy) {
        let local_terra_fee: Vec<Coin> = deduct_localterra_fee(&balance, &mut final_balance);
        let fee_collector = factory_cfg.fee_collector_addr.clone();
        send_msgs.push(SubMsg::new(create_send_msg(
            &deps,
            fee_collector,
            local_terra_fee,
        )));
    }

    //Send Coins
    send_msgs.push(SubMsg::new(create_send_msg(
        &deps,
        state.recipient.clone(),
        final_balance.clone(),
    )));

    //Create Trade Registration message to be sent to the Trading Incentives contract.
    let register_trade_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: factory_cfg.trading_incentives_addr.to_string(),
        msg: to_binary(&TradingIncentivesMsg::RegisterTrade {
            trade: env.contract.address.to_string(),
            maker: offer.owner.to_string(),
        })
        .unwrap(),
        funds: vec![],
    }));
    send_msgs.push(register_trade_msg);

    let res = Response::new().add_submessages(send_msgs);
    Ok(res)
}

fn try_refund(deps: DepsMut, env: Env, state: State) -> Result<Response, TradeError> {
    // anyone can try to refund, as long as the contract is expired
    if state.expire_height > env.block.height {
        return Err(TradeError::RefundError {
            message: "Only expired trades can be refunded.".to_string(),
        });
    }

    let balance_result = deps.querier.query_all_balances(&env.contract.address);
    return if balance_result.is_ok() {
        let balance = balance_result.unwrap();
        let send_msg = create_send_msg(&deps, state.sender, balance);
        let res = Response::new().add_submessage(SubMsg::new(send_msg));
        Ok(res)
    } else {
        Err(TradeError::RefundError {
            message: "Contract has no funds.".to_string(),
        })
    };
}

fn get_ust_amount(info: MessageInfo) -> Uint128 {
    let ust = &info.funds.iter().find(|c| c.denom.eq("uusd"));
    return match ust {
        None => Uint128::zero(),
        Some(c) => c.amount,
    };
}

fn deduct_localterra_fee(balance: &Vec<Coin>, final_balance: &mut Vec<Coin>) -> Vec<Coin> {
    let mut fees: Vec<Coin> = Vec::new();
    balance.iter().for_each(|coin| {
        let mut fee_amount = coin.amount.checked_div(Uint128::new(1000u128)).unwrap();
        let fee = Coin::new(fee_amount.u128(), coin.denom.to_string());
        fees.push(fee);
        final_balance.push(Coin::new(
            (coin.amount - fee_amount).u128(),
            coin.denom.to_string(),
        ));
    });
    fees
}

fn deduct_local_terra_fee(balance: Vec<Coin>, local_terra_fee: Vec<Coin>) -> StdResult<Vec<Coin>> {
    Ok([Coin {
        amount: (balance[0].amount.checked_sub(local_terra_fee[0].amount)).unwrap(),
        denom: balance[0].clone().denom,
    }]
    .to_vec())
}

fn create_send_msg(deps: &DepsMut, to_address: Addr, coins: Vec<Coin>) -> CosmosMsg {
    let mut coins_without_tax: Vec<Coin> = Vec::new();
    coins
        .iter()
        .for_each(|c| coins_without_tax.push(deduct_tax(&deps.querier, c.clone()).unwrap()));
    CosmosMsg::Bank(BankMsg::Send {
        to_address: to_address.to_string(),
        amount: coins_without_tax,
    })
}
