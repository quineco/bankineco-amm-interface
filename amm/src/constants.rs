use solana_pubkey::{ pubkey, Pubkey };

// pub const PROD_PROGRAM_ID: Pubkey = pubkey!("save8RQVPMWNTzU18t3GBvBkN9hT7jsGjiCQ28FpD9H");
pub const PROGRAM_ID: Pubkey = pubkey!("6HyT8NQDpXY5wGkvX7haQVJ5nGUBVXQSkaT6Nf7fbsuJ");

// pub const PROD_VAULT: Pubkey = pubkey!("ECJGrTZ6QYMEwiEAnL4oReWF126uc22e9Lojy9qyCjHT"); // vault_id = 0
pub const TEST_VAULT: Pubkey = pubkey!("Bzj2KQqSaUB9QAWmdz1r4HttLjtGi5UQFTJrLx1B5hYK");

pub const USD_STAR_MINT: Pubkey = pubkey!("star9agSpjiFe3M49B3RniVU4CMBBEK3Qnaqn3RGiFM");

// Supported deposit/withdraw asset mints.
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
pub const PYUSD_MINT: Pubkey = pubkey!("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo");
pub const USDG_MINT: Pubkey = pubkey!("GbMiMDYFX9sVMNQFmqmgKMhWfBvPNTJjxz4YubDKtDKE");
pub const USDS_MINT: Pubkey = pubkey!("USDSwr9ApdHk5bvJKMjzff41FfuX8bSxdKcR81vTwcA");
pub const CASH_MINT: Pubkey = pubkey!("CASHVDm2wsJXfhj6VWxb7GiMdoLc17Du7paH4bNr5woT");

pub const MARGINFI_PROGRAM_ID: Pubkey = pubkey!("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");
pub const MAIN_MARGINFI_GROUP: Pubkey = pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8");

/// Per-mint Marginfi bank configuration.
///
/// Seeds (both derived from the Marginfi program with `bank` as the seed):
///   bank_liquidity_vault_authority : ["liquidity_vault_auth", bank]
///   bank_liquidity_vault            : ["liquidity_vault",      bank]
pub struct MarginfiMintConfig {
    pub bank: Pubkey,
    pub liquidity_vault: Pubkey,
    pub liquidity_vault_auth: Pubkey,
}

// ---------------------------------------------------------------------------
// Per-mint Marginfi bank accounts (mainnet).
// Add new entries here as the vault gains additional whitelisted assets.
// ---------------------------------------------------------------------------

pub const MARGINFI_USDC: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB"),
    liquidity_vault: pubkey!("7jaiZR5Sk8hdYN9MxTpczTcwbWpb5WEoxSANuUwveuat"),
    liquidity_vault_auth: pubkey!("3uxNepDbmkDNq6JhRja5Z8QwbTrfmkKP8AKZV5chYDGG"),
};

pub const MARGINFI_USDT: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("HmpMfL8942u22htC4EMiandCNCtkoFtyytu6aTFZMoiD"),
    liquidity_vault: pubkey!("4tFJXnPFMWnqFBYBhd3FnBMWMM4PJJmqcCH4ZYrCFvNe"),
    liquidity_vault_auth: pubkey!("7sXoVHHR7SLRB9Cz3EHjSM3M1JBoqB6fVLSmjVYTATxB"),
};

// USDG and CASH have no Marginfi bank on mainnet (not listed in Marginfi's
// bank registry). Using the system program pubkey as an obvious unset sentinel.
const _UNSET: Pubkey = pubkey!("11111111111111111111111111111111");

pub const MARGINFI_PYUSD: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("8UEiPmgZHXXEDrqLS3oiTxQxTbeYTtPbeMBxAd2XGbpu"),
    liquidity_vault: pubkey!("ENnfVnYcbKZN57mUYCvsMiNUXZ8m2Dc1HETyfNDD66A8"),
    liquidity_vault_auth: pubkey!("582VxpQGLfUJRsdPYU2Q8dVLn1uxx9BuPMvtgwseB662"),
};

pub const MARGINFI_USDG: MarginfiMintConfig = MarginfiMintConfig {
    bank: _UNSET,
    liquidity_vault: _UNSET,
    liquidity_vault_auth: _UNSET,
};

pub const MARGINFI_USDS: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("FDsf8sj6SoV313qrA91yms3u5b3P4hBxEPvanVs8LtJV"),
    liquidity_vault: pubkey!("26uoGkHSxBSL2oMcpdMZT7pss6wsiVCgFw6US58YZggd"),
    liquidity_vault_auth: pubkey!("2bqe5Zdkw7zsyWZ2prmWgPbr3LfMCYEDNSqizTw2BqKL"),
};

pub const MARGINFI_CASH: MarginfiMintConfig = MarginfiMintConfig {
    bank: _UNSET,
    liquidity_vault: _UNSET,
    liquidity_vault_auth: _UNSET,
};

/// Look up the Marginfi bank config for a given asset mint.
///
/// Returns `None` for mints with no Marginfi bank configured.
pub fn marginfi_config_for_mint(mint: &Pubkey) -> Option<&'static MarginfiMintConfig> {
    match *mint {
        USDC_MINT  => Some(&MARGINFI_USDC),
        USDT_MINT  => Some(&MARGINFI_USDT),
        PYUSD_MINT => Some(&MARGINFI_PYUSD),
        USDG_MINT  => Some(&MARGINFI_USDG),
        USDS_MINT  => Some(&MARGINFI_USDS),
        CASH_MINT  => Some(&MARGINFI_CASH),
        _          => None,
    }
}
