//! Quote math mirroring the vault program's deposit / withdraw planning.
//!
//! Onchain minting does **not** invert `mint_share_price`. It converts the
//! deposited asset to an accounting amount, takes the mint fee in accounting
//! units, then mints:
//!
//! ```text
//! shares = floor(net_accounting × total_mint_supply / backing_value)
//! ```
//!
//! (or `net_accounting` when supply or backing is zero). Withdrawals use the
//! inverse ratio, then convert accounting value back to asset tokens.
//!
//! See `vault::mint_burn::plan_deposit_with_regular_backing_and_new_holding_price`
//! and `Vault::shares_for_deposit_with_value`.

const BPS: u128 = 10_000;

#[inline]
fn mul_div(a: u128, b: u128, c: u128) -> Option<u128> {
    a.checked_mul(b)?.checked_div(c)
}

#[inline]
fn ceil_div(a: u128, b: u128) -> Option<u128> {
    if b == 0 {
        return None;
    }
    a.checked_add(b - 1)?.checked_div(b)
}

/// `floor(token_amount × price / 10^decimals)` — asset tokens → accounting units.
fn accounting_amount_for_asset(
    token_amount: u64,
    asset_price: u64,
    asset_decimals: u8,
) -> Option<u64> {
    if asset_price == 0 {
        return None;
    }
    let scale = 10u128.pow(asset_decimals as u32);
    let amount = mul_div(token_amount as u128, asset_price as u128, scale)?;
    amount.try_into().ok()
}

/// `floor(accounting × 10^decimals / price)` — accounting units → asset tokens.
fn token_amount_for_accounting(
    accounting_value: u64,
    asset_price: u64,
    asset_decimals: u8,
) -> Option<u64> {
    if accounting_value == 0 {
        return Some(0);
    }
    if asset_price == 0 {
        return None;
    }
    let scale = 10u128.pow(asset_decimals as u32);
    let amount = mul_div(accounting_value as u128, scale, asset_price as u128)?;
    amount.try_into().ok()
}

/// `floor(amount × supply / backing)`, or `amount` when the vault is empty.
fn shares_for_deposit(amount: u64, total_mint_supply: u64, backing_value: u64) -> Option<u64> {
    if total_mint_supply == 0 || backing_value == 0 {
        return Some(amount);
    }
    mul_div(amount as u128, total_mint_supply as u128, backing_value as u128)?.try_into().ok()
}

/// `floor(shares × backing / supply)`.
fn amount_for_withdraw_shares(
    share_amount: u64,
    total_mint_supply: u64,
    backing_value: u64,
) -> Option<u64> {
    if total_mint_supply == 0 {
        return None;
    }
    mul_div(
        share_amount as u128,
        backing_value as u128,
        total_mint_supply as u128,
    )?
    .try_into()
    .ok()
}

fn fee_for_bps(amount: u64, fee_bps: u16) -> Option<u64> {
    mul_div(amount as u128, fee_bps as u128, BPS)?.try_into().ok()
}

/// Deposit quote: base asset in → share tokens out.
///
/// Returns `(shares_out, fee_in_asset_tokens)`.
pub fn calc_deposit_out(
    in_amount: u64,
    asset_price: u64,
    asset_decimals: u8,
    total_mint_supply: u64,
    backing_value: u64,
    fee_bps: u16,
) -> Option<(u64, u64)> {
    let accounting = accounting_amount_for_asset(in_amount, asset_price, asset_decimals)?;
    if accounting == 0 {
        return None;
    }
    let fee_accounting = fee_for_bps(accounting, fee_bps)?;
    let net_accounting = accounting.checked_sub(fee_accounting)?;
    let shares = shares_for_deposit(net_accounting, total_mint_supply, backing_value)?;
    let fee_tokens = token_amount_for_accounting(fee_accounting, asset_price, asset_decimals)?;
    Some((shares, fee_tokens))
}

/// Withdraw quote: share tokens in → base asset out.
///
/// Returns `(asset_tokens_out, fee_in_asset_tokens)`.
pub fn calc_withdraw_out(
    in_shares: u64,
    asset_price: u64,
    asset_decimals: u8,
    total_mint_supply: u64,
    backing_value: u64,
    fee_bps: u16,
) -> Option<(u64, u64)> {
    let amount_out = amount_for_withdraw_shares(in_shares, total_mint_supply, backing_value)?;
    let fee_accounting = fee_for_bps(amount_out, fee_bps)?;
    let net_accounting = amount_out.checked_sub(fee_accounting)?;
    let net_tokens = token_amount_for_accounting(net_accounting, asset_price, asset_decimals)?;
    let fee_tokens = token_amount_for_accounting(fee_accounting, asset_price, asset_decimals)?;
    Some((net_tokens, fee_tokens))
}

