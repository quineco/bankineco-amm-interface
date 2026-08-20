use vault_sdk::{TrancheKind, Vault, VaultTrancheState};
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
use liquidity::available_withdraw_liquidity;
use math::{
    calc_out_amount, calc_tranche_out_amount, required_input_amount,
    required_tranche_input_amount,
};

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
    /// Full tranche state, cached when tranching is enabled. Needed to price
    /// the junior / senior receipt mints, which are backed by their own class
    /// accounting rather than by the regular class NAV.
    tranche_state: Option<VaultTrancheState>,
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
            tranche_state: None,
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

    /// Cached tranche state, if the vault has tranching enabled.
    fn tranche(&self) -> Option<&VaultTrancheState> {
        if self.vault_state.tranching_enabled == 1 {
            self.tranche_state.as_ref()
        } else {
            None
        }
    }

    /// Which tranche class `mint` is the receipt mint for, if any.
    fn tranche_kind_for_mint(&self, mint: &Pubkey) -> Option<TrancheKind> {
        let tranche = self.tranche()?;
        for kind in [TrancheKind::Junior, TrancheKind::Senior] {
            let bytes = tranche.mint_for_kind(kind);
            if bytes != [0u8; 32] && Pubkey::from(bytes) == *mint {
                return Some(kind);
            }
        }
        None
    }

    /// Receipt mint of a tranche class.
    fn tranche_share_mint(&self, kind: TrancheKind) -> Option<Pubkey> {
        self.tranche()
            .map(|t| Pubkey::from(t.mint_for_kind(kind)))
            .filter(|m| *m != Pubkey::default())
    }

    /// Token program owning a tranche receipt mint. Falls back to legacy SPL
    /// Token when the stored discriminant is unrecognized, matching
    /// [`constants::share_token_program`].
    fn tranche_share_token_program(&self, kind: TrancheKind) -> Pubkey {
        self.tranche()
            .and_then(|t| t.mint_token_program_for_kind(kind).ok())
            .map(constants::pubkey_for_token_program)
            .unwrap_or(anchor_spl::token::ID)
    }

    /// Unstake fee for a tranche withdraw routed through this AMM.
    ///
    /// Jupiter can only route atomic swaps, so junior redemptions always take
    /// the instant path (`execute_tranche_withdraw`, `use_standard_fee = false`)
    /// and pay `early_unstake_fee_bps`. The cheaper `standard_unstake_fee_bps`
    /// is only reachable through the multi-transaction
    /// `request_junior_tranche_withdraw` / `fulfill_junior_tranche_withdraw`
    /// flow, which cannot be expressed as a swap. Senior is always fee-free.
    fn tranche_unstake_fee_bps(&self, kind: TrancheKind) -> u16 {
        match kind {
            TrancheKind::Junior => self
                .tranche()
                .map(|t| t.config.early_unstake_fee_bps)
                .unwrap_or(0),
            TrancheKind::Senior => 0,
        }
    }

    /// Whether a tranche withdraw of `asset_mint` can carry the external-liquidity
    /// CPI: the vault must have a Marginfi position and the mint must have a
    /// configured Marginfi bank to build the remaining accounts from.
    fn tranche_external_withdraw_source(&self, asset_mint: &Pubkey) -> Option<(Pubkey, u8)> {
        let position = self.marginfi_position?;
        constants::marginfi_config_for_mint(asset_mint)?;
        Some(position)
    }

    /// Liquidity a tranche withdraw can actually draw on for `asset_mint`.
    ///
    /// External liquidity only counts when the swap can supply the withdraw
    /// refs; otherwise `external_token_liquidity` returns 0 on-chain and the
    /// redemption is capped at the local ATA balance.
    fn tranche_withdraw_liquidity(&self, asset_mint: &Pubkey) -> u64 {
        let Some(liq) = available_withdraw_liquidity(&self.vault_state, asset_mint) else {
            return 0;
        };
        if self.tranche_external_withdraw_source(asset_mint).is_some() {
            liq.total
        } else {
            liq.local_amount
        }
    }

    /// `(class_supply, class_value)` for a tranche class — the accounting the
    /// receipt mint is priced against.
    fn tranche_class_accounting(&self, kind: TrancheKind) -> Option<(u64, u64)> {
        let class = self.tranche()?.class_accounting(kind);
        Some((class.total_supply, class.value))
    }

    /// Quote a swap where one side is a tranche receipt mint.
    ///
    /// Mirrors `vault::mint_burn::plan_tranche_deposit` / `plan_tranche_withdraw`:
    /// the asset is converted to accounting units, the fee is taken in
    /// accounting units, and shares are minted/burned against the class's own
    /// `(value, total_supply)` — not the regular class NAV.
    fn quote_tranche(&self, kind: TrancheKind, quote_params: &QuoteParams) -> Result<Quote> {
        let tranche_mint = self
            .tranche_share_mint(kind)
            .ok_or_else(|| anyhow!("Tranche class has no receipt mint"))?;
        let is_deposit = quote_params.output_mint == tranche_mint;
        let asset_mint = if is_deposit { quote_params.input_mint } else { quote_params.output_mint };
        if asset_mint == tranche_mint {
            return Err(anyhow!("Tranche receipt mint cannot be both sides of a swap"));
        }
        let (asset_price, asset_decimals) =
            base_holding_for_mint(&self.vault_state, &asset_mint).ok_or_else(|| {
                anyhow!("Mint {} is not a whitelisted base asset", asset_mint)
            })?;

        let (class_supply, class_value) = self
            .tranche_class_accounting(kind)
            .ok_or_else(|| anyhow!("Tranching is not enabled on this vault"))?;

        let fee_bps = if is_deposit {
            self.vault_state.config.fees.mint_fee_bps
        } else {
            self.tranche_unstake_fee_bps(kind)
        };

        let in_amount: u64 = if quote_params.swap_mode == SwapMode::ExactIn {
            quote_params.amount
        } else {
            required_tranche_input_amount(
                is_deposit,
                quote_params.amount,
                asset_price,
                asset_decimals,
                class_supply,
                class_value,
                fee_bps,
            )
            .ok_or_else(|| anyhow!("Tranche quote calculation overflow"))?
            .try_into()?
        };

        let (out_amount, fee_amount) = calc_tranche_out_amount(
            is_deposit,
            in_amount,
            asset_price,
            asset_decimals,
            class_supply,
            class_value,
            fee_bps,
        )
        .ok_or_else(|| anyhow!("Tranche quote calculation overflow"))?;

        // `plan_tranche_withdraw` rejects a redemption larger than the liquidity
        // reachable in the instruction. That is the vault's local ATA plus, when
        // the swap can carry the external-withdraw CPI, the external position —
        // see `external_token_liquidity`, which counts external liquidity only
        // if refs were supplied.
        if !is_deposit {
            let available = self.tranche_withdraw_liquidity(&asset_mint);
            if out_amount > available {
                return Err(anyhow!(
                    "Insufficient vault liquidity for tranche withdraw: need {out_amount}, have {available}"
                ));
            }
        }

        Ok(Quote {
            in_amount,
            out_amount,
            fee_amount,
            // Onchain tranche mint/unstake fees are taken in accounting units,
            // reported here in the deposit/withdraw asset.
            fee_mint: asset_mint,
            fee_pct: rust_decimal::Decimal::new(fee_bps.into(), 4),
        })
    }
}

