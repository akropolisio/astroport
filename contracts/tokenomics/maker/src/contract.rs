use crate::error::ContractError;
use crate::state::{Config, CONFIG, OWNERSHIP_PROPOSAL};
use astroport::asset::{addr_validate_to_lower, Asset, AssetInfo, PairInfo};
use astroport::common::{claim_ownership, drop_ownership_proposal, propose_new_owner};
use astroport::factory::UpdateAddr;
use astroport::maker::{BalancesResponse, ConfigResponse, ExecuteMsg, InstantiateMsg, QueryMsg};
use astroport::pair::{Cw20HookMsg, QueryMsg as PairQueryMsg};
use astroport::querier::query_pair_info;
use cosmwasm_std::{
    attr, entry_point, to_binary, Addr, Attribute, Binary, Coin, Decimal, Deps, DepsMut, Env,
    MessageInfo, QueryRequest, Reply, ReplyOn, Response, StdError, StdResult, SubMsg, Uint128,
    Uint64, WasmMsg, WasmQuery,
};
use cw2::set_contract_version;
use std::collections::HashMap;

// version info for migration info
const CONTRACT_NAME: &str = "astroport-maker";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const DEFAULT_MAX_SPREAD: u64 = 5; // 5%

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let governance_contract = if let Some(governance_contract) = msg.governance_contract {
        Option::from(addr_validate_to_lower(deps.api, &governance_contract)?)
    } else {
        None
    };

    let governance_percent = if let Some(governance_percent) = msg.governance_percent {
        if governance_percent > Uint64::new(100) {
            return Err(ContractError::IncorrectGovernancePercent {});
        };
        governance_percent
    } else {
        Uint64::zero()
    };

    let max_spread = if let Some(max_spread) = msg.max_spread {
        if max_spread.gt(&Decimal::one()) {
            return Err(ContractError::IncorrectMaxSpread {});
        };

        max_spread
    } else {
        Decimal::percent(DEFAULT_MAX_SPREAD)
    };

    let cfg = Config {
        owner: addr_validate_to_lower(deps.api, &msg.owner)?,
        astro_token_contract: addr_validate_to_lower(deps.api, &msg.astro_token_contract)?,
        factory_contract: addr_validate_to_lower(deps.api, &msg.factory_contract)?,
        staking_contract: addr_validate_to_lower(deps.api, &msg.staking_contract)?,
        governance_contract,
        governance_percent,
        max_spread,
    };

    CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Collect { pair_addresses } => collect(deps, env, pair_addresses),
        ExecuteMsg::UpdateConfig {
            factory_contract,
            staking_contract,
            governance_contract,
            governance_percent,
            max_spread,
        } => update_config(
            deps,
            info,
            factory_contract,
            staking_contract,
            governance_contract,
            governance_percent,
            max_spread,
        ),
        ExecuteMsg::ProposeNewOwner { owner, expires_in } => {
            let config: Config = CONFIG.load(deps.storage)?;

            propose_new_owner(
                deps,
                info,
                env,
                owner,
                expires_in,
                config.owner,
                OWNERSHIP_PROPOSAL,
            )
            .map_err(|e| e.into())
        }
        ExecuteMsg::DropOwnershipProposal {} => {
            let config: Config = CONFIG.load(deps.storage)?;

            drop_ownership_proposal(deps, info, config.owner, OWNERSHIP_PROPOSAL)
                .map_err(|e| e.into())
        }
        ExecuteMsg::ClaimOwnership {} => {
            claim_ownership(deps, info, env, OWNERSHIP_PROPOSAL, |deps, new_owner| {
                CONFIG.update::<_, StdError>(deps.storage, |mut v| {
                    v.owner = new_owner;
                    Ok(v)
                })?;

                Ok(())
            })
            .map_err(|e| e.into())
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, _msg: Reply) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    let astro = AssetInfo::Token {
        contract_addr: cfg.astro_token_contract.clone(),
    };

    let mut resp = Response::new();

    let balance = astro.query_pool(&deps.querier, env.contract.address)?;
    if !balance.is_zero() {
        resp.messages
            .append(&mut distribute_astro(deps.as_ref(), &cfg, balance)?);
    }

    Ok(resp)
}

