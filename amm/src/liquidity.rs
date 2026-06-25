use solana_pubkey::Pubkey;
use vault_sdk::Vault;

/// Breakdown of available liquidity for a given base-asset mint.
///
/// `local_amount`    — tokens sitting in the vault's on-chain ATA; withdrawable
///                     without any external CPI.
/// `external_amount` — tokens currently deployed to an external protocol
///                     (e.g. Marginfi); withdrawable only via
///                     `execute_withdraw_from_external`.
/// `total`           — `local_amount + external_amount`; the maximum a caller
///                     can request in a single withdraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawLiquidity {
    pub local_amount: u64,
    pub external_amount: u64,
    pub total: u64,
}

/// Returns the withdrawable liquidity for `mint` in `vault`, or `None` if the
/// mint is not a recognised base holding.
///
/// Both `local_amount` and `external_amount` come directly from the vault's
/// on-chain accounting; they are denominated in the mint's native token units
/// (i.e. they are raw u64 amounts, not share-price-adjusted values).
///
/// # Example
/// ```ignore
/// let liq = available_withdraw_liquidity(&vault, &USDC_MINT)?;
/// println!("local={} external={} total={}", liq.local_amount, liq.external_amount, liq.total);
/// ```
pub fn available_withdraw_liquidity(vault: &Vault, mint: &Pubkey) -> Option<WithdrawLiquidity> {
    vault
        .holdings
        .iter()
        .find(|h| h.is_base == 1 && &Pubkey::from(h.mint) == mint)
        .map(|h| WithdrawLiquidity {
            local_amount: h.local_amount,
            external_amount: h.external_amount,
            total: h.local_amount.saturating_add(h.external_amount),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;
    use vault_sdk::Vault;
    use crate::constants::USDC_MINT;

    #[test]
    fn returns_none_for_unknown_mint() {
        let vault = Vault::zeroed();
        assert!(available_withdraw_liquidity(&vault, &USDC_MINT).is_none());
    }

    #[test]
    fn returns_none_for_non_base_holding() {
        let mut vault = Vault::zeroed();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 0;
        vault.holdings[0].local_amount = 1_000_000;
        assert!(available_withdraw_liquidity(&vault, &USDC_MINT).is_none());
    }

    #[test]
    fn local_only() {
        let mut vault = Vault::zeroed();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].local_amount = 5_000_000;
        vault.holdings[0].external_amount = 0;

        let liq = available_withdraw_liquidity(&vault, &USDC_MINT).unwrap();
        assert_eq!(liq.local_amount, 5_000_000);
        assert_eq!(liq.external_amount, 0);
        assert_eq!(liq.total, 5_000_000);
    }

    #[test]
    fn external_only() {
        let mut vault = Vault::zeroed();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].local_amount = 0;
        vault.holdings[0].external_amount = 3_000_000;

        let liq = available_withdraw_liquidity(&vault, &USDC_MINT).unwrap();
        assert_eq!(liq.local_amount, 0);
        assert_eq!(liq.external_amount, 3_000_000);
        assert_eq!(liq.total, 3_000_000);
    }

    #[test]
    fn local_and_external() {
        let mut vault = Vault::zeroed();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].local_amount = 2_000_000;
        vault.holdings[0].external_amount = 8_000_000;

        let liq = available_withdraw_liquidity(&vault, &USDC_MINT).unwrap();
        assert_eq!(liq.local_amount, 2_000_000);
        assert_eq!(liq.external_amount, 8_000_000);
        assert_eq!(liq.total, 10_000_000);
    }

    #[test]
    fn saturating_add_on_overflow() {
        let mut vault = Vault::zeroed();
        vault.holdings[0].mint = USDC_MINT.to_bytes();
        vault.holdings[0].is_base = 1;
        vault.holdings[0].local_amount = u64::MAX;
        vault.holdings[0].external_amount = 1;

        let liq = available_withdraw_liquidity(&vault, &USDC_MINT).unwrap();
        assert_eq!(liq.total, u64::MAX);
    }
}
