use anchor_lang::prelude::Pubkey;

use fixed::types::I80F48;

use std::{cell::Ref, cmp};

use zo_abi::{
    Cache, CollateralInfo, Control, FractionType, Margin, MarkCache,
    OpenOrdersInfo, PerpMarketInfo, State, WrappedI80F48, MAX_COLLATERALS,
    MAX_MARKETS, SPOT_INITIAL_MARGIN_REQ, SPOT_MAINT_MARGIN_REQ,
};

use crate::liquidator::{error::ErrorCode, math::*, utils::*};

struct PerpAccParams {
    total_acc_value: i64,
    has_open_pos_notional: bool,
    total_realized_pnl: i64,
    pimf_vec: Vec<u16>,
    pmmf_vec: Vec<u16>,
    pcmf_vec: Vec<u16>,
    pos_open_notional_vec: Vec<i64>,
    pos_notional_vec: Vec<i64>,
}

#[derive(Clone, Copy)]
enum MfReturnOption {
    Imf,
    Mmf,
    Cancel,
    Both,
}

pub fn check_fraction_requirement(
    fraction_type: FractionType,
    col: i64, // weighted collateral adjusted for bnl fees
    max_markets: usize,
    max_cols: usize,
    oo_agg: &[OpenOrdersInfo; MAX_MARKETS as usize],
    pm: &[PerpMarketInfo; MAX_MARKETS as usize],
    col_info_arr: &[CollateralInfo; MAX_COLLATERALS as usize],
    margin_col: &[WrappedI80F48; MAX_COLLATERALS as usize],
    cache: &Ref<Cache>,
) -> Result<bool, ErrorCode> {
    let return_option = match fraction_type {
        FractionType::Initial => MfReturnOption::Imf,
        FractionType::Maintenance => MfReturnOption::Mmf,
        FractionType::Cancel => MfReturnOption::Cancel,
    };
    let PerpAccParams {
        total_acc_value,
        mut has_open_pos_notional,
        total_realized_pnl,
        mut pimf_vec,
        mut pmmf_vec,
        mut pcmf_vec,
        mut pos_open_notional_vec,
        mut pos_notional_vec,
    } = get_perp_acc_params(
        col,
        return_option,
        max_markets,
        oo_agg,
        &cache.marks,
        pm,
        &{ cache.funding_cache },
    )?;

    let (
        has_spot_pos_notional,
        mut spot_imf_vec,
        mut spot_mmf_vec,
        mut spot_pos_notional_vec,
    ) = get_spot_borrows(
        return_option,
        max_cols,
        margin_col,
        col_info_arr,
        cache,
        total_realized_pnl,
    )?;

    if has_spot_pos_notional {
        has_open_pos_notional = true;
    }

    pos_open_notional_vec.extend(spot_pos_notional_vec.iter().clone());
    pos_notional_vec.append(&mut spot_pos_notional_vec);

    match fraction_type {
        FractionType::Initial => {
            if has_open_pos_notional {
                pimf_vec.append(&mut spot_imf_vec);
                let omf = total_acc_value
                    .min(col + total_realized_pnl)
                    .safe_mul(1000i64)?;
                let imf =
                    calc_weighted_sum(pimf_vec, pos_open_notional_vec).unwrap();
                Ok(omf > imf)
            } else {
                Ok(true)
            }
        }
        FractionType::Maintenance => {
            if has_open_pos_notional {
                pmmf_vec.append(&mut spot_mmf_vec);
                let mf = total_acc_value.safe_mul(1000i64)?;
                let mmf =
                    calc_weighted_sum(pmmf_vec, pos_notional_vec).unwrap();
                Ok(mf > mmf)
            } else {
                Ok(true)
            }
        }
        FractionType::Cancel => {
            if has_open_pos_notional {
                pcmf_vec.append(&mut spot_imf_vec);
                let omf = total_acc_value
                    .min(col + total_realized_pnl)
                    .safe_mul(1000)?;

                let cmf =
                    calc_weighted_sum(pcmf_vec, pos_open_notional_vec).unwrap();

                Ok(omf > cmf)
            } else {
                Ok(true)
            }
        }
    }
}