fn collect(deps: DepsMut, env: Env, pair_addresses: Vec<Addr>) -> Result<Response, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    let astro = AssetInfo::Token {
        contract_addr: cfg.astro_token_contract.clone(),
    };

    let mut response = Response::default();

    // Collect assets
    let mut assets_map: HashMap<String, AssetInfo> = HashMap::new();
    for pair in pair_addresses {
        let pair = query_pair(deps.as_ref(), pair)?;
        assets_map.insert(pair[0].to_string(), pair[0].clone());
        assets_map.insert(pair[1].to_string(), pair[1].clone());
    }

    // Swap all non-astro tokens
    for a in assets_map.values().cloned().filter(|a| a.ne(&astro)) {
        // Get Balance
        let balance = a.query_pool(&deps.querier, env.contract.address.clone())?;
        if !balance.is_zero() {
            // Swap to astro and transfer to staking and governance
            response
                .messages
                .push(swap_to_astro(deps.as_ref(), &cfg, a, balance)?);
        }
    }

    // Use ReplyOn to have a proper amount of astro
    if !response.messages.is_empty() {
        if let Some(last) = response.messages.last_mut() {
            last.reply_on = ReplyOn::Success;
        } else {
            return Err(ContractError::SwapNonAstroToAstroError {});
        }
    } else {
        let balance = astro.query_pool(&deps.querier, env.contract.address)?;
        if !balance.is_zero() {
            response
                .messages
                .append(&mut distribute_astro(deps.as_ref(), &cfg, balance)?);
        }
    }

    Ok(response)
}

fn distribute_astro(
    deps: Deps,
    cfg: &Config,
    amount: Uint128,
) -> Result<Vec<SubMsg>, ContractError> {
    let mut result = vec![];

    let info = AssetInfo::Token {
        contract_addr: cfg.astro_token_contract.clone(),
    };

    let governance_amount = if let Some(governance_contract) = cfg.governance_contract.clone() {
        let amount =
            amount.multiply_ratio(Uint128::from(cfg.governance_percent), Uint128::new(100));
        let to_governance_asset = Asset {
            info: info.clone(),
            amount,
        };
        result.push(SubMsg::new(
            to_governance_asset.into_msg(&deps.querier, governance_contract)?,
        ));
        amount
    } else {
        Uint128::zero()
    };
    let staking_amount = amount - governance_amount;
    let to_staking_asset = Asset {
        info,
        amount: staking_amount,
    };
    result.push(SubMsg::new(
        to_staking_asset.into_msg(&deps.querier, cfg.staking_contract.clone())?,
    ));
    Ok(result)
}

fn swap_to_astro(
    deps: Deps,
    cfg: &Config,
    from_token: AssetInfo,
    amount_in: Uint128,
) -> Result<SubMsg, ContractError> {
    let to_token = AssetInfo::Token {
        contract_addr: cfg.astro_token_contract.clone(),
    };

    let pair: PairInfo = query_pair_info(
        &deps.querier,
        cfg.factory_contract.clone(),
        &[from_token.clone(), to_token.clone()],
    )
    .map_err(|_| ContractError::PairNotFound(from_token.clone(), to_token.clone()))?;

    if from_token.is_native_token() {
        let mut offer_asset = Asset {
            info: from_token.clone(),
            amount: amount_in,
        };

        // deduct tax first
        let amount_in = amount_in.checked_sub(offer_asset.compute_tax(&deps.querier)?)?;

        offer_asset.amount = amount_in;

        Ok(SubMsg::new(WasmMsg::Execute {
            contract_addr: pair.contract_addr.to_string(),
            msg: to_binary(&astroport::pair::ExecuteMsg::Swap {
                offer_asset,
                belief_price: None,
                max_spread: Some(cfg.max_spread),
                to: None,
            })?,
            funds: vec![Coin {
                denom: from_token.to_string(),
                amount: amount_in,
            }],
        }))
    } else {
        Ok(SubMsg::new(WasmMsg::Execute {
            contract_addr: from_token.to_string(),
            msg: to_binary(&cw20::Cw20ExecuteMsg::Send {
                contract: pair.contract_addr.to_string(),
                amount: amount_in,
                msg: to_binary(&Cw20HookMsg::Swap {
                    belief_price: None,
                    max_spread: Some(cfg.max_spread),
                    to: None,
                })?,
            })?,
            funds: vec![],
        }))
    }
}

