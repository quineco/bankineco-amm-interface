use solana_pubkey::Pubkey;
use vault_sdk::Vault;

/// Returns `(mint, decimals)` for the first base holding with a non-zero mint.
pub fn find_base_holding(vault: &Vault) -> Option<(Pubkey, u8)> {
    vault.holdings
        .iter()
        .find(|h| h.is_base == 1 && h.mint != [0u8; 32])
        .map(|h| (Pubkey::from(h.mint), h.decimals))
}

/// Returns `(price, decimals)` for the base holding matching `mint`, if any.
pub fn base_holding_for_mint(vault: &Vault, mint: &Pubkey) -> Option<(u64, u8)> {
    vault.holdings
        .iter()
        .find(|h| h.is_base == 1 && &Pubkey::from(h.mint) == mint)
        .map(|h| (h.price, h.decimals))
}

/// All base-asset holding mints — the vault's whitelisted deposit assets.
pub fn holding_mints(vault: &Vault) -> Vec<Pubkey> {
    vault.holdings
        .iter()
        .filter(|h| h.is_base == 1 && h.mint != [0u8; 32])
        .map(|h| Pubkey::from(h.mint))
        .collect()
}
