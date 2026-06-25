use solana_pubkey::{ pubkey, Pubkey };

pub const PROGRAM_ID: Pubkey = pubkey!("save8RQVPMWNTzU18t3GBvBkN9hT7jsGjiCQ28FpD9H");
pub const USD_STAR_MINT: Pubkey = pubkey!("star9agSpjiFe3M49B3RniVU4CMBBEK3Qnaqn3RGiFM");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

// Marginfi program and mainnet accounts.
// Seeds (both derived from the Marginfi program):
//   bank_liquidity_vault_authority : ["liquidity_vault_auth", bank]
//   bank_liquidity_vault            : ["liquidity_vault",      bank]
pub const MARGINFI_PROGRAM_ID: Pubkey = pubkey!("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");
pub const MAIN_MARGINFI_GROUP: Pubkey = pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8");
pub const MAIN_MARGINFI_BANK: Pubkey = pubkey!("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB");
pub const MAIN_MARGINFI_LIQUIDITY_VAULT: Pubkey =
    pubkey!("7jaiZR5Sk8hdYN9MxTpczTcwbWpb5WEoxSANuUwveuat");
pub const MAIN_MARGINFI_LIQUIDITY_VAULT_AUTH: Pubkey =
    pubkey!("3uxNepDbmkDNq6JhRja5Z8QwbTrfmkKP8AKZV5chYDGG");
