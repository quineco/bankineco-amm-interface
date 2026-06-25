use vault_sdk::Vault;
use jupiter_amm_interface::{
    AccountMap,
    Amm,
    AmmContext,
    KeyedAccount,
    Quote,
    QuoteParams,
    Swap,
    SwapAndAccountMetas,
    SwapMode,
    SwapParams,
    try_get_account_data,
};
use anchor_spl::associated_token::get_associated_token_address;
use anyhow::Result;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::system_program::ID as SystemProgramId;
use solana_pubkey::Pubkey;
use rust_decimal::Decimal;

pub mod constants;
use constants::*;

#[derive(Copy, Clone)]
pub struct BankinecoAmm {
    vault: Pubkey,
    vault_state: Vault,
    share_mint: Pubkey,
    base_asset_mint: Pubkey,
    base_asset_decimals: u8,
    /// Vault's Marginfi user account and the external_liquidity slot index it
    /// occupies, if the vault has Marginfi external liquidity deployed.
    marginfi_position: Option<(Pubkey, u8)>,
}

impl BankinecoAmm {
    pub fn new(vault: Pubkey, vault_state: Vault) -> Self {
        let share_mint = Pubkey::from(vault_state.mint);
        let (base_asset_mint, base_asset_decimals) =
            find_base_holding(&vault_state).unwrap_or((USDC_MINT, 6));
        let marginfi_position = marginfi_position_from_vault(&vault_state);
        Self { vault, vault_state, share_mint, base_asset_mint, base_asset_decimals, marginfi_position }
    }
}

fn find_base_holding(vault: &Vault) -> Option<(Pubkey, u8)> {
    vault.holdings
        .iter()
        .find(|h| h.is_base == 1 && h.mint != [0u8; 32])
        .map(|h| (Pubkey::from(h.mint), h.decimals))
}

/// Returns `(price, decimals)` for the base holding matching `mint`, if any.
fn base_holding_for_mint(vault: &Vault, mint: &Pubkey) -> Option<(u64, u8)> {
    vault.holdings
        .iter()
        .find(|h| h.is_base == 1 && &Pubkey::from(h.mint) == mint)
        .map(|h| (h.price, h.decimals))
}

/// All base-asset holding mints — the vault's whitelisted deposit assets.
fn holding_mints(vault: &Vault) -> Vec<Pubkey> {
    vault.holdings
        .iter()
        .filter(|h| h.is_base == 1 && h.mint != [0u8; 32])
        .map(|h| Pubkey::from(h.mint))
        .collect()
}

// ---------------------------------------------------------------------------
// Marginfi external liquidity helpers
// ---------------------------------------------------------------------------

/// Scan the vault's external_liquidity slots for a Marginfi position.
///
/// Returns `(marginfi_user_account, slot_index)` for the first active slot,
/// or `None` if the vault has no Marginfi liquidity deployed.
///
/// Layout of `MarginfiExternalLiquidityData` (common::state::external_liquidity):
///   [0]      source discriminant (1 = Marginfi)
///   [1..8]   _padding1 (7 bytes)
///   [8..40]  user_account (Pubkey, 32 bytes)
fn marginfi_position_from_vault(vault: &Vault) -> Option<(Pubkey, u8)> {
    vault.external_liquidity
        .iter()
        .enumerate()
        .find(|(_, slot)| slot.data[0] == 1) // ExternalLiquiditySource::Marginfi
        .and_then(|(i, slot)| {
            let bytes: [u8; 32] = slot.data[8..40].try_into().ok()?;
            let pk = Pubkey::from(bytes);
            if pk == Pubkey::default() { None } else { Some((pk, i as u8)) }
        })
}

/// Encode a slice as a borsh `Vec<u8>`: u32 LE length prefix followed by bytes.
fn borsh_vec(v: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(v.len() as u32).to_le_bytes());
    out.extend_from_slice(v);
}

