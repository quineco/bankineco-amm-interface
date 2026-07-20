use vault_sdk::{Vault, VaultTrancheState};
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
use anchor_spl::associated_token::get_associated_token_address_with_program_id;
use anyhow::{anyhow, Result};
use solana_sdk::instruction::AccountMeta;
use solana_pubkey::Pubkey;

pub mod constants;
pub mod holdings;
pub mod liquidity;
pub mod marginfi;
pub mod math;

use constants::*;
use holdings::{find_base_holding, base_holding_for_mint, holding_mints};
use marginfi::{marginfi_position_from_vault, marginfi_withdraw_remaining_accounts};
use math::{calc_out_amount, required_input_amount};

fn vault_tranche_pda(vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault_tranche", vault.as_ref()], &PROGRAM_ID).0
}

#[derive(Copy, Clone)]
pub struct BankinecoAmm {
    vault: Pubkey,
    vault_state: Vault,
    share_mint: Pubkey,
    base_asset_mint: Pubkey,
    base_asset_decimals: u8,
    /// Junior + senior tranche claim on TVL (accounting units). Zero when
    /// tranching is disabled. Regular share-class NAV is `tvl - tranche_value`.
    tranche_value: u64,
    /// Vault's Marginfi user account and the external_liquidity slot index it
    /// occupies, if the vault has Marginfi external liquidity deployed.
    marginfi_position: Option<(Pubkey, u8)>,
}

impl BankinecoAmm {
    pub fn new(vault: Pubkey, vault_state: Vault) -> Self {
        let mut amm = Self {
            vault,
            vault_state,
            share_mint: Pubkey::default(),
            base_asset_mint: USDC_MINT,
            base_asset_decimals: 6,
            tranche_value: 0,
            marginfi_position: None,
        };
        amm.refresh_from_state();
        amm
    }

    fn refresh_from_state(&mut self) {
        self.share_mint = Pubkey::from(self.vault_state.mint);
        if let Some((mint, decimals)) = find_base_holding(&self.vault_state) {
            self.base_asset_mint = mint;
            self.base_asset_decimals = decimals;
        }
        self.marginfi_position = marginfi_position_from_vault(&self.vault_state);
    }
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
        let mut accounts = vec![self.vault];
        if self.vault_state.tranching_enabled == 1 {
            accounts.push(vault_tranche_pda(&self.vault));
        }
        accounts
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let vault_data = try_get_account_data(account_map, &self.vault)?;
        self.vault_state = Vault::from_account_data(vault_data)
            .map_err(|e| anyhow!("Vault load error: {:?}", e))?;
        self.refresh_from_state();

        // Tranche classes claim part of TVL; the regular share class is backed by
        // tvl - tranche_value. Read the tranche state when tranching is enabled.
        self.tranche_value = if self.vault_state.tranching_enabled == 1 {
            let tranche_pda = vault_tranche_pda(&self.vault);
            let tranche_data = try_get_account_data(account_map, &tranche_pda)?;
            let tranche = VaultTrancheState::from_account_data(tranche_data)
                .map_err(|e| anyhow!("Tranche load error: {:?}", e))?;
            tranche.junior.value.saturating_add(tranche.senior.value)
        } else {
            0
        };
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let is_deposit = quote_params.input_mint != self.share_mint;
        let asset_mint = if is_deposit { quote_params.input_mint } else { quote_params.output_mint };
        let (asset_price, asset_decimals) =
            base_holding_for_mint(&self.vault_state, &asset_mint).ok_or_else(|| {
                anyhow!("Mint {} is not a whitelisted base asset", asset_mint)
            })?;