fn get_perp_acc_params(
    col: i64,
    return_option: MfReturnOption,
    max_markets: usize,
    open_orders_agg: &[OpenOrdersInfo; 50],
    marks: &[MarkCache; 50],
    perp_markets: &[PerpMarketInfo; 50],
    funding_cache: &[i128; 50],
) -> Result<PerpAccParams, ErrorCode> {
    // for omf
    let mut total_acc_value = col;
    let mut has_open_pos_notional = false;
    let mut total_realized_pnl = 0i64;

    // for imf or mmf
    let mut imf_vec = Vec::new();
    let mut mmf_vec = Vec::new();
    let mut cmf_vec = Vec::new();
    let mut pos_notional_vec = Vec::new();
    let mut pos_open_notional_vec = Vec::new();

    for (index, oo_info) in open_orders_agg.iter().enumerate() {
        if !(index < max_markets) {
            break;
        }
        if oo_info.key == Pubkey::default() {
            continue;
        }

        let mark = marks[index].price.into();

        let new_acc_val = calc_acc_val(
            total_acc_value,
            mark,
            oo_info.pos_size,
            oo_info.native_pc_total,
            oo_info.realized_pnl,
            oo_info.funding_index,
            funding_cache[index],
            perp_markets[index].asset_decimals as u32,
        )?;
        total_acc_value = new_acc_val;

        let pos_notional =
            safe_mul_i80f48(I80F48::from_num(oo_info.pos_size.abs()), mark)
                .ceil()
                .to_num::<i64>();
        let pos_open_notional = safe_mul_i80f48(
            I80F48::from_num(cmp::max(
                (oo_info.pos_size + oo_info.coin_on_bids as i64).abs(),
                (oo_info.pos_size - oo_info.coin_on_asks as i64).abs(),
            )),
            mark,
        )
        .ceil()
        .to_num::<i64>();

        if pos_open_notional.is_positive() {
            has_open_pos_notional = true;
        }

        let base_imf = perp_markets[index].base_imf;
        match return_option {
            MfReturnOption::Mmf => {
                mmf_vec.push(base_imf.safe_div(2u16)?);
            }
            MfReturnOption::Imf => {
                imf_vec.push(base_imf);
            }
            MfReturnOption::Cancel => {
                cmf_vec.push(base_imf.safe_mul(5u16)?.safe_div(8u16)?);
            }
            MfReturnOption::Both => {
                imf_vec.push(base_imf);
                mmf_vec.push(base_imf.safe_div(2u16)?);
            }
        };
        pos_open_notional_vec.push(pos_open_notional);
        pos_notional_vec.push(pos_notional);

        total_realized_pnl =
            total_realized_pnl.safe_add(oo_info.realized_pnl)?;
    }

    Ok(PerpAccParams {
        total_acc_value,
        has_open_pos_notional,
        total_realized_pnl,
        pimf_vec: imf_vec,
        pmmf_vec: mmf_vec,
        pcmf_vec: cmf_vec,
        pos_open_notional_vec,
        pos_notional_vec,
    })
}

