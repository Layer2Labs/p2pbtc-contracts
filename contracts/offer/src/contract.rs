use cosmwasm_std::{
    entry_point, to_binary, Addr, Binary, ContractResult, CosmosMsg, Deps, DepsMut, Env,
    MessageInfo, Order, QueryRequest, Reply, ReplyOn, Response, StdResult, Storage, SubMsg,
    SubMsgExecutionResponse, WasmMsg, WasmQuery,
};
use cw_storage_plus::Bound;

use localterra_protocol::factory_util::get_factory_config;
use localterra_protocol::guards::{assert_min_g_max, assert_ownership};
use localterra_protocol::offer::{
    offers, Config, ExecuteMsg, InstantiateMsg, Offer, OfferModel, OfferMsg, OfferState, QueryMsg,
    State, TradeAddr, TradeInfo, TradesIndex,
};
use localterra_protocol::trade::{
    InstantiateMsg as TradeInstantiateMsg, QueryMsg as TradeQueryMsg, TradeData, TradeState,
};

use crate::state::{config_read, config_storage, state_read, state_storage, trades};
use localterra_protocol::errors::OfferError;

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    _msg: InstantiateMsg,
) -> Result<Response, OfferError> {
    config_storage(deps.storage).save(&Config {
        factory_addr: info.sender,
    })?;
    state_storage(deps.storage).save(&State { offers_count: 0 })?;
    Ok(Response::default())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, OfferError> {
    match msg {
        ExecuteMsg::Create { offer } => create_offer(deps, env, info, offer),
        ExecuteMsg::Activate { id } => activate_offer(deps, env, info, id),
        ExecuteMsg::Pause { id } => pause_offer(deps, env, info, id),
        ExecuteMsg::Update { id, offer } => update_offer(deps, env, info, id, offer),
        ExecuteMsg::NewTrade {
            offer_id,
            ust_amount,
            taker,
            taker_contact,
            arbitrator,
        } => create_trade(
            deps,
            env,
            info,
            offer_id,
            ust_amount,
            taker,
            taker_contact,
            arbitrator,
        ),
    }
}

#[entry_point]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::State {} => to_binary(&query_state(deps)?),
        QueryMsg::Offers { fiat_currency } => {
            to_binary(&OfferModel::query_all_offers(deps.storage, fiat_currency)?)
        }
        QueryMsg::OffersQuery {
            owner,
            last_value,
            limit,
        } => to_binary(&OfferModel::query(deps, owner, last_value, limit)?),
        QueryMsg::OffersByType {
            offer_type,
            last_value,
            limit,
        } => to_binary(&OfferModel::query_by_type(
            deps, offer_type, last_value, limit,
        )?),
        QueryMsg::OffersByFiat {
            fiat_currency,
            last_value,
            limit,
        } => to_binary(&OfferModel::query_by_fiat(
            deps,
            fiat_currency,
            last_value,
            limit,
        )?),
        QueryMsg::OffersByTypeFiat {
            offer_type,
            fiat_currency,
            last_value,
            limit,
        } => to_binary(&OfferModel::query_by_type_fiat(
            deps,
            offer_type,
            fiat_currency,
            last_value,
            limit,
        )?),
        QueryMsg::Offer { id } => to_binary(&load_offer_by_id(deps.storage, id)?),
        QueryMsg::TradesQuery {
            user,
            state,
            index,
            last_value,
            limit,
        } => to_binary(&query_trades(
            env,
            deps,
            deps.api.addr_validate(user.as_str()).unwrap(),
            state,
            index,
            last_value,
            limit,
        )?),
    }
}

#[entry_point]
pub fn reply(deps: DepsMut, env: Env, msg: Reply) -> Result<Response, OfferError> {
    match msg.id {
        0 => trade_instance_reply(deps, env, msg.result),
        _ => Err(OfferError::InvalidReply {}),
    }
}

