use solana_pubkey::Pubkey;
use solana_sdk::instruction::AccountMeta;
use vault_sdk::Vault;
use crate::constants::{
    MARGINFI_PROGRAM_ID, MAIN_MARGINFI_GROUP, MarginfiMintConfig,
};

/// Scan the vault's external_liquidity slots for a Marginfi position.
///
/// Returns `(marginfi_user_account, slot_index)` for the first active slot,
/// or `None` if the vault has no Marginfi liquidity deployed.
///
/// Layout of `MarginfiExternalLiquidityData` (common::state::external_liquidity):
///   [0]      source discriminant (1 = Marginfi)
///   [1..8]   _padding1 (7 bytes)
///   [8..40]  user_account (Pubkey, 32 bytes)
pub fn marginfi_position_from_vault(vault: &Vault) -> Option<(Pubkey, u8)> {
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

fn borsh_vec(v: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(v.len() as u32).to_le_bytes());
    out.extend_from_slice(v);
}

/// Build the serialized `InstructionRefs` bytes for a single Marginfi withdraw CPI.
///
/// Wire format (`InstructionRefs` → `CpiRefs` → `CpiMapping`, all borsh):
///   CpiMapping.indices  : [0,1,2,3,4,5,6,7,8,9]  — 10 remaining_accounts in order
///   CpiMapping.lengths  : [10]                    — one CPI, 10 accounts
///   CpiRefs.types       : [3]                     — CpiType::MARGINFI_WITHDRAW
///   CpiRefs.args        : [0xFF, 0xFF]             — Skip sentinel; amount computed on-chain
///   InstructionRefs.tracked: []
///
/// References:
///   bankineco/rust/crates/common/src/accounts/refs.rs        (InstructionRefs)
///   bankineco/rust/crates/common/src/cpi/refs.rs             (CpiRefs)
///   bankineco/rust/crates/common/src/accounts/cpi.rs         (CpiMapping)
///   bankineco/rust/crates/common/src/cpi/registry.rs         (CpiType::MARGINFI_WITHDRAW = 3)
pub fn build_marginfi_withdraw_instruction_refs() -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    borsh_vec(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], &mut out); // CpiMapping.indices
    borsh_vec(&[10], &mut out);                             // CpiMapping.lengths
    borsh_vec(&[3], &mut out);                              // CpiRefs.types (MARGINFI_WITHDRAW)
    borsh_vec(&[0xFF, 0xFF], &mut out);                     // CpiRefs.args  (Skip sentinel)
    borsh_vec(&[], &mut out);                               // InstructionRefs.tracked
    out
}

/// Build the remaining `AccountMeta`s for the Marginfi withdraw CPI.
///
/// These are appended after the fixed `execute_withdraw_from_external` accounts.
/// The account order matches the `CpiMapping.indices` in
/// [`build_marginfi_withdraw_instruction_refs`]:
///
///   [0] Marginfi program              (readonly)
///   [1] marginfi_group                (writable)
///   [2] marginfi_account              (writable)  ← vault's Marginfi user account
///   [3] vault PDA                     (readonly)  ← signing authority (PDA-signs internally)
///   [4] bank                          (writable)  ← mint-specific Marginfi bank
///   [5] vault_asset_ata               (writable)  ← withdrawal destination
///   [6] bank_liquidity_vault_auth     (readonly)  ← mint-specific
///   [7] bank_liquidity_vault          (writable)  ← mint-specific
///   [8] token_program                 (readonly)
///   [9] oracle                        (readonly)  ← required for post-withdrawal health check
pub fn marginfi_withdraw_remaining_accounts(
    marginfi_account: Pubkey,
    vault: Pubkey,
    vault_asset_ata: Pubkey,
    mint_config: &MarginfiMintConfig,
    token_program: Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(MARGINFI_PROGRAM_ID, false),
        AccountMeta::new(MAIN_MARGINFI_GROUP, false),
        AccountMeta::new(marginfi_account, false),
        AccountMeta::new_readonly(vault, false),
        AccountMeta::new(mint_config.bank, false),
        AccountMeta::new(vault_asset_ata, false),
        AccountMeta::new_readonly(mint_config.liquidity_vault_auth, false),
        AccountMeta::new(mint_config.liquidity_vault, false),
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(mint_config.oracle, false),
    ]
}