/// Calculate the output amount and fee for a swap.
///
/// `is_deposit = true`  → base asset in, share tokens out (`execute_deposit`)
/// `is_deposit = false` → share tokens in, base asset out (`execute_withdraw_*`)
///
/// `backing_value` is the regular share class NAV (`tvl`, or `tvl − tranche`
/// value when tranching is enabled).
pub fn calc_out_amount(
    is_deposit: bool,
    in_amount: u64,
    asset_price: u64,
    asset_decimals: u8,
    total_mint_supply: u64,
    backing_value: u64,
    fee_bps: u16,
) -> Option<(u64, u64)> {
    if is_deposit {
        calc_deposit_out(
            in_amount,
            asset_price,
            asset_decimals,
            total_mint_supply,
            backing_value,
            fee_bps,
        )
    } else {
        calc_withdraw_out(
            in_amount,
            asset_price,
            asset_decimals,
            total_mint_supply,
            backing_value,
            fee_bps,
        )
    }
}

/// Required asset input for an exact-output deposit (ceiling division).
pub fn required_deposit_input(
    desired_shares: u64,
    asset_price: u64,
    asset_decimals: u8,
    total_mint_supply: u64,
    backing_value: u64,
    fee_bps: u16,
) -> Option<u128> {
    if asset_price == 0 || fee_bps as u128 > BPS {
        return None;
    }

    // Minimum net accounting such that shares_for_deposit(net) >= desired.
    let net_min: u128 = if total_mint_supply == 0 || backing_value == 0 {
        desired_shares as u128
    } else {
        ceil_div(
            (desired_shares as u128).checked_mul(backing_value as u128)?,
            total_mint_supply as u128,
        )?
    };

    let effective_bps = BPS - fee_bps as u128;
    let accounting_min = if fee_bps == 0 {
        net_min
    } else {
        ceil_div(net_min.checked_mul(BPS)?, effective_bps)?
    };

    let scale = 10u128.pow(asset_decimals as u32);
    ceil_div(accounting_min.checked_mul(scale)?, asset_price as u128)
}

/// Required share input for an exact-output withdraw (ceiling division).
pub fn required_withdraw_input(
    desired_tokens: u64,
    asset_price: u64,
    asset_decimals: u8,
    total_mint_supply: u64,
    backing_value: u64,
    fee_bps: u16,
) -> Option<u128> {
    if asset_price == 0 || total_mint_supply == 0 || backing_value == 0 || fee_bps as u128 > BPS {
        return None;
    }

    // ExactIn: amount_out = floor(shares × backing / supply)
    //          net_acc    = amount_out − fee(amount_out)
    //          tokens     = floor(net_acc × 10^dec / price)
    // Invert: net_acc >= ceil(desired × price / 10^dec)
    let scale = 10u128.pow(asset_decimals as u32);
    let net_acc_min = ceil_div((desired_tokens as u128).checked_mul(asset_price as u128)?, scale)?;
    let effective_bps = BPS - fee_bps as u128;
    let amount_out_min = if fee_bps == 0 {
        net_acc_min
    } else {
        ceil_div(net_acc_min.checked_mul(BPS)?, effective_bps)?
    };

    // shares >= ceil(amount_out_min × supply / backing)
    ceil_div(
        amount_out_min.checked_mul(total_mint_supply as u128)?,
        backing_value as u128,
    )
}

/// Calculate the required input for an exact-output swap (ceiling division).
pub fn required_input_amount(
    is_deposit: bool,
    desired_out: u64,
    asset_price: u64,
    asset_decimals: u8,
    total_mint_supply: u64,
    backing_value: u64,
    fee_bps: u16,
) -> Option<u128> {
    if is_deposit {
        required_deposit_input(
            desired_out,
            asset_price,
            asset_decimals,
            total_mint_supply,
            backing_value,
            fee_bps,
        )
    } else {
        required_withdraw_input(
            desired_out,
            asset_price,
            asset_decimals,
            total_mint_supply,
            backing_value,
            fee_bps,
        )
    }
}
