use anchor_spl::associated_token::get_associated_token_address;
use bankineco_helpers::{ bank::BankState, oracle::OracleGenState, vault::VaultGenState };
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
use anyhow::Result;
use solana_sdk::{
    instruction::AccountMeta,
    system_program::ID as SystemProgramId,
    sysvar::instructions::BorrowedAccountMeta,
};
use solana_pubkey::Pubkey;
use rust_decimal::Decimal;

pub mod constants;
use constants::*;

#[derive(Copy, Clone)]
pub struct BankinecoAmm {
    bank: Pubkey,
    bank_state: Option<BankState>,
    vault: Pubkey,
    vault_state: VaultGenState,
    oracle: Pubkey,
    oracle_state: Option<OracleGenState>,
    team: Pubkey,
    yielding_mint_program: Pubkey,
}

impl BankinecoAmm {
    pub fn new(vault: Pubkey, vault_state: VaultGenState) -> Self {
        let oracle = Pubkey::find_program_address(&[b"VORACLEA", vault.as_ref()], &PROGRAM_ID).0;
        let team = Pubkey::find_program_address(&[b"VTEAMA", vault.as_ref()], &PROGRAM_ID).0;
        Self {
            bank: USD_STAR_BANK,
            bank_state: None,
            vault,
            vault_state,
            oracle,
            oracle_state: None,
            team,
            yielding_mint_program: anchor_spl::token::ID,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct BankinecoSwapAction {
    user: Pubkey,
    bank: Pubkey,
    vault: Pubkey,
    oracle: Pubkey,
    yielding_mint: Pubkey,
    bank_mint: Pubkey,
    team: Pubkey,
    yielding_mint_program: Pubkey,
}

impl BankinecoSwapAction {
    fn to_account_metas(accounts: Self, is_mint: bool) -> Result<Vec<AccountMeta>> {
        let yielding_user_ta = get_associated_token_address(
            &accounts.user,
            &accounts.yielding_mint
        );
        let bank_mint_user_ta = get_associated_token_address(&accounts.user, &accounts.bank_mint);
        let yielding_vault_ta = get_associated_token_address(
            &accounts.vault,
            &accounts.yielding_mint
        );
        let fee_team_ta = get_associated_token_address(&accounts.team, &accounts.yielding_mint);

        let mut account_metas = vec![
            AccountMeta::new(accounts.user, true),
            AccountMeta::new(accounts.bank, false),
            AccountMeta::new(accounts.vault, false),
            AccountMeta::new_readonly(accounts.oracle, false),
            AccountMeta::new_readonly(accounts.yielding_mint, false),
            AccountMeta::new(accounts.bank_mint, false),
            AccountMeta::new(yielding_user_ta, false),
            AccountMeta::new(bank_mint_user_ta, false),
            AccountMeta::new(yielding_vault_ta, false),
            AccountMeta::new(accounts.team, false),
            AccountMeta::new(fee_team_ta, false),
            AccountMeta::new_readonly(SystemProgramId, false),
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
            AccountMeta::new_readonly(accounts.yielding_mint_program, false),
            AccountMeta::new_readonly(anchor_spl::associated_token::ID, false)
        ];

        // Remaining accounts
        if accounts.vault.eq(&MAIN_USDC_VAULT) {
            account_metas.extend_from_slice(
                &[
                    AccountMeta::new_readonly(MARGINFI_PROGRAM_ID, false),
                    AccountMeta::new_readonly(MAIN_MARGINFI_GROUP, false),
                    AccountMeta::new(MAIN_MARGINFI_ACCOUNT, false),
                    AccountMeta::new(MAIN_MARGINFI_BANK, false),
                ]
            );

            if !is_mint {
                account_metas.push(
                    AccountMeta::new_readonly(MAIN_MARGINFI_LIQUIDITY_VAULT_AUTH, false)
                );
            }

            account_metas.push(AccountMeta::new(MAIN_MARGINFI_LIQUIDITY_VAULT, false));

            if !is_mint {
                account_metas.extend_from_slice(
                    &[
                        AccountMeta::new_readonly(MAIN_MARGINFI_BANK, false),
                        AccountMeta::new_readonly(MAIN_MARGINFI_ORACLE, false),
                    ]
                );
            }
        }

        Ok(account_metas)
    }
}

pub fn required_input_amount_u128(
    is_mint: bool,
    desired_out: u64,
    yielding_mint_price: u64,
    bank_mint_price: u64,
    fee_bps: u16
) -> u128 {
    const BPS_DENOM: u128 = 10_000;

    let fee_bps_u128 = fee_bps as u128;
    let effective_bps = BPS_DENOM.checked_sub(fee_bps_u128).expect("fee_bps must be <= 10_000");

    // Choose which price is input vs output based on direction.
    let (price_in, price_out) = if is_mint {
        (yielding_mint_price as u128, bank_mint_price as u128)
    } else {
        (bank_mint_price as u128, yielding_mint_price as u128)
    };

    // numerator = desired_out * price_out * BPS_DENOM
    let numerator = (desired_out as u128)
        .checked_mul(price_out)
        .expect("overflow in desired_out * price_out")
        .checked_mul(BPS_DENOM)
        .expect("overflow in * BPS_DENOM");

    // denominator = price_in * effective_bps
    let denominator = price_in
        .checked_mul(effective_bps)
        .expect("overflow in price_in * effective_bps");

    // ceil_div(numerator, denominator)
    numerator.checked_add(denominator - 1).expect("overflow in ceil adjustment") / denominator
}

impl Amm for BankinecoAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self>
        where Self: Sized
    {
        let vault_state = VaultGenState::from_data(&keyed_account.account.data).map_err(|e|
            anyhow::anyhow!("Failed to load vault {:?}", e)
        )?;
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
        vec![USD_STAR_MINT, USDC_MINT]
    }

    /// The accounts necessary to produce a quote
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.bank, self.oracle]
    }

