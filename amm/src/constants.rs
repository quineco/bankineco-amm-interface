use solana_pubkey::{pubkey, Pubkey};
use vault_sdk::{TokenProgram, Vault};

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
pub const CASH_MINT: Pubkey = pubkey!("CASHx9KJUStyftLFWGvEVf59SGeG9sh5FfcnZMVPCASH");

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
    /// Pyth price oracle account required by marginfi for the post-withdrawal health check.
    pub oracle: Pubkey,
}

// ---------------------------------------------------------------------------
// Per-mint Marginfi bank accounts (mainnet).
// Add new entries here as the vault gains additional whitelisted assets.
// ---------------------------------------------------------------------------

pub const MARGINFI_USDC: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB"),
    liquidity_vault: pubkey!("7jaiZR5Sk8hdYN9MxTpczTcwbWpb5WEoxSANuUwveuat"),
    liquidity_vault_auth: pubkey!("3uxNepDbmkDNq6JhRja5Z8QwbTrfmkKP8AKZV5chYDGG"),
    oracle: pubkey!("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX"),
};

pub const MARGINFI_USDT: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("HmpMfL8942u22htC4EMiandCNCtkoFtyytu6aTFZMoiD"),
    liquidity_vault: pubkey!("4tFJXnPFMWnqFBYBhd3FnBMWMM4PJJmqcCH4ZYrCFvNe"),
    liquidity_vault_auth: pubkey!("7sXoVHHR7SLRB9Cz3EHjSM3M1JBoqB6fVLSmjVYTATxB"),
    oracle: pubkey!("HT2PLQBcG5W5UrEKtNkLwNhXHMdWJv7WKQUQYmFtB9KL"),
};

pub const MARGINFI_PYUSD: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("8UEiPmgZHXXEDrqLS3oiTxQxTbeYTtPbeMBxAd2XGbpu"),
    liquidity_vault: pubkey!("ENnfVnYcbKZN57mUYCvsMiNUXZ8m2Dc1HETyfNDD66A8"),
    liquidity_vault_auth: pubkey!("582VxpQGLfUJRsdPYU2Q8dVLn1uxx9BuPMvtgwseB662"),
    oracle: pubkey!("9zXQxpYH3kYhtoybmZfUNNCRVuud7fY9jswTg1hLyT8k"),
};

pub const MARGINFI_USDG: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("Dj2CwMF3GM7mMT5hcyGXKuYSQ2kQ5zaVCkA1zX1qaTva"),
    liquidity_vault: pubkey!("5Euy1GJaWcF8BcZa2wbvKZq9ZU95anedL9TW416ZJNpK"),
    liquidity_vault_auth: pubkey!("J2RutaNtmw5Ri32iiZTexxNYHyDqJKbt6gVWCmv6hmnx"),
    oracle: pubkey!("5jaKPgAzTZZKfDPSfBtCgETFBXSQkgBDovdFoHAK6m3C"),
};

pub const MARGINFI_USDS: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("FDsf8sj6SoV313qrA91yms3u5b3P4hBxEPvanVs8LtJV"),
    liquidity_vault: pubkey!("26uoGkHSxBSL2oMcpdMZT7pss6wsiVCgFw6US58YZggd"),
    liquidity_vault_auth: pubkey!("2bqe5Zdkw7zsyWZ2prmWgPbr3LfMCYEDNSqizTw2BqKL"),
    oracle: pubkey!("DyYBBWEi9xZvgNAeMDCiFnmC1U9gqgVsJDXkL5WETpoX"),
};

pub const MARGINFI_CASH: MarginfiMintConfig = MarginfiMintConfig {
    bank: pubkey!("F4brCRJHx8epWah7p8Ace4ehutphxYZ1ctRq2LS3iiBh"),
    liquidity_vault: pubkey!("BogSuoRVycg5VSKSXi9YGjajhZ5uwCDA4HVPATEQXYVq"),
    liquidity_vault_auth: pubkey!("2nbp41Q7xN9wtomgoP3APtanSvqTg5PfYyNafPyABBp6"),
    oracle: pubkey!("6BfFmUuNJgQ5GCNj3V8YmgSLskFrQMWXa2N8i6ACsW5q"),
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

/// Maps a vault-sdk `TokenProgram` discriminant to its on-chain program id.
pub fn pubkey_for_token_program(tp: TokenProgram) -> Pubkey {
    Pubkey::from(tp.program_id())
}

/// Share-mint token program from `vault.mint_token_program`.
///
/// Falls back to legacy SPL Token if the stored discriminant is unrecognized
/// (e.g. pre-migration accounts that still have zeroed trailing padding).
pub fn share_token_program(vault: &Vault) -> Pubkey {
    vault
        .share_mint_token_program()
        .map(pubkey_for_token_program)
        .unwrap_or(anchor_spl::token::ID)
}

/// Token program for a vault holding mint, read from the holding's cached
/// `token_program` enum. Returns `None` if the mint is not a vault holding.
pub fn token_program_for_vault_mint(vault: &Vault, mint: &Pubkey) -> Option<Pubkey> {
    vault
        .holdings
        .iter()
        .find(|h| h.mint != [0u8; 32] && &Pubkey::from(h.mint) == mint)
        .and_then(|h| h.token_program_enum().ok())
        .map(pubkey_for_token_program)
}
