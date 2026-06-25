/// Calculate the output amount and fee for a swap.
///
/// `is_deposit = true`  → base asset in, share tokens out (execute_deposit)
/// `is_deposit = false` → share tokens in, base asset out (execute_withdraw_from_external)
///
/// Prices are 6-decimal fixed-point in the vault's accounting unit. Amounts
/// are in native token units (applying the respective token's decimal scale).
pub fn calc_out_amount(
    is_deposit: bool,
    in_amount: u64,
    share_price: u64,
    share_decimals: u8,
    asset_price: u64,
    asset_decimals: u8,
    fee_bps: u16,
) -> Option<(u64, u64)> {
    const BPS: u128 = 10_000;
    let fee_bps = fee_bps as u128;

    let (price_in, dec_in, price_out, dec_out) = if is_deposit {
        (asset_price as u128, asset_decimals, share_price as u128, share_decimals)
    } else {
        (share_price as u128, share_decimals, asset_price as u128, asset_decimals)
    };

    // out_gross = in * price_in * 10^dec_out / (price_out * 10^dec_in)
    let numerator = (in_amount as u128)
        .checked_mul(price_in)?
        .checked_mul(10u128.pow(dec_out as u32))?;
    let denominator = price_out.checked_mul(10u128.pow(dec_in as u32))?;
    let out_gross = numerator.checked_div(denominator)?;

    let fee = out_gross * fee_bps / BPS;
    let out_net = out_gross.checked_sub(fee)?;

    Some((out_net.try_into().ok()?, fee.try_into().ok()?))
}

/// Calculate the required input for an exact-output swap (ceiling division).
pub fn required_input_amount(
    is_deposit: bool,
    desired_out: u64,
    share_price: u64,
    share_decimals: u8,
    asset_price: u64,
    asset_decimals: u8,
    fee_bps: u16,
) -> u128 {
    const BPS: u128 = 10_000;
    let fee_bps = fee_bps as u128;
    let effective_bps = BPS.checked_sub(fee_bps).expect("fee_bps must be <= 10_000");

    let (price_in, dec_in, price_out, dec_out) = if is_deposit {
        (asset_price as u128, asset_decimals, share_price as u128, share_decimals)
    } else {
        (share_price as u128, share_decimals, asset_price as u128, asset_decimals)
    };

    // in = ceil(desired_out * price_out * 10^dec_in * BPS / (price_in * 10^dec_out * effective_bps))
    let numerator = (desired_out as u128)
        .checked_mul(price_out).expect("overflow")
        .checked_mul(10u128.pow(dec_in as u32)).expect("overflow")
        .checked_mul(BPS).expect("overflow");

    let denominator = price_in
        .checked_mul(10u128.pow(dec_out as u32)).expect("overflow")
        .checked_mul(effective_bps).expect("overflow");

    (numerator + denominator - 1) / denominator
}