fn trade_instance_reply(
    deps: DepsMut,
    _env: Env,
    result: ContractResult<SubMsgExecutionResponse>,
) -> Result<Response, OfferError> {
    if result.is_err() {
        return Err(OfferError::InvalidReply {});
    }

    let trade_addr: Addr = result
        .unwrap()
        .events
        .into_iter()
        .find(|e| e.ty == "instantiate_contract")
        .and_then(|ev| {
            ev.attributes
                .into_iter()
                .find(|attr| attr.key == "contract_address")
                .map(|addr| addr.value)
        })
        .and_then(|addr| deps.api.addr_validate(addr.as_str()).ok())
        .unwrap();

    let trade: TradeData = deps
        .querier
        .query_wasm_smart(trade_addr.to_string(), &TradeQueryMsg::State {})
        .unwrap();

    trades()
        .save(
            deps.storage,
            trade.addr.as_str(),
            &TradeAddr {
                trade: trade_addr.clone(),
                seller: trade.seller.clone(),
                buyer: trade.buyer.clone(),
                arbitrator: trade.arbitrator.clone(),
                state: trade.state.clone(),
            },
        )
        .unwrap();

    let offer = load_offer_by_id(deps.storage, trade.offer_id.clone()).unwrap();

    //trade_state, offer_id, trade_amount,owner
    let res = Response::new()
        .add_attribute("action", "create_trade_reply")
        .add_attribute("addr", trade_addr)
        .add_attribute("offer_id", offer.id.to_string())
        .add_attribute("amount", trade.ust_amount)
        .add_attribute("owner", offer.owner);
    Ok(res)
}

pub fn create_offer(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: OfferMsg,
) -> Result<Response, OfferError> {
    assert_min_g_max(msg.min_amount, msg.max_amount)?;

    let mut state = state_storage(deps.storage).load()?;

    let offer_id = state.offers_count + 1;

    state.offers_count = offer_id;

    let offer = OfferModel::create(
        deps.storage,
        Offer {
            id: offer_id,
            owner: info.sender.clone(),
            maker_contact: msg.maker_contact,
            offer_type: msg.offer_type,
            fiat_currency: msg.fiat_currency.clone(),
            min_amount: msg.min_amount,
            max_amount: msg.max_amount,
            state: OfferState::Active,
            timestamp: env.block.time.seconds(),
        },
    )
    .offer;

    state_storage(deps.storage).save(&state)?;

    let res = Response::new()
        .add_attribute("action", "create_offer")
        .add_attribute("type", offer.offer_type.to_string())
        .add_attribute("id", offer.id.to_string())
        .add_attribute("min_amount", offer.min_amount.to_string())
        .add_attribute("max_amount", offer.max_amount.to_string())
        .add_attribute("owner", offer.owner);

    Ok(res)
}

pub fn activate_offer(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    id: u64,
) -> Result<Response, OfferError> {
    let mut offer_model = OfferModel::may_load(deps.storage, &id);

    assert_ownership(info.sender, offer_model.offer.owner.clone())?;

    let offer = offer_model.activate()?;

    let res = Response::new()
        .add_attribute("action", "activate_offer")
        .add_attribute("id", offer.id.to_string())
        .add_attribute("owner", offer.owner.to_string());

    Ok(res)
}

pub fn pause_offer(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    id: u64,
) -> Result<Response, OfferError> {
    let mut offer_model = OfferModel::may_load(deps.storage, &id);

    assert_ownership(info.sender, offer_model.offer.owner.clone())?;

    let offer = offer_model.pause()?;

    let res = Response::new()
        .add_attribute("action", "pause_offer")
        .add_attribute("id", offer.id.to_string())
        .add_attribute("owner", offer.owner.to_string());

    Ok(res)
}

pub fn update_offer(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    id: u64,
    msg: OfferMsg,
) -> Result<Response, OfferError> {
    assert_min_g_max(msg.min_amount, msg.max_amount)?;

    let mut offer_model = OfferModel::may_load(deps.storage, &id);

    assert_ownership(info.sender, offer_model.offer.owner.clone())?;

    let offer = offer_model.update(msg);

    let res = Response::new()
        .add_attribute("action", "pause_offer")
        .add_attribute("id", offer.id.to_string())
        .add_attribute("owner", offer.owner.to_string());

    Ok(res)
}