    /// Picks necessary accounts to update it's internal state
    /// Heavy deserialization and precomputation caching should be done in this function
    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let bank_data = try_get_account_data(account_map, &self.bank)?;
        self.bank_state = Some(
            BankState::from_data(bank_data).map_err(|e|
                anyhow::anyhow!("Bank load error: {:?}", e)
            )?
        );

        let oracle_data = try_get_account_data(account_map, &self.oracle)?;
        self.oracle_state = Some(
            OracleGenState::from_data(oracle_data).map_err(|e|
                anyhow::anyhow!("Oracle load error: {:?}", e)
            )?
        );

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let yielding_mint = Pubkey::from(self.vault_state.config.yielding_token_mint);
        let is_mint = quote_params.input_mint.eq(&yielding_mint);

        let fee_bps = if is_mint {
            self.vault_state.config.minting_fee_bps
        } else {
            self.vault_state.config.burning_fee_bps
        };

        let in_amount: u64 = if quote_params.swap_mode == SwapMode::ExactIn {
            quote_params.amount
        } else {
            let bank_mint_price = self.bank_state.unwrap().mint.price;
            let yielding_mint_price = self.oracle_state.unwrap().result.yielding_token_price;

            required_input_amount_u128(
                is_mint,
                quote_params.amount,
                yielding_mint_price,
                bank_mint_price,
                fee_bps
            ).try_into()?
        };

        let (out_amount, fee) = if is_mint {
            let (out_amount, fee, _) = self.vault_state
                .calc_yielding_to_bank_mint(
                    in_amount,
                    self.bank_state.unwrap().mint.price,
                    self.bank_state.unwrap().mint.decimals,
                    self.oracle_state.unwrap().result.yielding_token_price
                )
                .map_err(|e| anyhow::anyhow!("Failed to calculate {:?}", e))?;

            (out_amount, fee)
        } else {
            let (out_amount, fee, _) = self.vault_state
                .calc_bank_mint_to_yielding(
                    in_amount,
                    self.bank_state.unwrap().mint.price,
                    self.bank_state.unwrap().mint.decimals,
                    self.oracle_state.unwrap().result.yielding_token_price
                )
                .map_err(|e| anyhow::anyhow!("Failed to calculate {:?}", e))?;

            (out_amount, fee)
        };

        Ok(Quote {
            in_amount,
            out_amount: out_amount,
            fee_amount: fee,
            fee_mint: yielding_mint,
            fee_pct: Decimal::new(fee_bps.into(), 4),
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams { source_mint, destination_mint, token_transfer_authority, .. } =
            swap_params;

        let user = token_transfer_authority;
        let yielding_mint = Pubkey::from(self.vault_state.config.yielding_token_mint);
        let is_mint = source_mint.eq(&yielding_mint);

        let (yielding_mint, bank_mint) = if is_mint {
            (source_mint, destination_mint)
        } else {
            (destination_mint, source_mint)
        };

        Ok(SwapAndAccountMetas {
            swap: Swap::TokenSwap,
            account_metas: BankinecoSwapAction::to_account_metas(
                BankinecoSwapAction {
                    user: *user,
                    bank: self.bank,
                    vault: self.vault,
                    oracle: self.oracle,
                    yielding_mint: *yielding_mint,
                    bank_mint: *bank_mint,
                    team: self.team,
                    yielding_mint_program: self.yielding_mint_program,
                },
                is_mint
            )?,
        })
    }

    fn has_dynamic_accounts(&self) -> bool {
        false
    }

    /// Indicates whether `update` needs to be called before `get_reserve_mints`
    fn requires_update_for_reserve_mints(&self) -> bool {
        false
    }

    fn supports_exact_out(&self) -> bool {
        true
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    /// It can only trade in one direction from its first mint to second mint, assuming it is a two mint AMM
    fn unidirectional(&self) -> bool {
        false
    }

    /// For testing purposes, provide a mapping of dependency programs to function
    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![]
    }

    fn get_accounts_len(&self) -> usize {
        32 // Default to a near whole legacy transaction to penalize no implementation
    }

    /// Provides a shortcut to establish if the AMM can be used for trading
    /// If the market is active at all
    fn is_active(&self) -> bool {
        true
    }
}