/// Build the serialized `InstructionRefs` bytes for a single Marginfi withdraw CPI.
///
/// This is passed as part of the `execute_withdraw` instruction data alongside
/// `external_liquidity_source` (the slot index). The vault program then uses it
/// to invoke the Marginfi `lending_account_withdraw` CPI via `CpiDispatcher`.
///
/// Wire format (`InstructionRefs` → `CpiRefs` → `CpiMapping`, all borsh):
///   CpiMapping.indices  : [0,1,2,3,4,5,6,7,8]  — 9 remaining_accounts in order
///   CpiMapping.lengths  : [9]                   — one CPI, 9 accounts
///   CpiRefs.types       : [3]                   — CpiType::MARGINFI_WITHDRAW
///   CpiRefs.args        : [0xFF, 0xFF]           — Skip sentinel; amount computed on-chain
///   InstructionRefs.tracked: []
///
/// References:
///   bankineco/rust/crates/common/src/accounts/refs.rs        (InstructionRefs)
///   bankineco/rust/crates/common/src/cpi/refs.rs             (CpiRefs)
///   bankineco/rust/crates/common/src/accounts/cpi.rs         (CpiMapping)
///   bankineco/rust/crates/common/src/cpi/registry.rs         (CpiType::MARGINFI_WITHDRAW = 3)
pub fn build_marginfi_withdraw_instruction_refs() -> Vec<u8> {
    let mut out = Vec::with_capacity(33);
    borsh_vec(&[0, 1, 2, 3, 4, 5, 6, 7, 8], &mut out); // CpiMapping.indices
    borsh_vec(&[9], &mut out);                           // CpiMapping.lengths
    borsh_vec(&[3], &mut out);                           // CpiRefs.types (MARGINFI_WITHDRAW)
    borsh_vec(&[0xFF, 0xFF], &mut out);                  // CpiRefs.args  (Skip sentinel)
    borsh_vec(&[], &mut out);                            // InstructionRefs.tracked
    out
}

/// Build the remaining `AccountMeta`s that the vault program passes through to
/// the Marginfi CPI inside `execute_withdraw`.
///
/// These are appended after the fixed `execute_withdraw` accounts.
/// The account order matches the `CpiMapping.indices` built by
/// [`build_marginfi_withdraw_instruction_refs`]:
///
///   [0] Marginfi program              (readonly)
///   [1] marginfi_group                (writable)
///   [2] marginfi_account              (writable)  ← vault's Marginfi user account
///   [3] vault PDA                     (readonly)  ← signing authority (PDA-signs internally)
///   [4] bank                          (writable)
///   [5] vault_asset_ata               (writable)  ← withdrawal destination
///   [6] bank_liquidity_vault_auth     (readonly)
///   [7] bank_liquidity_vault          (writable)
///   [8] token_program                 (readonly)
///
/// Validation by `validate_external_withdraw_refs` in execute_withdraw.rs:
///   accounts[3].key == vault_key          (signer authority)
///   accounts[5].key == vault_asset_ata    (destination)
pub fn marginfi_withdraw_remaining_accounts(
    marginfi_account: Pubkey,
    vault: Pubkey,
    vault_asset_ata: Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(MARGINFI_PROGRAM_ID, false),
        AccountMeta::new(MAIN_MARGINFI_GROUP, false),
        AccountMeta::new(marginfi_account, false),
        AccountMeta::new_readonly(vault, false),
        AccountMeta::new(MAIN_MARGINFI_BANK, false),
        AccountMeta::new(vault_asset_ata, false),
        AccountMeta::new_readonly(MAIN_MARGINFI_LIQUIDITY_VAULT_AUTH, false),
        AccountMeta::new(MAIN_MARGINFI_LIQUIDITY_VAULT, false),
        AccountMeta::new_readonly(anchor_spl::token::ID, false),
    ]
}

/// Calculate the output amount and fee for a swap.
///
/// `is_deposit = true`  → base asset in, share tokens out (execute_deposit)
/// `is_deposit = false` → share tokens in, base asset out (execute_withdraw)
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