fn create_trade(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    offer_id: u64,
    ust_amount: String,
    taker: String,
    taker_contact: String,
    arbitrator: String,
) -> Result<Response, OfferError> {
    let cfg = config_read(deps.storage).load().unwrap();
    // let offer = load_offer_by_id(deps.storage, offer_id).unwrap();
    let offer = OfferModel::from_store(deps.storage, &offer_id);
    //     .ok_or(OfferError::InvalidReply {})?; // TODO choose better error

    let factory_cfg = get_factory_config(&deps.querier, cfg.factory_addr.to_string());

    let instantiate_msg = WasmMsg::Instantiate {
        admin: None,
        code_id: factory_cfg.trade_code_id,
        msg: to_binary(&TradeInstantiateMsg {
            offer_id,
            ust_amount: ust_amount.clone(),
            taker: taker.clone(),
            taker_contact,
            arbitrator,
            offers_addr: env.contract.address.to_string(),
            timestamp: env.block.time.seconds(),
        })
        .unwrap(),
        funds: info.funds,
        label: "new-trade".to_string(),
    };
    let sub_message = SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(instantiate_msg),
        gas_limit: None,
        reply_on: ReplyOn::Success,
    };

    let res = Response::new()
        .add_submessage(sub_message)
        .add_attribute("action", "create_trade")
        .add_attribute("id", offer.id.to_string())
        .add_attribute("owner", offer.owner.to_string())
        .add_attribute("ust_amount", ust_amount)
        .add_attribute("taker", taker);
    Ok(res)
}

fn query_config(deps: Deps) -> StdResult<Config> {
    let cfg = config_read(deps.storage).load().unwrap();
    Ok(cfg)
}

fn query_state(deps: Deps) -> StdResult<State> {
    let state = state_read(deps.storage).load().unwrap();
    Ok(state)
}

pub fn load_offer_by_id(storage: &dyn Storage, id: u64) -> StdResult<Offer> {
    let offer = offers()
        .may_load(storage, &id.to_string())
        .unwrap_or_default()
        .unwrap();
    Ok(offer)
}

pub fn query_trades(
    env: Env,
    deps: Deps,
    user: Addr,
    state: Option<TradeState>,
    index: TradesIndex,
    last_value: Option<Addr>,
    limit: u32,
) -> StdResult<Vec<TradeInfo>> {
    let curr_height = env.block.height;

    let mut trades_infos: Vec<TradeInfo> = vec![];

    // Pagination range (TODO pagination doesn't work with Addr as pk)
    let range_from = match last_value {
        Some(addr) => {
            let valid_addr = deps.api.addr_validate(addr.as_str()).unwrap();
            Some(Bound::Exclusive(Vec::from(valid_addr.to_string())))
        }
        None => None,
    };

    // Select correct index for data lookup
    // * The `state<TradeState>` filter only supported for `user == arbitrator` queries
    let prefix = match index {
        TradesIndex::Seller => trades().idx.sender.prefix(user),
        TradesIndex::Buyer => trades().idx.recipient.prefix(user),
        TradesIndex::ArbitratorState => match state {
            Some(state) => trades()
                .idx
                .arbitrator_state
                .prefix((user, state.to_string())),
            None => trades().idx.arbitrator_state.sub_prefix(user),
        },
    };

    let trade_results: Vec<TradeAddr> = prefix
        .range(deps.storage, range_from, None, Order::Ascending)
        .flat_map(|item| item.and_then(|(_, offer)| Ok(offer)))
        .take(limit as usize)
        .collect();

    trade_results.iter().for_each(|t| {
        let trade_state: TradeData = deps
            .querier
            .query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: t.trade.to_string(),
                msg: to_binary(&TradeQueryMsg::State {}).unwrap(),
            }))
            .unwrap();
        let offer: Offer = deps
            .querier
            .query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: trade_state.offer_contract.to_string(),
                msg: to_binary(&QueryMsg::Offer {
                    id: trade_state.offer_id,
                })
                .unwrap(),
            }))
            .unwrap();

        let expired = curr_height >= trade_state.expire_height;
        trades_infos.push(TradeInfo {
            trade: trade_state,
            offer,
            expired,
        })
    });
    Ok(trades_infos)
}