        // Match onchain deposit/withdraw planning: shares are minted/burned against
        // the regular class NAV (TVL net of the tranche classes' claim) ÷ supply,
        // not the floored mint_share_price inverse (which caused quote drift).
        let total_mint_supply = self.vault_state.accounting.total_mint_supply;
        let backing_value = self
            .vault_state
            .accounting
            .tvl
            .saturating_sub(self.tranche_value);

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
                asset_price,
                asset_decimals,
                total_mint_supply,
                backing_value,
                fee_bps,
            )
            .ok_or_else(|| anyhow!("Quote calculation overflow"))?
            .try_into()?
        };

        let (out_amount, fee_amount) = calc_out_amount(
            is_deposit,
            in_amount,
            asset_price,
            asset_decimals,
            total_mint_supply,
            backing_value,
            fee_bps,
        )
        .ok_or_else(|| anyhow!("Quote calculation overflow"))?;

        // Onchain mint/burn fees are taken in the deposit/withdraw asset.
        let fee_mint = asset_mint;

        Ok(Quote {
            in_amount,
            out_amount,
            fee_amount,
            fee_mint,
            fee_pct: rust_decimal::Decimal::new(fee_bps.into(), 4),
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
        let asset_token_program = constants::token_program_for_mint(asset_mint);
        let share_token_program = anchor_spl::token::ID; // USD* is standard SPL Token
        let user_asset_ata = get_associated_token_address_with_program_id(user, asset_mint, &asset_token_program);
        let vault_asset_ata = get_associated_token_address_with_program_id(&self.vault, asset_mint, &asset_token_program);
        let fee_vault_ata = get_associated_token_address_with_program_id(&fee_vault, asset_mint, &asset_token_program);
        let user_share_ata = get_associated_token_address_with_program_id(user, &self.share_mint, &share_token_program);

        // Account order mirrors ExecuteDeposit / ExecuteWithdraw in the vault program:
        //   rust/programs/vault/src/instructions/vault/permissionless/execute_deposit.rs
        //   rust/programs/vault/src/instructions/vault/permissionless/execute_withdraw.rs
        let vault_tranche_state = if self.vault_state.tranching_enabled == 1 {
            Some(vault_tranche_pda(&self.vault))
        } else {
            None
        };

        let mut account_metas = vec![
            AccountMeta::new_readonly(*user, false),
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
            AccountMeta::new_readonly(asset_token_program, false),
            AccountMeta::new_readonly(share_token_program, false),
        ]);

        // For withdrawals, always call execute_withdraw_from_external.
        // When the vault has a Marginfi position, append the 9 remaining_accounts
        // and pass the slot index as in_index so Jupiter can encode:
        //   external_withdraw_ix_refs : Some(build_marginfi_withdraw_instruction_refs())
        //   external_liquidity_source : Some(slot_index)
        // When there is no external liquidity, no extra accounts are appended and
        // Jupiter encodes (None, None); in_index is set to u8::MAX as a sentinel.
        let external_liquidity_source: u8 = if !is_deposit {
            if let Some((marginfi_account, slot_index)) = self.marginfi_position {
                if let Some(mint_config) = constants::marginfi_config_for_mint(asset_mint) {
                    account_metas.extend(marginfi_withdraw_remaining_accounts(
                        marginfi_account,
                        self.vault,
                        vault_asset_ata,
                        mint_config,
                        constants::token_program_for_mint(asset_mint),
                    ));
                }
                slot_index
            } else {
                u8::MAX
            }
        } else {
            0
        };

        // TODO: Jupiter needs new Swap variants for the Bankineco vault:
        //   - BankinecoDeposit: execute_deposit no longer takes an out_amount argument.
        //   - BankinecoWithdrawFromExternal { external_liquidity_source: u8 }: calls
        //     execute_withdraw_from_external; external_liquidity_source is the vault's
        //     external_liquidity slot index (u8::MAX = no external position → None, None).
        // Using TokenSwap as a placeholder until those variants are added.
        let swap = Swap::TokenSwap;
        let _ = external_liquidity_source;

        Ok(SwapAndAccountMetas {
            swap,
            account_metas,
        })
    }

    fn has_dynamic_accounts(&self) -> bool {
        // Accounts vary with vault state: marginfi position adds 9 remaining accounts,
        // tranching adds the tranche_state account.
        true
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
        if self.marginfi_position.is_some() {
            vec![(MARGINFI_PROGRAM_ID, "marginfi".to_string())]
        } else {
            vec![]
        }
    }

    fn get_accounts_len(&self) -> usize {
        // Fixed accounts: user, vault, vault_oracle, asset_mint, share_mint,
        // user_asset_ata, vault_asset_ata, fee_vault, fee_vault_ata,
        // user_share_ata, asset_token_program, share_token_program = 12
        let tranche = if self.vault_state.tranching_enabled == 1 { 1 } else { 0 };
        // Marginfi remaining accounts (9) are only appended on withdrawals, but
        // we report the max so Jupiter can allocate the worst-case account list.
        let marginfi = if self.marginfi_position.is_some() { 9 } else { 0 };
        12 + tranche + marginfi
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
    use vault_sdk::{Vault, VaultTrancheState, VAULT_DISCRIMINATOR, VAULT_TRANCHE_STATE_DISCRIMINATOR};

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

    fn tranche_bytes(tranche: &VaultTrancheState) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + std::mem::size_of::<VaultTrancheState>());
        data.extend_from_slice(&VAULT_TRANCHE_STATE_DISCRIMINATOR);
        data.extend_from_slice(bytes_of(tranche));
        data
    }

    fn make_amm_context() -> AmmContext {
        AmmContext { clock_ref: ClockRef::from(Clock::default()) }
    }

    /// Build an AMM with share NAV 1.05, USDC as base, no fee by default.
    ///
    /// `tvl / supply = 1.05` exactly so share price and the onchain ratio agree.
    fn make_amm() -> BankinecoAmm {
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.total_mint_supply = 1_000_000_000_000;
        vault.accounting.tvl = 1_050_000_000_000; // 1.05 per share
        vault.accounting.mint_share_price = 1_050_000;
        vault.config.fees.mint_fee_bps = 0;
        vault.config.fees.burn_fee_bps = 0;
        // Set a base holding (USDC, price = 1.00)
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000; // 1.00 USD
        BankinecoAmm::new(Pubkey::default(), vault)
    }

    // supply/tvl fixture used by unit math tests (NAV = 1.05)
    const TEST_SUPPLY: u64 = 1_000_000_000_000;
    const TEST_TVL_PREMIUM: u64 = 1_050_000_000_000;
    const TEST_TVL_PAR: u64 = 1_000_000_000_000;

    // -----------------------------------------------------------------------
    // calc_out_amount
    // -----------------------------------------------------------------------

    #[test]
    fn calc_out_deposit_at_par_no_fee() {
        let (out, fee) =
            calc_out_amount(true, 1_000_000, 1_000_000, 6, TEST_SUPPLY, TEST_TVL_PAR, 0).unwrap();
        assert_eq!(out, 1_000_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_deposit_share_premium() {
        // tvl/supply = 1.05 → 1 USDC buys floor(1e6 * supply / tvl) shares
        let (out, fee) = calc_out_amount(
            true,
            1_000_000,
            1_000_000,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            0,
        )
        .unwrap();
        assert_eq!(out, 952_380); // floor(1_000_000 * 1e12 / 1.05e12)
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_deposit_with_fee() {
        // 10 bps on 1_000_000 accounting → 1_000 fee (asset), 999_000 shares
        let (out, fee) =
            calc_out_amount(true, 1_000_000, 1_000_000, 6, TEST_SUPPLY, TEST_TVL_PAR, 10)
                .unwrap();
        assert_eq!(out, 999_000);
        assert_eq!(fee, 1_000);
    }

    #[test]
    fn calc_out_deposit_matches_onchain_not_share_price_inverse() {
        // Floored mint_share_price would over-quote; supply/tvl must win.
        // share_price = floor(tvl * 1e6 / supply) = 1_000_000, but tvl/supply > 1.
        let supply = 1_000_000_000_000u64;
        let tvl = 1_000_000_000_000u64 + 999_999;
        let in_amount = 555_000_000u64;

        let (out, _) =
            calc_out_amount(true, in_amount, 1_000_000, 6, supply, tvl, 0).unwrap();
        let onchain = (in_amount as u128 * supply as u128 / tvl as u128) as u64;
        let share_price_inverse = in_amount; // floor(in * 1e6 / 1_000_000)

        assert_eq!(out, onchain);
        assert!(share_price_inverse - out >= 400);
    }

    #[test]
    fn calc_out_withdraw_at_par_no_fee() {
        let (out, fee) =
            calc_out_amount(false, 1_000_000, 1_000_000, 6, TEST_SUPPLY, TEST_TVL_PAR, 0)
                .unwrap();
        assert_eq!(out, 1_000_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_withdraw_share_premium() {
        // 1 share redeems floor(1e6 * tvl / supply) = 1.05 USDC
        let (out, fee) = calc_out_amount(
            false,
            1_000_000,
            1_000_000,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            0,
        )
        .unwrap();
        assert_eq!(out, 1_050_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn calc_out_withdraw_with_fee() {
        // burn 30 bps on 1_050_000 accounting → 3150 fee, 1_046_850 net tokens
        let (out, fee) = calc_out_amount(
            false,
            1_000_000,
            1_000_000,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            30,
        )
        .unwrap();
        assert_eq!(out, 1_046_850);
        assert_eq!(fee, 3_150);
    }

    #[test]
    fn calc_out_returns_none_on_zero_asset_price() {
        assert!(
            calc_out_amount(true, 1_000_000, 0, 6, TEST_SUPPLY, TEST_TVL_PAR, 0).is_none()
        );
    }

    // -----------------------------------------------------------------------
    // required_input_amount
    // -----------------------------------------------------------------------

    #[test]
    fn required_input_deposit_at_par_no_fee() {
        let req =
            required_input_amount(true, 1_000_000, 1_000_000, 6, TEST_SUPPLY, TEST_TVL_PAR, 0)
                .unwrap();
        assert_eq!(req, 1_000_000);
    }

    #[test]
    fn required_input_deposit_share_premium() {
        // want 952_380 shares; ceil(952_380 * tvl / supply) accounting at par asset
        let req = required_input_amount(
            true,
            952_380,
            1_000_000,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            0,
        )
        .unwrap();
        assert_eq!(req, 999_999);
    }

    #[test]
    fn required_input_deposit_with_fee() {
        // want exactly 999_000 shares; fee 10 bps; at par
        let req =
            required_input_amount(true, 999_000, 1_000_000, 6, TEST_SUPPLY, TEST_TVL_PAR, 10)
                .unwrap();
        assert_eq!(req, 1_000_000);
    }

    #[test]
    fn required_input_withdraw_at_par_no_fee() {
        let req =
            required_input_amount(false, 1_000_000, 1_000_000, 6, TEST_SUPPLY, TEST_TVL_PAR, 0)
                .unwrap();
        assert_eq!(req, 1_000_000);
    }

    // -----------------------------------------------------------------------
    // Roundtrip: exact_in then exact_out should recover original input
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_deposit_exact_in_then_exact_out() {
        let in_amount: u64 = 1_234_567;
        let asset_price = 1_000_000u64;
        let fee_bps = 30u16;

        let (out, _) = calc_out_amount(
            true,
            in_amount,
            asset_price,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            fee_bps,
        )
        .unwrap();
        let req = required_input_amount(
            true,
            out,
            asset_price,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            fee_bps,
        )
        .unwrap();

        // Ceil rounding means required >= original, but only by a small amount
        assert!(req as u64 >= in_amount);
        assert!(req as u64 <= in_amount + 2);
    }

    #[test]
    fn roundtrip_withdraw_exact_in_then_exact_out() {
        let in_shares: u64 = 987_654;
        let asset_price = 1_000_000u64;
        let fee_bps = 20u16;

        let (out, _) = calc_out_amount(
            false,
            in_shares,
            asset_price,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            fee_bps,
        )
        .unwrap();
        let req = required_input_amount(
            false,
            out,
            asset_price,
            6,
            TEST_SUPPLY,
            TEST_TVL_PREMIUM,
            fee_bps,
        )
        .unwrap();

        assert!(req as u64 >= in_shares);
        assert!(req as u64 <= in_shares + 2);
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

    #[test]
    fn get_accounts_to_update_includes_tranche_when_enabled() {
        let vault_key = Pubkey::new_unique();
        let mut vault = make_vault();
        vault.tranching_enabled = 1;
        let amm = BankinecoAmm::new(vault_key, vault);
        assert_eq!(
            amm.get_accounts_to_update(),
            vec![vault_key, vault_tranche_pda(&vault_key)]
        );
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
        vault.accounting.total_mint_supply = TEST_SUPPLY;
        vault.accounting.tvl = TEST_TVL_PAR;
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
        assert_eq!(q.fee_mint, USDC_MINT);
    }

    #[test]
    fn quote_withdraw_exact_in_with_fee() {
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.total_mint_supply = TEST_SUPPLY;
        vault.accounting.tvl = TEST_TVL_PAR;
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
        vault.accounting.total_mint_supply = TEST_SUPPLY;
        vault.accounting.tvl = TEST_TVL_PAR;
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

        // deposit USDT: 1_000_000 USDT at 0.99 → 990_000 accounting → 990_000 shares
        let q = amm.quote(&QuoteParams {
            amount: 1_000_000,
            input_mint: usdt_mint,
            output_mint: USD_STAR_MINT,
            swap_mode: SwapMode::ExactIn,
        }).unwrap();
        assert_eq!(q.in_amount, 1_000_000);
        assert_eq!(q.out_amount, 990_000);
        assert_eq!(q.fee_mint, usdt_mint);
    }

    #[test]
    fn quote_withdraw_to_non_default_base_uses_correct_price() {
        let usdt_mint = Pubkey::new_unique();
        let mut vault = make_vault();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.accounting.total_mint_supply = TEST_SUPPLY;
        vault.accounting.tvl = TEST_TVL_PAR;
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
        assert_eq!(amm.tranche_value, 0);
    }

    #[test]
    fn update_loads_tranche_value_when_tranching_enabled() {
        let vault_key = Pubkey::new_unique();
        let mut amm = BankinecoAmm::new(vault_key, make_vault());

        let mut updated_vault = make_vault();
        updated_vault.tranching_enabled = 1;
        updated_vault.mint = USD_STAR_MINT.to_bytes();
        updated_vault.accounting.tvl = 1_000_000_000_000;
        updated_vault.holdings[0].mint = USDC_MINT.to_bytes();
        updated_vault.holdings[0].is_base = 1;
        updated_vault.holdings[0].decimals = 6;
        updated_vault.holdings[0].price = 1_000_000;

        let mut tranche = VaultTrancheState::zeroed();
        tranche.junior.value = 100_000_000_000;
        tranche.senior.value = 50_000_000_000;

        let mut account_map = AccountMap::default();
        account_map.insert(
            vault_key,
            Account { data: vault_bytes(&updated_vault), ..Account::default() },
        );
        account_map.insert(
            vault_tranche_pda(&vault_key),
            Account { data: tranche_bytes(&tranche), ..Account::default() },
        );

        amm.update(&account_map).unwrap();
        assert_eq!(amm.tranche_value, 150_000_000_000);
    }

    #[test]
    fn quote_deposit_uses_regular_class_nav_when_tranched() {
        let mut amm = make_amm();
        // tvl=1.05e12, tranche claim=0.05e12 → regular backing = 1.00e12 (= supply)
        amm.tranche_value = 50_000_000_000;
        let q = amm
            .quote(&QuoteParams {
                amount: 1_000_000,
                input_mint: USDC_MINT,
                output_mint: USD_STAR_MINT,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();
        assert_eq!(q.out_amount, 1_000_000);
        assert_eq!(q.fee_amount, 0);
    }
}