fn get_spot_borrows(
    return_option: MfReturnOption,
    max_cols: usize,
    col_arr: &[WrappedI80F48; 25],
    col_info_arr: &[CollateralInfo; 25],
    cache: &Cache,
    total_realized_pnl: i64,
) -> Result<(bool, Vec<u16>, Vec<u16>, Vec<i64>), ErrorCode> {
    // for omf
    let mut has_open_pos_notional = false;

    // for imf or mmf
    let mut imf_vec = Vec::new();
    let mut mmf_vec = Vec::new();
    let mut pos_open_notional_vec = Vec::new();

    // loop through negative margin collateral
    for (dep_index, col_info) in col_info_arr.iter().enumerate() {
        if !(dep_index < max_cols) {
            break;
        }

        if col_arr[dep_index] >= WrappedI80F48::zero() {
            continue;
        }

        let bor_info = &cache.borrow_cache[dep_index];
        let mut dep: I80F48 = calc_actual_collateral(
            col_arr[dep_index].into(),
            bor_info.supply_multiplier.into(),
            bor_info.borrow_multiplier.into(),
        )?;
        // if collateral is USD, add the pos_realized_pnl
        if dep_index == 0 {
            dep += I80F48::from_num(total_realized_pnl);
        }

        // get oracle price
        let oracle_cache = get_oracle(&cache, &col_info.oracle_symbol).unwrap();
        let oracle_price: I80F48 = oracle_cache.price.into();

        // get position notional
        let pos_notional =
            safe_mul_i80f48(oracle_price, -dep).ceil().to_num::<i64>();

        // add it to total open pos notional
        if pos_notional.is_positive() {
            has_open_pos_notional = true;
        }

        let (imf, mmf) = match return_option {
            MfReturnOption::Imf => (
                Some(
                    (SPOT_INITIAL_MARGIN_REQ as u32 / col_info.weight as u32)
                        as u16
                        - 1000u16,
                ),
                None,
            ),
            MfReturnOption::Mmf => (
                None,
                Some(
                    (SPOT_MAINT_MARGIN_REQ as u32 / col_info.weight as u32)
                        as u16
                        - 1000u16,
                ),
            ),
            MfReturnOption::Cancel => (
                Some(
                    (SPOT_INITIAL_MARGIN_REQ as u32 / col_info.weight as u32)
                        as u16
                        - 1000u16,
                ),
                None,
            ),
            _ => (None, None),
        };

        if let Some(imf) = imf {
            imf_vec.push(imf);
        }
        if let Some(mmf) = mmf {
            mmf_vec.push(mmf);
        }
        pos_open_notional_vec.push(pos_notional);
    }

    Ok((
        has_open_pos_notional,
        imf_vec,
        mmf_vec,
        pos_open_notional_vec,
    ))
}

fn calc_weighted_sum(
    factor: Vec<u16>,
    weights: Vec<i64>,
) -> Result<i64, ErrorCode> {
    let mut numerator = 0i64;

    for (i, &factor) in factor.iter().enumerate() {
        numerator += (factor as i64).safe_mul(weights[i]).unwrap();
    }

    Ok(numerator)
}

fn calc_acc_val(
    collateral: i64,
    smol_mark_price: I80F48, // in smol usd per smol asset
    pos_size: i64,
    native_pc_total: i64,
    realized_pnl: i64,
    current_funding_index: i128,
    market_funding_index: i128,
    coin_decimals: u32,
) -> Result<i64, ErrorCode> {
    if pos_size == 0 {
        return Ok(collateral + realized_pnl);
    }

    let funding_diff = market_funding_index.safe_sub(current_funding_index)?;
    let unrealized_funding: i64 = (pos_size as i128)
        .safe_mul(-funding_diff)?
        .safe_div(10i64.pow(coin_decimals))?
        .try_into()
        .unwrap();

    let unrealized_pnl = if pos_size > 0 {
        let pos = safe_mul_i80f48(I80F48::from_num(pos_size), smol_mark_price)
            .floor()
            .to_num::<i64>();
        let bor = -native_pc_total;
        pos.safe_sub(bor)?
    } else {
        let pos = native_pc_total;
        let bor = safe_mul_i80f48(I80F48::from_num(-pos_size), smol_mark_price)
            .floor()
            .to_num::<i64>();
        pos.safe_sub(bor)?
    };

    Ok(collateral + realized_pnl + unrealized_pnl + unrealized_funding)
}