impl Amm for BankinecoAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self>
        where Self: Sized
    {
        let vault_state = Vault::from_account_data(&keyed_account.account.data)
            .map_err(|e| anyhow::anyhow!("Failed to load vault: {:?}", e))?;
        Ok(BankinecoAmm::new(keyed_account.key, vault_state))
    }

    fn label(&self) -> String {
        "PerenaBankinecoAmm".to_string()
    }

    fn program_id(&self) -> Pubkey {
        PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.vault
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        let holdings = holding_mints(&self.vault_state);
        if holdings.is_empty() {
            vec![self.share_mint, self.base_asset_mint]
        } else {
            let mut mints = Vec::with_capacity(1 + holdings.len());
            mints.push(self.share_mint);
            mints.extend(holdings);
            mints
        }
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.vault]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let vault_data = try_get_account_data(account_map, &self.vault)?;
        self.vault_state = Vault::from_account_data(vault_data)
            .map_err(|e| anyhow::anyhow!("Vault load error: {:?}", e))?;

        self.share_mint = Pubkey::from(self.vault_state.mint);
        if let Some((mint, decimals)) = find_base_holding(&self.vault_state) {
            self.base_asset_mint = mint;
            self.base_asset_decimals = decimals;
        }
        self.marginfi_position = marginfi_position_from_vault(&self.vault_state);

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let share_price = self.vault_state.accounting.mint_share_price;
        let share_decimals = self.vault_state.mint_decimals;

        let is_deposit = quote_params.input_mint != self.share_mint;
        let asset_mint = if is_deposit { quote_params.input_mint } else { quote_params.output_mint };
        let (asset_price, asset_decimals) =
            base_holding_for_mint(&self.vault_state, &asset_mint).ok_or_else(|| {
                anyhow::anyhow!("Mint {} is not a whitelisted base asset", asset_mint)
            })?;

        let fee_bps = if is_deposit {
            self.vault_state.config.fees.mint_fee_bps
        } else {
            self.vault_state.config.fees.burn_fee_bps
        };

        let in_amount: u64 = if quote_params.swap_mode == SwapMode::ExactIn {
            quote_params.amount
        } else {
            required_input_amount(
                is_deposit,
                quote_params.amount,
                share_price,
                share_decimals,
                asset_price,
                asset_decimals,
                fee_bps,
            ).try_into()?
        };

        let (out_amount, fee_amount) = calc_out_amount(
            is_deposit,
            in_amount,
            share_price,
            share_decimals,
            asset_price,
            asset_decimals,
            fee_bps,
        ).ok_or_else(|| anyhow::anyhow!("Quote calculation overflow"))?;

        let fee_mint = if is_deposit { self.share_mint } else { asset_mint };

        Ok(Quote {
            in_amount,
            out_amount,
            fee_amount,
            fee_mint,
            fee_pct: Decimal::new(fee_bps.into(), 4),
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams { source_mint, destination_mint, token_transfer_authority, .. } =
            swap_params;

        let is_deposit = !source_mint.eq(&self.share_mint);
        let asset_mint = if is_deposit { source_mint } else { destination_mint };
        let user = token_transfer_authority;

        // PDAs — seeds from the vault program (common crate):
        //   vault_oracle : ["vault_oracle", vault]      (common::state::oracle::VAULT_ORACLE_SEED)
        //   fee_vault    : ["VFEEVAULT",    vault]      (common::state::vault::FEE_VAULT_SEED)
        let vault_oracle = Pubkey::find_program_address(
            &[b"vault_oracle", self.vault.as_ref()],
            &PROGRAM_ID,
        ).0;
        let fee_vault = Pubkey::find_program_address(
            &[b"VFEEVAULT", self.vault.as_ref()],
            &PROGRAM_ID,
        ).0;

        // ATAs
        let user_asset_ata = get_associated_token_address(user, asset_mint);
        let vault_asset_ata = get_associated_token_address(&self.vault, asset_mint);
        let fee_vault_ata = get_associated_token_address(&fee_vault, asset_mint);
        let user_share_ata = get_associated_token_address(user, &self.share_mint);

        // Account order mirrors ExecuteDeposit / ExecuteWithdraw in the vault program:
        //   rust/programs/vault/src/instructions/vault/permissionless/execute_deposit.rs
        //   rust/programs/vault/src/instructions/vault/permissionless/execute_withdraw.rs
        let vault_tranche_state = if self.vault_state.tranching_enabled == 1 {
            Some(
                Pubkey::find_program_address(
                    &[b"vault_tranche", self.vault.as_ref()],
                    &PROGRAM_ID,
                ).0,
            )
        } else {
            None
        };

        let mut account_metas = vec![
            AccountMeta::new(*user, true),
            AccountMeta::new(self.vault, false),
            AccountMeta::new_readonly(vault_oracle, false),
        ];
        if let Some(tranche_state) = vault_tranche_state {
            account_metas.push(AccountMeta::new_readonly(tranche_state, false));
        }
        account_metas.extend_from_slice(&[
            AccountMeta::new_readonly(*asset_mint, false),
            AccountMeta::new(self.share_mint, false),
            AccountMeta::new(user_asset_ata, false),
            AccountMeta::new(vault_asset_ata, false),
            AccountMeta::new(fee_vault, false),
            AccountMeta::new(fee_vault_ata, false),
            AccountMeta::new(user_share_ata, false),
            AccountMeta::new_readonly(anchor_spl::token::ID, false),       // asset_token_program
            AccountMeta::new_readonly(anchor_spl::token::ID, false),       // share_token_program
            AccountMeta::new_readonly(anchor_spl::associated_token::ID, false),
            AccountMeta::new_readonly(SystemProgramId, false),
        ]);

        // For withdrawals, append Marginfi remaining_accounts when the vault has
        // external liquidity deployed. The vault program reads these via
        // `ctx.remaining_accounts` and passes them to the CpiDispatcher.
        //
        // The instruction data for execute_withdraw must also include:
        //   external_withdraw_ix_refs : Some(InstructionRefs)  ← build_marginfi_withdraw_instruction_refs()
        //   external_liquidity_source : Some(slot_index)       ← marginfi_position.1
        // Jupiter is responsible for encoding these into the instruction data alongside
        // the share_amount argument (see execute_withdraw.rs for the full signature).
        if !is_deposit {
            if let Some((marginfi_account, _slot_index)) = self.marginfi_position {
                account_metas.extend(marginfi_withdraw_remaining_accounts(
                    marginfi_account,
                    self.vault,
                    vault_asset_ata,
                ));
            }
        }

        Ok(SwapAndAccountMetas {
            swap: Swap::TokenSwap,
            account_metas,
        })
    }

    fn has_dynamic_accounts(&self) -> bool {
        false
    }

    fn requires_update_for_reserve_mints(&self) -> bool {
        false
    }

    fn supports_exact_out(&self) -> bool {
        true
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    fn unidirectional(&self) -> bool {
        false
    }

    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![]
    }

    fn get_accounts_len(&self) -> usize {
        32
    }

    fn is_active(&self) -> bool {
        self.vault_state.circuit_breaker_active == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::{bytes_of, Zeroable};
    use jupiter_amm_interface::{AmmContext, ClockRef, KeyedAccount, SwapMode};
    use solana_sdk::account::Account;
    use solana_sdk::clock::Clock;
    use vault_sdk::{Vault, VAULT_DISCRIMINATOR};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_vault() -> Vault {
        Vault::zeroed()
    }

    fn vault_bytes(vault: &Vault) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + std::mem::size_of::<Vault>());
        data.extend_from_slice(&VAULT_DISCRIMINATOR);
        data.extend_from_slice(bytes_of(vault));
        data
    }

    fn make_amm_context() -> AmmContext {
        AmmContext { clock_ref: ClockRef::from(Clock::default()) }
    }

    /// Build an AMM with a share price of 1.05, USDC as base, no fee by default.
    fn make_amm() -> BankinecoAmm {
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.mint_share_price = 1_050_000; // 1.05 USD
        vault.config.fees.mint_fee_bps = 0;
        vault.config.fees.burn_fee_bps = 0;
        // Set a base holding (USDC, price = 1.00)
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000; // 1.00 USD
        BankinecoAmm::new(Pubkey::default(), vault)
    }

    // -----------------------------------------------------------------------
    // calc_out_amount
    // -----------------------------------------------------------------------

    #[test]
    fn calc_out_deposit_at_par_no_fee() {
        let (out, fee) = calc_out_amount(true, 1_000_000, 1_000_000, 6, 1_000_000, 6, 0).unwrap();
        assert_eq!(out, 1_000_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_deposit_share_premium() {
        // share_price = 1.05, asset_price = 1.00 → 1 USDC buys 1/1.05 shares
        let (out, fee) =
            calc_out_amount(true, 1_000_000, 1_050_000, 6, 1_000_000, 6, 0).unwrap();
        assert_eq!(out, 952_380); // floor(1_000_000 * 1_000_000 / 1_050_000)
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_deposit_with_fee() {
        // 10 bps on 1_000_000 gross → 1_000 fee, 999_000 net
        let (out, fee) =
            calc_out_amount(true, 1_000_000, 1_000_000, 6, 1_000_000, 6, 10).unwrap();
        assert_eq!(out, 999_000);
        assert_eq!(fee, 1_000);
    }

    #[test]
    fn calc_out_withdraw_at_par_no_fee() {
        let (out, fee) =
            calc_out_amount(false, 1_000_000, 1_000_000, 6, 1_000_000, 6, 0).unwrap();
        assert_eq!(out, 1_000_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_withdraw_share_premium() {
        // share_price = 1.05 → 1 share redeems 1.05 USDC
        let (out, fee) =
            calc_out_amount(false, 1_000_000, 1_050_000, 6, 1_000_000, 6, 0).unwrap();
        assert_eq!(out, 1_050_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_withdraw_with_fee() {
        // burn 30 bps on 1_050_000 gross → 3150 fee, 1_046_850 net
        let (out, fee) =
            calc_out_amount(false, 1_000_000, 1_050_000, 6, 1_000_000, 6, 30).unwrap();
        assert_eq!(out, 1_046_850);
        assert_eq!(fee, 3_150);
    }

    #[test]
    fn calc_out_returns_none_on_zero_share_price() {
        assert!(calc_out_amount(true, 1_000_000, 0, 6, 1_000_000, 6, 0).is_none());
    }

    // -----------------------------------------------------------------------
    // required_input_amount
    // -----------------------------------------------------------------------

    #[test]
    fn required_input_deposit_at_par_no_fee() {
        let req = required_input_amount(true, 1_000_000, 1_000_000, 6, 1_000_000, 6, 0);
        assert_eq!(req, 1_000_000);
    }

    #[test]
    fn required_input_deposit_share_premium() {
        // want 952_380 shares out; at 1.05 share price that requires 952_380 * 1.05 ≈ 1_000_000 USDC
        let req = required_input_amount(true, 952_380, 1_050_000, 6, 1_000_000, 6, 0);
        // floor(952_380 * 1_050_000 / 1_000_000) = 999_999, so req is 1_000_000 with ceil
        assert_eq!(req, 999_999);
    }

    #[test]
    fn required_input_deposit_with_fee() {
        // want exactly 999_000 shares; fee 10 bps; share=asset=1.0
        // gross needed = 999_000 * 10000 / 9990 = 1_000_000
        let req = required_input_amount(true, 999_000, 1_000_000, 6, 1_000_000, 6, 10);
        assert_eq!(req, 1_000_000);
    }

    #[test]
    fn required_input_withdraw_at_par_no_fee() {
        let req = required_input_amount(false, 1_000_000, 1_000_000, 6, 1_000_000, 6, 0);
        assert_eq!(req, 1_000_000);
    }

    // -----------------------------------------------------------------------
    // Roundtrip: exact_in then exact_out should recover original input
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_deposit_exact_in_then_exact_out() {
        let in_amount: u64 = 1_234_567;
        let share_price = 1_050_000u64;
        let asset_price = 1_000_000u64;
        let fee_bps = 30u16;

        let (out, _) =
            calc_out_amount(true, in_amount, share_price, 6, asset_price, 6, fee_bps).unwrap();
        let req = required_input_amount(true, out, share_price, 6, asset_price, 6, fee_bps);

        // Ceil rounding means required >= original, but only by at most 1
        assert!(req as u64 >= in_amount);
        assert!(req as u64 <= in_amount + 1);
    }

    #[test]
    fn roundtrip_withdraw_exact_in_then_exact_out() {
        let in_shares: u64 = 987_654;
        let share_price = 1_050_000u64;
        let asset_price = 1_000_000u64;
        let fee_bps = 20u16;

        let (out, _) =
            calc_out_amount(false, in_shares, share_price, 6, asset_price, 6, fee_bps).unwrap();
        let req = required_input_amount(false, out, share_price, 6, asset_price, 6, fee_bps);

        assert!(req as u64 >= in_shares);
        assert!(req as u64 <= in_shares + 1);
    }

    // -----------------------------------------------------------------------
    // find_base_holding / base_holding_price
    // -----------------------------------------------------------------------

    #[test]
    fn find_base_holding_returns_first_base() {
        let mut vault = make_vault();
        vault.holdings[0].mint = [1u8; 32];
        vault.holdings[0].is_base = 0;
        vault.holdings[0].decimals = 9;
        vault.holdings[1].mint = [2u8; 32];
        vault.holdings[1].is_base = 1;
        vault.holdings[1].decimals = 6;

        let (mint, decimals) = find_base_holding(&vault).unwrap();
        assert_eq!(mint, Pubkey::from([2u8; 32]));
        assert_eq!(decimals, 6);
    }

    #[test]
    fn find_base_holding_ignores_zero_mint() {
        let mut vault = make_vault();
        vault.holdings[0].mint = [0u8; 32]; // zeroed mint
        vault.holdings[0].is_base = 1;
        assert!(find_base_holding(&vault).is_none());
    }

    #[test]
    fn find_base_holding_none_when_empty() {
        let vault = make_vault();
        assert!(find_base_holding(&vault).is_none());
    }

    #[test]
    fn base_holding_for_mint_returns_price_and_decimals() {
        let mut vault = make_vault();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].price = 1_000_000;
        vault.holdings[0].decimals = 6;
        assert_eq!(base_holding_for_mint(&vault, &USDC_MINT), Some((1_000_000, 6)));
    }

    #[test]
    fn base_holding_for_mint_ignores_non_base() {
        let mut vault = make_vault();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 0;
        vault.holdings[0].price = 1_000_000;
        assert_eq!(base_holding_for_mint(&vault, &USDC_MINT), None);
    }

    // -----------------------------------------------------------------------
    // BankinecoAmm::new
    // -----------------------------------------------------------------------

    #[test]
    fn new_extracts_share_mint() {
        let mut vault = make_vault();
        vault.mint = USD_STAR_MINT.to_bytes();
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        assert_eq!(amm.share_mint, USD_STAR_MINT);
    }

    #[test]
    fn new_extracts_base_asset_from_holdings() {
        let mut vault = make_vault();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        assert_eq!(amm.base_asset_mint, USDC_MINT);
        assert_eq!(amm.base_asset_decimals, 6);
    }

    #[test]
    fn new_falls_back_to_usdc_when_no_base_holding() {
        let vault = make_vault();
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        assert_eq!(amm.base_asset_mint, USDC_MINT);
        assert_eq!(amm.base_asset_decimals, 6);
    }

    // -----------------------------------------------------------------------
    // Amm trait: is_active, get_reserve_mints, get_accounts_to_update
    // -----------------------------------------------------------------------

    #[test]
    fn is_active_when_circuit_breaker_off() {
        let mut vault = make_vault();
        vault.circuit_breaker_active = 0;
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        assert!(amm.is_active());
    }

    #[test]
    fn is_inactive_when_circuit_breaker_on() {
        let mut vault = make_vault();
        vault.circuit_breaker_active = 1;
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        assert!(!amm.is_active());
    }

    #[test]
    fn get_reserve_mints_with_single_holding() {
        let amm = make_amm(); // share=USD_STAR, holding=USDC
        let mints = amm.get_reserve_mints();
        assert_eq!(mints, vec![USD_STAR_MINT, USDC_MINT]);
    }

    #[test]
    fn get_reserve_mints_fallback_when_no_holdings() {
        // No holdings set → falls back to [share_mint, USDC_MINT]
        let mut vault = make_vault();
        vault.mint = USD_STAR_MINT.to_bytes();
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        let mints = amm.get_reserve_mints();
        assert_eq!(mints, vec![USD_STAR_MINT, USDC_MINT]);
    }

    #[test]
    fn get_reserve_mints_includes_all_base_holdings() {
        let usdt_mint = Pubkey::new_unique();
        let mut vault = make_vault();
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[1].mint = usdt_mint.to_bytes();
        vault.holdings[1].is_base = 1;
        vault.holdings[1].decimals = 6;
        // holdings[2] has is_base = 0 — should be excluded
        vault.holdings[2].mint = Pubkey::new_unique().to_bytes();
        vault.holdings[2].is_base = 0;
        vault.holdings[2].decimals = 6;
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        let mints = amm.get_reserve_mints();
        assert_eq!(mints, vec![USD_STAR_MINT, USDC_MINT, usdt_mint]);
    }

    #[test]
    fn get_reserve_mints_skips_zeroed_holding_slots() {
        let mut vault = make_vault();
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        // holdings[1] stays zeroed — should be ignored
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        let mints = amm.get_reserve_mints();
        assert_eq!(mints, vec![USD_STAR_MINT, USDC_MINT]);
    }

    #[test]
    fn get_accounts_to_update_returns_vault() {
        let vault_key = Pubkey::new_unique();
        let amm = BankinecoAmm::new(vault_key, make_vault());
        assert_eq!(amm.get_accounts_to_update(), vec![vault_key]);
    }

    // -----------------------------------------------------------------------
    // quote – ExactIn
    // -----------------------------------------------------------------------

    #[test]
    fn quote_deposit_exact_in_no_fee() {
        let amm = make_amm(); // share_price=1.05, asset_price=1.00, fee=0
        let q = amm
            .quote(&QuoteParams {
                amount: 1_050_000,
                input_mint: USDC_MINT,
                output_mint: USD_STAR_MINT,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(q.in_amount, 1_050_000);
        assert_eq!(q.out_amount, 1_000_000); // 1.05 USDC → 1 share
        assert_eq!(q.fee_amount, 0);
    }

    #[test]
    fn quote_withdraw_exact_in_no_fee() {
        let amm = make_amm(); // share_price=1.05
        let q = amm
            .quote(&QuoteParams {
                amount: 1_000_000,
                input_mint: USD_STAR_MINT,
                output_mint: USDC_MINT,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(q.in_amount, 1_000_000);
        assert_eq!(q.out_amount, 1_050_000); // 1 share → 1.05 USDC
        assert_eq!(q.fee_amount, 0);
    }

    #[test]
    fn quote_deposit_exact_in_with_fee() {
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.mint_share_price = 1_000_000;
        vault.config.fees.mint_fee_bps = 10; // 0.1%
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000;
        let amm = BankinecoAmm::new(Pubkey::default(), vault);

        let q = amm
            .quote(&QuoteParams {
                amount: 1_000_000,
                input_mint: USDC_MINT,
                output_mint: USD_STAR_MINT,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(q.out_amount, 999_000);
        assert_eq!(q.fee_amount, 1_000);
        assert_eq!(q.fee_mint, USD_STAR_MINT);
    }

    #[test]
    fn quote_withdraw_exact_in_with_fee() {
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.mint_share_price = 1_000_000;
        vault.config.fees.burn_fee_bps = 20; // 0.2%
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000;
        let amm = BankinecoAmm::new(Pubkey::default(), vault);

        let q = amm
            .quote(&QuoteParams {
                amount: 1_000_000,
                input_mint: USD_STAR_MINT,
                output_mint: USDC_MINT,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(q.out_amount, 998_000);
        assert_eq!(q.fee_amount, 2_000);
        assert_eq!(q.fee_mint, USDC_MINT);
    }

    // -----------------------------------------------------------------------
    // quote – ExactOut
    // -----------------------------------------------------------------------

    #[test]
    fn quote_deposit_exact_out_no_fee() {
        let amm = make_amm(); // share_price=1.05
        let q = amm
            .quote(&QuoteParams {
                amount: 1_000_000, // want exactly 1 share out
                input_mint: USDC_MINT,
                output_mint: USD_STAR_MINT,
                swap_mode: SwapMode::ExactOut,
            })
            .unwrap();
        assert_eq!(q.out_amount, 1_000_000);
        // in_amount should be 1.05 USDC
        assert_eq!(q.in_amount, 1_050_000);
    }

    #[test]
    fn quote_withdraw_exact_out_no_fee() {
        let amm = make_amm(); // share_price=1.05
        let q = amm
            .quote(&QuoteParams {
                amount: 1_050_000, // want exactly 1.05 USDC out
                input_mint: USD_STAR_MINT,
                output_mint: USDC_MINT,
                swap_mode: SwapMode::ExactOut,
            })
            .unwrap();
        assert_eq!(q.out_amount, 1_050_000);
        assert_eq!(q.in_amount, 1_000_000);
    }

    #[test]
    fn quote_fails_when_input_not_whitelisted() {
        let vault = make_vault(); // no holdings set
        let amm = BankinecoAmm::new(Pubkey::default(), vault);
        let result = amm.quote(&QuoteParams {
            amount: 1_000_000,
            input_mint: USDC_MINT,
            output_mint: Pubkey::default(),
            swap_mode: SwapMode::ExactIn,
        });
        assert!(result.is_err());
    }

    #[test]
    fn quote_deposit_non_base_holding_uses_correct_price() {
        // vault has two base holdings: USDC at 1.00 and USDT at 0.99
        let usdt_mint = Pubkey::new_unique();
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.mint_share_price = 1_000_000;
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000;
        vault.holdings[1].mint = usdt_mint.to_bytes();
        vault.holdings[1].is_base = 1;
        vault.holdings[1].decimals = 6;
        vault.holdings[1].price = 990_000; // 0.99 USD
        let amm = BankinecoAmm::new(Pubkey::default(), vault);

        // deposit USDT: 1_000_000 USDT at 0.99 → 990_000 shares
        let q = amm.quote(&QuoteParams {
            amount: 1_000_000,
            input_mint: usdt_mint,
            output_mint: USD_STAR_MINT,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        assert_eq!(q.in_amount, 1_000_000);
        assert_eq!(q.out_amount, 990_000);
        assert_eq!(q.fee_mint, USD_STAR_MINT);
    }

    #[test]
    fn quote_withdraw_to_non_default_base_uses_correct_price() {
        let usdt_mint = Pubkey::new_unique();
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.mint_share_price = 1_000_000;
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000;
        vault.holdings[1].mint = usdt_mint.to_bytes();
        vault.holdings[1].is_base = 1;
        vault.holdings[1].decimals = 6;
        vault.holdings[1].price = 990_000; // 0.99 USD
        let amm = BankinecoAmm::new(Pubkey::default(), vault);

        // withdraw to USDT: 1_000_000 shares at share_price=1.00, usdt_price=0.99
        // → 1_000_000 * 1_000_000 / 990_000 ≈ 1_010_101 USDT
        let q = amm.quote(&QuoteParams {
            amount: 1_000_000,
            input_mint: USD_STAR_MINT,
            output_mint: usdt_mint,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        assert_eq!(q.in_amount, 1_000_000);
        assert_eq!(q.out_amount, 1_010_101);
        assert_eq!(q.fee_mint, usdt_mint);
    }

    // -----------------------------------------------------------------------
    // from_keyed_account and update
    // -----------------------------------------------------------------------

    #[test]
    fn from_keyed_account_parses_vault() {
        let mut vault = make_vault();
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.mint_decimals = 6;
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;

        let vault_key = Pubkey::new_unique();
        let keyed = KeyedAccount {
            key: vault_key,
            account: Account { data: vault_bytes(&vault), ..Account::default() },
            params: None,
        };

        let amm = BankinecoAmm::from_keyed_account(&keyed, &make_amm_context()).unwrap();
        assert_eq!(amm.key(), vault_key);
        assert_eq!(amm.share_mint, USD_STAR_MINT);
        assert_eq!(amm.base_asset_mint, USDC_MINT);
    }

    #[test]
    fn from_keyed_account_rejects_bad_discriminator() {
        let data = vec![0u8; 8 + std::mem::size_of::<Vault>()];
        let keyed = KeyedAccount {
            key: Pubkey::default(),
            account: Account { data, ..Account::default() },
            params: None,
        };
        assert!(BankinecoAmm::from_keyed_account(&keyed, &make_amm_context()).is_err());
    }

    #[test]
    fn update_refreshes_vault_state() {
        let vault_key = Pubkey::new_unique();
        let mut amm = BankinecoAmm::new(vault_key, make_vault());

        let mut updated_vault = make_vault();
        updated_vault.mint = USD_STAR_MINT.to_bytes();
        updated_vault.accounting.mint_share_price = 1_100_000;
        updated_vault.holdings[0].mint = USDC_MINT.to_bytes();
        updated_vault.holdings[0].is_base = 1;
        updated_vault.holdings[0].decimals = 6;
        updated_vault.holdings[0].price = 1_000_000;

        let mut account_map = AccountMap::default();
        account_map.insert(
            vault_key,
            Account { data: vault_bytes(&updated_vault), ..Account::default() },
        );

        amm.update(&account_map).unwrap();
        assert_eq!(amm.vault_state.accounting.mint_share_price, 1_100_000);
        assert_eq!(amm.share_mint, USD_STAR_MINT);
        assert_eq!(amm.base_asset_mint, USDC_MINT);
    }
}