fn update_config(
    deps: DepsMut,
    info: MessageInfo,
    factory_contract: Option<String>,
    staking_contract: Option<String>,
    governance_contract: Option<UpdateAddr>,
    governance_percent: Option<Uint64>,
    max_spread: Option<Decimal>,
) -> Result<Response, ContractError> {
    let mut attributes = vec![attr("action", "set_config")];

    let mut config = CONFIG.load(deps.storage)?;

    // permission check
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if let Some(factory_contract) = factory_contract {
        config.factory_contract = addr_validate_to_lower(deps.api, &factory_contract)?;
        attributes.push(Attribute::new("factory_contract", &factory_contract));
    };

    if let Some(staking_contract) = staking_contract {
        config.staking_contract = addr_validate_to_lower(deps.api, &staking_contract)?;
        attributes.push(Attribute::new("staking_contract", &staking_contract));
    };

    if let Some(action) = governance_contract {
        match action {
            UpdateAddr::Set(gov) => {
                config.governance_contract = Option::from(addr_validate_to_lower(deps.api, &gov)?);
                attributes.push(Attribute::new("governance_contract", &gov));
            }
            UpdateAddr::Remove {} => {
                config.governance_contract = None;
            }
        }
    }

    if let Some(governance_percent) = governance_percent {
        if governance_percent > Uint64::new(100) {
            return Err(ContractError::IncorrectGovernancePercent {});
        };

        config.governance_percent = governance_percent;
        attributes.push(Attribute::new("governance_percent", governance_percent));
    };

    if let Some(max_spread) = max_spread {
        if max_spread.gt(&Decimal::one()) {
            return Err(ContractError::IncorrectMaxSpread {});
        };

        config.max_spread = max_spread;
        attributes.push(Attribute::new("max_spread", max_spread.to_string()));
    };

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(attributes))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_get_config(deps)?),
        QueryMsg::Balances { assets } => to_binary(&query_get_balances(deps, env, assets)?),
    }
}

fn query_get_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        owner: config.owner,
        factory_contract: config.factory_contract,
        staking_contract: config.staking_contract,
        governance_contract: config.governance_contract,
        governance_percent: config.governance_percent,
        astro_token_contract: config.astro_token_contract,
        max_spread: config.max_spread,
    })
}

fn query_get_balances(deps: Deps, env: Env, assets: Vec<AssetInfo>) -> StdResult<BalancesResponse> {
    let mut resp = BalancesResponse { balances: vec![] };

    for a in assets {
        // Get Balance
        let balance = a.query_pool(&deps.querier, env.contract.address.clone())?;
        if !balance.is_zero() {
            resp.balances.push(Asset {
                info: a,
                amount: balance,
            })
        }
    }

    Ok(resp)
}

pub fn query_pair(deps: Deps, contract_addr: Addr) -> StdResult<[AssetInfo; 2]> {
    let res: PairInfo = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: String::from(contract_addr),
        msg: to_binary(&PairQueryMsg::Pair {})?,
    }))?;

    Ok(res.asset_infos)
}