pub fn get_actual_collateral_vec(
    margin: &Margin,
    state: &Ref<State>,
    cache: &Ref<Cache>,
    is_weighted: bool,
) -> Result<Vec<I80F48>, ErrorCode> {
    let mut vec = Vec::with_capacity({ margin.collateral }.len());

    let max_col = state.total_collaterals;
    for (i, _v) in { margin.collateral }.iter().enumerate() {
        if i >= max_col as usize {
            break;
        }

        let info = &state.collaterals[i];
        let borrow = &cache.borrow_cache[i];

        if info.is_empty() {
            continue;
        }

        let v: I80F48 = get_actual_collateral(
            i,
            margin,
            borrow.supply_multiplier.into(),
            borrow.borrow_multiplier.into(),
        )
        .unwrap();

        let oracle_cache = get_oracle(cache, &info.oracle_symbol).unwrap();
        let price: I80F48 = oracle_cache.price.into();

        // Price is only weighted when collateral is non-negative.
        let weighted_price = match is_weighted && v >= 0u64 {
            true => safe_mul_i80f48(
                price,
                I80F48::from_num(info.weight as f64 / 1000.0),
            ),
            false => price,
        };
        vec.push(safe_mul_i80f48(weighted_price, v));
    }

    Ok(vec)
}

pub fn get_actual_collateral(
    index: usize,
    margin: &Margin,
    supply_multiplier: I80F48,
    borrow_multiplier: I80F48,
) -> Result<I80F48, ErrorCode> {
    let initial_col: I80F48 = margin.collateral[index].into();
    calc_actual_collateral(initial_col, supply_multiplier, borrow_multiplier)
}

pub fn calc_actual_collateral(
    initial_col: I80F48,
    supply_multiplier: I80F48,
    borrow_multiplier: I80F48,
) -> Result<I80F48, ErrorCode> {
    if initial_col > I80F48::ZERO {
        Ok(safe_mul_i80f48(initial_col, supply_multiplier))
    } else {
        Ok(safe_mul_i80f48(initial_col, borrow_multiplier))
    }
}

pub fn largest_open_order(
    cache: &Cache,
    control: &Control,
) -> Result<Option<usize>, ErrorCode> {
    let open_orders: Vec<I80F48> = control
        .open_orders_agg
        .iter()
        .zip(cache.marks)
        .map(|(order, mark)| {
            safe_mul_i80f48(
                I80F48::from_num(order.coin_on_asks.max(order.coin_on_bids)),
                mark.price.into(),
            )
        })
        .collect();

    let open_orders = open_orders.iter().enumerate();

    let open_order: Option<(usize, &I80F48)> =
        match open_orders.max_by_key(|a| a.1) {
            Some(x) => {
                if x.1.is_zero() {
                    None
                } else {
                    Some(x)
                }
            }
            None => return Err(ErrorCode::NoPositions),
        };

    if open_order == None || open_order.unwrap().1.is_zero() {
        return Ok(None);
    }

    Ok(Some(open_order.unwrap().0))
}

pub fn has_open_orders(
    cache: &Cache,
    control: &Control,
) -> Result<bool, ErrorCode> {
    let result = largest_open_order(cache, control)?;
    Ok(result.is_some())
}

pub fn get_total_collateral(
    margin: &Margin,
    cache: &Cache,
    state: &State,
) -> I80F48 {
    let mut total: I80F48 = I80F48::ZERO;
    // Estimate using mark prices.

    for (i, &coll) in { margin.collateral }.iter().enumerate() {
        if coll == WrappedI80F48::zero() {
            continue;
        }

        let oracle =
            get_oracle(cache, &state.collaterals[i].oracle_symbol).unwrap();
        let borrow_cache = cache.borrow_cache[i];
        let usdc_col = safe_mul_i80f48(coll.into(), oracle.price.into());

        let weighted_col: I80F48 = if usdc_col > I80F48::ZERO {
            match state.collaterals[i].weight.try_into() {
                Ok(weight) => safe_mul_i80f48(usdc_col, weight)
                    .checked_div(I80F48::from_num(1000u16))
                    .unwrap(),
                Err(_) => usdc_col,
            }
        } else {
            usdc_col
        };

        let accrued = if coll > WrappedI80F48::zero() {
            safe_mul_i80f48(weighted_col, borrow_cache.supply_multiplier.into())
        } else {
            safe_mul_i80f48(weighted_col, borrow_cache.borrow_multiplier.into())
        };

        total = safe_add_i80f48(total, accrued);
    }

    total
}