impl BankinecoAmm {
    /// Account metas for `ExecuteTrancheDeposit` / `ExecuteTrancheWithdraw`.
    ///
    /// Order mirrors the vault program:
    ///   rust/programs/vault/src/instructions/vault/permissionless/execute_tranche_deposit.rs
    ///   rust/programs/vault/src/instructions/vault/permissionless/execute_tranche_withdraw.rs
    ///
    /// Both tranche instructions use `init_if_needed` ATAs, so unlike the
    /// regular-class instructions they take the associated-token and system
    /// programs, and `user` is a writable payer.
    /// Returns the metas and the `external_liquidity_source` Jupiter must encode:
    /// `Some(slot_index)` when the Marginfi remaining accounts were appended,
    /// `None` when the redemption is served locally and the instruction should
    /// be built with `(None, None)`.
    fn tranche_account_metas(
        &self,
        kind: TrancheKind,
        is_deposit: bool,
        user: &Pubkey,
        asset_mint: &Pubkey,
    ) -> Result<(Vec<AccountMeta>, Option<u8>)> {
        let tranche_share_mint = self
            .tranche_share_mint(kind)
            .ok_or_else(|| anyhow!("Tranche class has no receipt mint"))?;

        let vault_oracle = Pubkey::find_program_address(
            &[b"vault_oracle", self.vault.as_ref()],
            &PROGRAM_ID,
        ).0;
        let tranche_state = vault_tranche_pda(&self.vault);

        let asset_token_program = constants::token_program_for_vault_mint(
            &self.vault_state,
            asset_mint,
        )
        .ok_or_else(|| anyhow!("Asset mint is not a vault holding: {asset_mint}"))?;
        let share_token_program = self.tranche_share_token_program(kind);

        let user_asset_ata =
            get_associated_token_address_with_program_id(user, asset_mint, &asset_token_program);
        let vault_asset_ata = get_associated_token_address_with_program_id(
            &self.vault,
            asset_mint,
            &asset_token_program,
        );
        let user_tranche_share_ata = get_associated_token_address_with_program_id(
            user,
            &tranche_share_mint,
            &share_token_program,
        );

        let mut metas = vec![
            AccountMeta::new(*user, false),
            AccountMeta::new(self.vault, false),
            AccountMeta::new_readonly(vault_oracle, false),
            AccountMeta::new(tranche_state, false),
            AccountMeta::new_readonly(*asset_mint, false),
            AccountMeta::new(tranche_share_mint, false),
            AccountMeta::new(user_asset_ata, false),
            AccountMeta::new(vault_asset_ata, false),
        ];

        // Only the tranche deposit carries the legacy fee-vault accounts.
        if is_deposit {
            let fee_vault = Pubkey::find_program_address(
                &[b"VFEEVAULT", self.vault.as_ref()],
                &PROGRAM_ID,
            ).0;
            let fee_vault_ata = get_associated_token_address_with_program_id(
                &fee_vault,
                asset_mint,
                &asset_token_program,
            );
            metas.push(AccountMeta::new(fee_vault, false));
            metas.push(AccountMeta::new(fee_vault_ata, false));
        }

        metas.extend_from_slice(&[
            AccountMeta::new(user_tranche_share_ata, false),
            AccountMeta::new_readonly(asset_token_program, false),
            AccountMeta::new_readonly(share_token_program, false),
            AccountMeta::new_readonly(anchor_spl::associated_token::ID, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
        ]);

        // Withdrawals may need to top the vault ATA up from Marginfi first.
        // `execute_tranche_withdraw_from_external` consumes the same 9
        // remaining accounts as `execute_withdraw_from_external`, so the CPI
        // refs and account pool are shared verbatim.
        let external_liquidity_source = if is_deposit {
            None
        } else {
            self.tranche_external_withdraw_source(asset_mint).map(
                |(marginfi_account, slot_index)| {
                    let mint_config = constants::marginfi_config_for_mint(asset_mint)
                        .expect("checked by tranche_external_withdraw_source");
                    metas.extend(marginfi_withdraw_remaining_accounts(
                        marginfi_account,
                        self.vault,
                        vault_asset_ata,
                        mint_config,
                        asset_token_program,
                    ));
                    slot_index
                },
            )
        };

        Ok((metas, external_liquidity_source))
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
        let mut mints = vec![self.share_mint];
        // Tranche receipt mints are quotable against the same asset holdings as
        // the regular share mint, via execute_tranche_deposit / _withdraw.
        for kind in [TrancheKind::Junior, TrancheKind::Senior] {
            if let Some(mint) = self.tranche_share_mint(kind) {
                mints.push(mint);
            }
        }
        if holdings.is_empty() {
            mints.push(self.base_asset_mint);
        } else {
            mints.extend(holdings);
        }
        mints
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
        if self.vault_state.tranching_enabled == 1 {
            let tranche_pda = vault_tranche_pda(&self.vault);
            let tranche_data = try_get_account_data(account_map, &tranche_pda)?;
            let tranche = VaultTrancheState::from_account_data(tranche_data)
                .map_err(|e| anyhow!("Tranche load error: {:?}", e))?;
            self.tranche_value = tranche.junior.value.saturating_add(tranche.senior.value);
            self.tranche_state = Some(tranche);
        } else {
            self.tranche_value = 0;
            self.tranche_state = None;
        }
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        if let Some(kind) = self
            .tranche_kind_for_mint(&quote_params.input_mint)
            .or_else(|| self.tranche_kind_for_mint(&quote_params.output_mint))
        {
            return self.quote_tranche(kind, quote_params);
        }

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

        // Tranche legs use their own instructions and account layout.
        if let Some(kind) = self
            .tranche_kind_for_mint(source_mint)
            .or_else(|| self.tranche_kind_for_mint(destination_mint))
        {
            let is_deposit = self.tranche_kind_for_mint(destination_mint) == Some(kind);
            let asset_mint = if is_deposit { source_mint } else { destination_mint };
            let (account_metas, external_liquidity_source) = self.tranche_account_metas(
                kind,
                is_deposit,
                token_transfer_authority,
                asset_mint,
            )?;
            // TODO: Jupiter needs new Swap variants for the tranche legs:
            //   - BankinecoTrancheDeposit { kind: u8 }: execute_tranche_deposit.
            //   - BankinecoTrancheWithdrawFromExternal { kind: u8, external_liquidity_source: u8 }:
            //     calls execute_tranche_withdraw_from_external; u8::MAX = no
            //     external position → encode (None, None).
            // `kind` is 0 = junior, 1 = senior; junior withdraws always take the
            // instant path and pay early_unstake_fee_bps.
            let _ = external_liquidity_source.unwrap_or(u8::MAX);
            return Ok(SwapAndAccountMetas { swap: Swap::TokenSwap, account_metas });
        }

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

        // ATAs — token programs come from vault state (share mint enum + holding enum).
        let asset_token_program = constants::token_program_for_vault_mint(
            &self.vault_state,
            asset_mint,
        )
        .ok_or_else(|| anyhow!("Asset mint is not a vault holding: {asset_mint}"))?;
        let share_token_program = constants::share_token_program(&self.vault_state);
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
        // When the vault has a Marginfi position, append the 10 remaining_accounts
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
                        asset_token_program,
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
        // Accounts vary with vault state: marginfi position adds 10 remaining accounts,
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
        // Marginfi remaining accounts (10, see marginfi_withdraw_remaining_accounts)
        // are only appended on withdrawals, but we report the max so Jupiter can
        // allocate the worst-case account list.
        let marginfi = if self.marginfi_position.is_some() { 10 } else { 0 };
        let regular = 12 + tranche + marginfi;
        // Tranche legs: deposit is 15 fixed accounts; withdraw is 13 fixed plus
        // the 10 Marginfi remaining accounts when external liquidity is routable.
        let tranche_leg = if self.tranche().is_some() {
            15.max(13 + marginfi)
        } else {
            0
        };
        regular.max(tranche_leg)
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

#[cfg(test)]
mod tranche_tests {
    use super::*;
    use bytemuck::{bytes_of, Zeroable};
    use jupiter_amm_interface::{QuoteParams, SwapMode, SwapParams};
    use solana_sdk::account::Account;
    use vault_sdk::{
        TokenProgram, Vault, VaultTrancheState, VAULT_DISCRIMINATOR,
        VAULT_TRANCHE_STATE_DISCRIMINATOR,
    };

    const JUNIOR_MINT: Pubkey = solana_pubkey::pubkey!("JUnwqhFRvJ1U4wJ4v1Cq5vN3jvKk1vhFtvKmvMe2GRz");
    const SENIOR_MINT: Pubkey = solana_pubkey::pubkey!("SEnwqhFRvJ1U4wJ4v1Cq5vN3jvKk1vhFtvKmvMe2GRz");

    const VAULT_KEY: Pubkey = TEST_VAULT;

    /// Junior class: value 2.0e12, supply 1.0e12 → 2.00 per junior share.
    /// Senior class: value 0.5e12, supply 0.5e12 → 1.00 per senior share.
    /// Early-unstake fee: 100 bps. Standard (non-instant) fee: 10 bps.
    fn make_tranche_state() -> VaultTrancheState {
        let mut t = VaultTrancheState::zeroed();
        t.vault = VAULT_KEY.to_bytes();
        t.config.junior_mint = JUNIOR_MINT.to_bytes();
        t.config.senior_mint = SENIOR_MINT.to_bytes();
        t.config.junior_mint_token_program = TokenProgram::Spl as u8;
        t.config.senior_mint_token_program = TokenProgram::Spl as u8;
        t.config.early_unstake_fee_bps = 100;
        t.config.standard_unstake_fee_bps = 10;
        t.junior.value = 2_000_000_000_000;
        t.junior.total_supply = 1_000_000_000_000;
        t.senior.value = 500_000_000_000;
        t.senior.total_supply = 500_000_000_000;
        t
    }

    fn make_tranched_vault() -> Vault {
        let mut vault = Vault::zeroed();
        vault.mint_decimals = 6;
        vault.mint = USD_STAR_MINT.to_bytes();
        vault.tranching_enabled = 1;
        vault.accounting.total_mint_supply = 1_000_000_000_000;
        vault.accounting.tvl = 3_500_000_000_000; // 2.5e12 tranche + 1.0e12 regular
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].decimals = 6;
        vault.holdings[0].price = 1_000_000;
        vault.holdings[0].token_program = TokenProgram::Spl as u8;
        vault.holdings[0].local_amount = 1_000_000_000_000;
        vault
    }

    /// AMM with tranching enabled and the tranche state loaded via `update`.
    fn make_tranched_amm() -> BankinecoAmm {
        let vault = make_tranched_vault();
        let tranche = make_tranche_state();
        let mut amm = BankinecoAmm::new(VAULT_KEY, vault);

        let mut vault_data = Vec::new();
        vault_data.extend_from_slice(&VAULT_DISCRIMINATOR);
        vault_data.extend_from_slice(bytes_of(&vault));
        let mut tranche_data = Vec::new();
        tranche_data.extend_from_slice(&VAULT_TRANCHE_STATE_DISCRIMINATOR);
        tranche_data.extend_from_slice(bytes_of(&tranche));

        let account_map = AccountMap::from_iter([
            (VAULT_KEY, Account { data: vault_data, ..Account::default() }),
            (
                vault_tranche_pda(&VAULT_KEY),
                Account { data: tranche_data, ..Account::default() },
            ),
        ]);
        amm.update(&account_map).unwrap();
        amm
    }

    fn quote_params(input: Pubkey, output: Pubkey, amount: u64, mode: SwapMode) -> QuoteParams {
        QuoteParams { amount, input_mint: input, output_mint: output, swap_mode: mode }
    }

    // -----------------------------------------------------------------------
    // Reserve mints / routing
    // -----------------------------------------------------------------------

    #[test]
    fn reserve_mints_include_tranche_mints() {
        let mints = make_tranched_amm().get_reserve_mints();
        assert!(mints.contains(&JUNIOR_MINT));
        assert!(mints.contains(&SENIOR_MINT));
        assert!(mints.contains(&USD_STAR_MINT));
        assert!(mints.contains(&USDC_MINT));
    }

    #[test]
    fn reserve_mints_exclude_tranche_mints_when_disabled() {
        let mut vault = make_tranched_vault();
        vault.tranching_enabled = 0;
        let amm = BankinecoAmm::new(VAULT_KEY, vault);
        let mints = amm.get_reserve_mints();
        assert!(!mints.contains(&JUNIOR_MINT));
        assert!(!mints.contains(&SENIOR_MINT));
    }

    #[test]
    fn tranche_kind_lookup() {
        let amm = make_tranched_amm();
        assert_eq!(amm.tranche_kind_for_mint(&JUNIOR_MINT), Some(TrancheKind::Junior));
        assert_eq!(amm.tranche_kind_for_mint(&SENIOR_MINT), Some(TrancheKind::Senior));
        assert_eq!(amm.tranche_kind_for_mint(&USD_STAR_MINT), None);
        assert_eq!(amm.tranche_kind_for_mint(&USDC_MINT), None);
    }

    // -----------------------------------------------------------------------
    // Deposit quotes
    // -----------------------------------------------------------------------

    #[test]
    fn junior_deposit_prices_against_junior_class() {
        // Junior NAV = 2.00 → 100 USDC mints 50 junior shares.
        let amm = make_tranched_amm();
        let quote = amm
            .quote(&quote_params(USDC_MINT, JUNIOR_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap();
        assert_eq!(quote.out_amount, 50_000_000);
        assert_eq!(quote.fee_amount, 0);
    }

    #[test]
    fn senior_deposit_prices_against_senior_class() {
        // Senior NAV = 1.00 → 100 USDC mints 100 senior shares.
        let amm = make_tranched_amm();
        let quote = amm
            .quote(&quote_params(USDC_MINT, SENIOR_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap();
        assert_eq!(quote.out_amount, 100_000_000);
    }

    #[test]
    fn tranche_deposit_charges_mint_fee() {
        let mut vault = make_tranched_vault();
        vault.config.fees.mint_fee_bps = 50; // 0.50%
        let tranche = make_tranche_state();
        let mut amm = BankinecoAmm::new(VAULT_KEY, vault);
        amm.vault_state = vault;
        amm.tranche_state = Some(tranche);
        amm.tranche_value = 2_500_000_000_000;

        let quote = amm
            .quote(&quote_params(USDC_MINT, JUNIOR_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap();
        // 100 USDC → 0.5 USDC fee, 99.5 net accounting / 2.00 = 49.75 shares
        assert_eq!(quote.fee_amount, 500_000);
        assert_eq!(quote.out_amount, 49_750_000);
    }

    #[test]
    fn tranche_deposit_into_empty_class_mints_one_to_one() {
        let mut tranche = make_tranche_state();
        tranche.junior.value = 0;
        tranche.junior.total_supply = 0;
        let mut amm = make_tranched_amm();
        amm.tranche_state = Some(tranche);

        let quote = amm
            .quote(&quote_params(USDC_MINT, JUNIOR_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap();
        assert_eq!(quote.out_amount, 100_000_000);
    }

    // -----------------------------------------------------------------------
    // Withdraw quotes
    // -----------------------------------------------------------------------

    #[test]
    fn junior_withdraw_pays_early_unstake_fee() {
        // 50 junior shares × 2.00 = 100 gross; 100 bps early-unstake fee = 1 USDC.
        let amm = make_tranched_amm();
        let quote = amm
            .quote(&quote_params(JUNIOR_MINT, USDC_MINT, 50_000_000, SwapMode::ExactIn))
            .unwrap();
        assert_eq!(quote.fee_amount, 1_000_000);
        assert_eq!(quote.out_amount, 99_000_000);
    }

    #[test]
    fn junior_withdraw_ignores_standard_unstake_fee() {
        // The routable path is always instant redemption, so the cheaper
        // standard fee must never be quoted.
        let amm = make_tranched_amm();
        assert_eq!(amm.tranche_unstake_fee_bps(TrancheKind::Junior), 100);
        let quote = amm
            .quote(&quote_params(JUNIOR_MINT, USDC_MINT, 50_000_000, SwapMode::ExactIn))
            .unwrap();
        // 10 bps (standard) would leave 99.9 USDC.
        assert_ne!(quote.out_amount, 99_900_000);
    }

    #[test]
    fn senior_withdraw_is_fee_free() {
        let amm = make_tranched_amm();
        let quote = amm
            .quote(&quote_params(SENIOR_MINT, USDC_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap();
        assert_eq!(quote.fee_amount, 0);
        assert_eq!(quote.out_amount, 100_000_000);
    }

    /// Stand-in for the vault's Marginfi user account.
    const MARGINFI_ACCOUNT: Pubkey = Pubkey::new_from_array([7u8; 32]);

    /// Mark external_liquidity slot 0 as a Marginfi position.
    fn with_marginfi_position(vault: &mut Vault) {
        vault.external_liquidity[0].data[0] = 1; // ExternalLiquiditySource::Marginfi
        vault.external_liquidity[0].data[8..40]
            .copy_from_slice(&MARGINFI_ACCOUNT.to_bytes());
    }

    #[test]
    fn tranche_withdraw_capped_at_local_without_external_source() {
        // No Marginfi position: the instruction is built with (None, None) and
        // `external_token_liquidity` contributes 0 on-chain.
        let mut vault = make_tranched_vault();
        vault.holdings[0].local_amount = 10_000_000; // 10 USDC local
        vault.holdings[0].external_amount = 1_000_000_000_000;
        let mut amm = make_tranched_amm();
        amm.vault_state = vault;
        amm.refresh_from_state();

        let err = amm
            .quote(&quote_params(SENIOR_MINT, USDC_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap_err();
        assert!(err.to_string().contains("Insufficient vault liquidity"));

        // Within local liquidity it still quotes.
        assert!(amm
            .quote(&quote_params(SENIOR_MINT, USDC_MINT, 5_000_000, SwapMode::ExactIn))
            .is_ok());
    }

    #[test]
    fn tranche_withdraw_counts_external_liquidity_when_routable() {
        // With a Marginfi position the swap carries the withdraw refs, so the
        // external balance is reachable.
        let mut vault = make_tranched_vault();
        vault.holdings[0].local_amount = 10_000_000;
        vault.holdings[0].external_amount = 1_000_000_000_000;
        with_marginfi_position(&mut vault);
        let mut amm = make_tranched_amm();
        amm.vault_state = vault;
        amm.refresh_from_state();

        let quote = amm
            .quote(&quote_params(SENIOR_MINT, USDC_MINT, 100_000_000, SwapMode::ExactIn))
            .unwrap();
        assert_eq!(quote.out_amount, 100_000_000);
    }

    #[test]
    fn tranche_withdraw_still_capped_at_total_liquidity() {
        let mut vault = make_tranched_vault();
        vault.holdings[0].local_amount = 10_000_000;
        vault.holdings[0].external_amount = 20_000_000;
        with_marginfi_position(&mut vault);
        let mut amm = make_tranched_amm();
        amm.vault_state = vault;
        amm.refresh_from_state();

        // 40 senior shares → 40 USDC > 30 USDC total.
        let err = amm
            .quote(&quote_params(SENIOR_MINT, USDC_MINT, 40_000_000, SwapMode::ExactIn))
            .unwrap_err();
        assert!(err.to_string().contains("Insufficient vault liquidity"));
        assert!(amm
            .quote(&quote_params(SENIOR_MINT, USDC_MINT, 30_000_000, SwapMode::ExactIn))
            .is_ok());
    }

    #[test]
    fn tranche_withdraw_ignores_external_for_unbanked_mint() {
        // A Marginfi position exists, but the asset has no configured bank, so
        // the remaining accounts cannot be built and external is unreachable.
        let mut vault = make_tranched_vault();
        vault.holdings[0].mint = USD_STAR_MINT.to_bytes();
        vault.holdings[0].local_amount = 1_000_000;
        vault.holdings[0].external_amount = 1_000_000_000_000;
        with_marginfi_position(&mut vault);
        let mut amm = make_tranched_amm();
        amm.vault_state = vault;
        amm.refresh_from_state();

        let err = amm
            .quote(&quote_params(SENIOR_MINT, USD_STAR_MINT, 50_000_000, SwapMode::ExactIn))
            .unwrap_err();
        assert!(err.to_string().contains("Insufficient vault liquidity"));
    }

    #[test]
    fn tranche_withdraw_appends_marginfi_remaining_accounts() {
        let mut vault = make_tranched_vault();
        with_marginfi_position(&mut vault);
        let mut amm = make_tranched_amm();
        amm.vault_state = vault;
        amm.refresh_from_state();

        let user = Pubkey::new_unique();
        let metas = amm
            .get_swap_and_account_metas(&swap_params(SENIOR_MINT, USDC_MINT, user))
            .unwrap()
            .account_metas;

        // 13 fixed + 10 Marginfi remaining accounts.
        assert_eq!(metas.len(), 23);
        assert_eq!(metas[13].pubkey, MARGINFI_PROGRAM_ID);
        assert_eq!(metas[15].pubkey, MARGINFI_ACCOUNT);
        assert_eq!(metas[17].pubkey, MARGINFI_USDC.bank);
        assert_eq!(
            metas[18].pubkey,
            get_associated_token_address_with_program_id(
                &VAULT_KEY,
                &USDC_MINT,
                &anchor_spl::token::ID,
            )
        );
        assert_eq!(metas[23 - 1].pubkey, MARGINFI_USDC.oracle);
        assert!(amm.get_accounts_len() >= 23);
    }

    #[test]
    fn tranche_deposit_never_appends_marginfi_accounts() {
        let mut vault = make_tranched_vault();
        with_marginfi_position(&mut vault);
        let mut amm = make_tranched_amm();
        amm.vault_state = vault;
        amm.refresh_from_state();

        let metas = amm
            .get_swap_and_account_metas(&swap_params(USDC_MINT, JUNIOR_MINT, Pubkey::new_unique()))
            .unwrap()
            .account_metas;
        assert_eq!(metas.len(), 15);
    }

    // -----------------------------------------------------------------------
    // ExactOut round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn tranche_deposit_exact_out_roundtrip() {
        let amm = make_tranched_amm();
        let exact_out = amm
            .quote(&quote_params(USDC_MINT, JUNIOR_MINT, 50_000_000, SwapMode::ExactOut))
            .unwrap();
        let exact_in = amm
            .quote(&quote_params(USDC_MINT, JUNIOR_MINT, exact_out.in_amount, SwapMode::ExactIn))
            .unwrap();
        assert!(exact_in.out_amount >= 50_000_000);
    }

    #[test]
    fn tranche_withdraw_exact_out_roundtrip() {
        let amm = make_tranched_amm();
        let exact_out = amm
            .quote(&quote_params(JUNIOR_MINT, USDC_MINT, 99_000_000, SwapMode::ExactOut))
            .unwrap();
        let exact_in = amm
            .quote(&quote_params(JUNIOR_MINT, USDC_MINT, exact_out.in_amount, SwapMode::ExactIn))
            .unwrap();
        assert!(exact_in.out_amount >= 99_000_000);
    }

    // -----------------------------------------------------------------------
    // Account metas
    // -----------------------------------------------------------------------

    static JUPITER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

    fn swap_params(source: Pubkey, destination: Pubkey, user: Pubkey) -> SwapParams<'static, 'static> {
        SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 0,
            out_amount: 0,
            source_mint: source,
            destination_mint: destination,
            source_token_account: Pubkey::default(),
            destination_token_account: Pubkey::default(),
            token_transfer_authority: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &JUPITER_PROGRAM_ID,
            missing_dynamic_accounts_as_default: false,
        }
    }

    #[test]
    fn tranche_deposit_account_metas_match_onchain_order() {
        let amm = make_tranched_amm();
        let user = Pubkey::new_unique();
        let metas = amm
            .get_swap_and_account_metas(&swap_params(USDC_MINT, JUNIOR_MINT, user))
            .unwrap()
            .account_metas;

        let vault_oracle =
            Pubkey::find_program_address(&[b"vault_oracle", VAULT_KEY.as_ref()], &PROGRAM_ID).0;
        let fee_vault =
            Pubkey::find_program_address(&[b"VFEEVAULT", VAULT_KEY.as_ref()], &PROGRAM_ID).0;
        let spl = anchor_spl::token::ID;
        let ata = |owner: &Pubkey, mint: &Pubkey| {
            get_associated_token_address_with_program_id(owner, mint, &spl)
        };

        let expected = vec![
            user,
            VAULT_KEY,
            vault_oracle,
            vault_tranche_pda(&VAULT_KEY),
            USDC_MINT,
            JUNIOR_MINT,
            ata(&user, &USDC_MINT),
            ata(&VAULT_KEY, &USDC_MINT),
            fee_vault,
            ata(&fee_vault, &USDC_MINT),
            ata(&user, &JUNIOR_MINT),
            spl,
            spl,
            anchor_spl::associated_token::ID,
            solana_sdk::system_program::ID,
        ];
        assert_eq!(metas.iter().map(|m| m.pubkey).collect::<Vec<_>>(), expected);
        // user is the init_if_needed payer, and the tranche state is mutated.
        assert!(metas[0].is_writable);
        assert!(metas[3].is_writable);
        assert!(!metas[2].is_writable);
    }

    #[test]
    fn tranche_withdraw_account_metas_omit_fee_vault() {
        let amm = make_tranched_amm();
        let user = Pubkey::new_unique();
        let metas = amm
            .get_swap_and_account_metas(&swap_params(SENIOR_MINT, USDC_MINT, user))
            .unwrap()
            .account_metas;

        let fee_vault =
            Pubkey::find_program_address(&[b"VFEEVAULT", VAULT_KEY.as_ref()], &PROGRAM_ID).0;
        assert_eq!(metas.len(), 13);
        assert!(!metas.iter().any(|m| m.pubkey == fee_vault));
        assert_eq!(metas[5].pubkey, SENIOR_MINT);
        assert_eq!(metas[8].pubkey, get_associated_token_address_with_program_id(
            &user,
            &SENIOR_MINT,
            &anchor_spl::token::ID,
        ));
    }

    #[test]
    fn accounts_len_covers_widest_tranche_leg() {
        assert!(make_tranched_amm().get_accounts_len() >= 15);
    }

    #[test]
    fn regular_share_swap_still_uses_regular_path_when_tranched() {
        let amm = make_tranched_amm();
        let user = Pubkey::new_unique();
        let metas = amm
            .get_swap_and_account_metas(&swap_params(USDC_MINT, USD_STAR_MINT, user))
            .unwrap()
            .account_metas;
        // Regular deposit: 12 fixed + vault_tranche_state, no ATA/system programs.
        assert_eq!(metas.len(), 13);
        assert!(!metas.iter().any(|m| m.pubkey == solana_sdk::system_program::ID));
    }
}