#[allow(dead_code)]
fn calc_max_reducible(
    weighted_sum_pimfs: i64,
    weighted_col: i64,
    total_acc_value: i64,
    base_imf: u16,
    price: I80F48,
    liq_fee: I80F48,
) -> Result<i64, ErrorCode> {
    let weighted_col = weighted_col.max(0i64);
    let numerator = weighted_sum_pimfs
        .safe_sub(weighted_col.min(total_acc_value).safe_mul(1000i64)?)?;
    let diff = I80F48::from_num(base_imf) - liq_fee;

    let denom = safe_mul_i80f48(price, diff);
    Ok(I80F48::from_num(numerator)
        .checked_div(denom)
        .unwrap()
        .ceil()
        .checked_to_num()
        .unwrap())
}

#[allow(dead_code)]
fn get_max_reducible_assets(
    base_imf: u16,
    liq_fee: I80F48,
    price: I80F48,
    weighted_col: i64,
    max_markets: usize,
    max_cols: usize,
    cache: &Cache,
    oo_agg: &[OpenOrdersInfo; 50],
    pm: &[PerpMarketInfo; 50],
    margin_col: &[WrappedI80F48; 25],
    col_info_arr: &[CollateralInfo; 25],
) -> Result<i64, ErrorCode> {
    let PerpAccParams {
        total_acc_value,
        has_open_pos_notional: _,
        total_realized_pnl,
        mut pimf_vec,
        mut pmmf_vec,
        pcmf_vec: _,
        mut pos_open_notional_vec,
        mut pos_notional_vec,
    } = get_perp_acc_params(
        weighted_col,
        MfReturnOption::Both,
        max_markets,
        oo_agg,
        &cache.marks,
        pm,
        &{ cache.funding_cache },
    )?;

    let (
        _spot_pos_notional,
        mut spot_imf_vec,
        mut spot_mmf_vec,
        mut spot_pos_notional_vec,
    ) = get_spot_borrows(
        MfReturnOption::Both,
        max_cols,
        margin_col,
        col_info_arr,
        cache,
        total_realized_pnl,
    )?;

    pimf_vec.append(&mut spot_imf_vec);
    pmmf_vec.append(&mut spot_mmf_vec);
    // pos_open_notional_vec.append(&mut spot_pos_notional_vec);
    // pos_notional_vec.append(&mut spot_pos_notional_vec);
    pos_open_notional_vec.extend(spot_pos_notional_vec.iter().clone());
    pos_notional_vec.append(&mut spot_pos_notional_vec);

    let mut weighted_sum_pimfs = 0i64;
    for (i, &pimf) in pimf_vec.iter().enumerate() {
        weighted_sum_pimfs += pos_open_notional_vec[i].safe_mul(pimf as i64)?;
    }

    let max_reducible = calc_max_reducible(
        weighted_sum_pimfs,
        weighted_col,
        total_acc_value,
        base_imf,
        price,
        liq_fee,
    )?;

    Ok(max_reducible)
}

#[allow(dead_code)]
pub fn estimate_spot_liquidation_size(
    // In assets
    margin: &Margin,
    control: &Control,
    state: &State,
    cache: &Cache,
    asset_index: usize, // What the liqee gets
    quote_index: usize,
    fudge: Option<f64>, // Amount to increase by
) -> Result<i64, ErrorCode> {
    let base_imf = SPOT_INITIAL_MARGIN_REQ
        .safe_div(state.collaterals[asset_index].weight as u64)?
        .safe_sub(1000u64)? as u16;
    let liq_fee = (1000 + state.collaterals[asset_index].liq_fee) as f64
        / (1000 - state.collaterals[quote_index].liq_fee) as f64
        - 1.0;
    let num_lf = -1000.0
        + state.collaterals[quote_index].weight as f64 * (1.0 + liq_fee);
    let asset_oracle =
        get_oracle(cache, &state.collaterals[asset_index].oracle_symbol)
            .unwrap();
    let asset_price: I80F48 = asset_oracle.price.into();
    let asset_amount = get_max_reducible_assets(
        base_imf,
        I80F48::from_num(num_lf),
        asset_price,
        get_total_collateral(margin, cache, state).to_num(),
        state.total_markets as usize,
        state.total_collaterals as usize,
        cache,
        &control.open_orders_agg,
        &state.perp_markets,
        &{ margin.collateral },
        &state.collaterals,
    )?; // In smol asset
    
    let usdc_amount = asset_amount.safe_mul(asset_price.to_num::<i64>())?;
    match fudge {
        Some(f) => Ok((f * usdc_amount as f64) as i64),
        None => Ok(usdc_amount),
    }
    /*
    let mut total_position_notional = I80F48::ZERO;

    for (i, position_size) in control
        .open_orders_agg
        .iter()
        .map(|oo_agg| oo_agg.pos_size)
        .enumerate()
    {
        let position_notional = safe_mul_i80f48(
            cache.marks[i].price.into(),
            I80F48::from_num(position_size),
        )
        .checked_div(I80F48::from_num(1_000_000i64))
        .unwrap();
        let increment = safe_mul_i80f48(
            I80F48::from_num(state.perp_markets[i].base_imf),
            position_notional,
        )
        .checked_div(I80F48::from_num(1000i64))
        .unwrap();
        total_position_notional =
            safe_add_i80f48(total_position_notional, increment);
    }

    let mut spot_position_notional = I80F48::ZERO;

    for (i, &coll) in { margin.collateral }.iter().enumerate() {
        if i as u16 >= state.total_collaterals {
            continue;
        }
        let symbol: String = state.collaterals[i].oracle_symbol.into();
        println!("{} {}", i, symbol);
        let coll: I80F48 = safe_mul_i80f48(
            coll.into(),
            get_oracle(cache, &state.collaterals[i].oracle_symbol)
                .unwrap()
                .price
                .into(),
        ).checked_div(I80F48::from_num(1_000_000i64)).unwrap();
        let weight = if coll.is_positive() {
            I80F48::from_num(state.collaterals[i].weight)
                .checked_div(I80F48::from_num(1000i64))
                .unwrap()
        } else {
            let raw = I80F48::from_num(1.1f32)
                .checked_div(I80F48::from_num(state.collaterals[i].weight))
                .unwrap();
            raw.checked_mul(I80F48::from_num(1000i64)).unwrap()
        };

        let increment = safe_mul_i80f48(coll, weight);
        spot_position_notional =
            safe_add_i80f48(increment, spot_position_notional);
    }

    // sub min(o, pnl)

    let mut factor = I80F48::from_num(state.collaterals[asset_index].weight)
        .checked_mul(I80F48::from_num(1000i64))
        .unwrap();
    factor = factor
        .checked_div(
            get_oracle(cache, &state.collaterals[asset_index].oracle_symbol)
                .unwrap()
                .price
                .into(),
        )
        .unwrap();

    factor = match fudge {
        Some(f) => factor.checked_mul(I80F48::from_num(f)).unwrap(),
        None => factor,
    };

    Ok(
        safe_add_i80f48(total_position_notional, spot_position_notional)
            .checked_mul(factor)
            .unwrap()
            .to_num(),
    )
    */
}
