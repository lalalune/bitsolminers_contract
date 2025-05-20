use anchor_lang::prelude::*;

#[error_code]
pub enum BitSolError {
    #[msg("Wallet age is less than 7 days")]
    WalletTooNew,
    #[msg("Facility power capacity exceeded")]
    PowerCapacityExceeded,
    #[msg("Facility miner capacity exceeded")]
    MinerCapacityExceeded,
    #[msg("Insufficient $BITSOL balance")]
    InsufficientBits,
    #[msg("Insufficient lamports")]
    InsufficientLamports,
    #[msg("Cooldown not expired")]
    CooldownNotExpired,
    #[msg("Production is disabled")]
    ProductionDisabled,
    #[msg("Invalid miner type")]
    InvalidMinerType,
    #[msg("Invalid facility type")]
    InvalidFacilityType,
    #[msg("Unauthorized access")]
    Unauthorized,
    #[msg("Initial facility already purchased")]
    InitialFacilityAlreadyPurchased,
    #[msg("Invalid referrer")]
    InvalidReferrer,
    #[msg("Invalid token mint")]
    InvalidTokenMint,
    #[msg("New wallet restricted")]
    NewWalletRestricted,
    #[msg("No pending reward")]
    NoPendingReward,
    #[msg("Reward already claimed")]
    RewardAlreadyClaimed,
    #[msg("Reward expired")]
    RewardExpired,
    #[msg("Reward exceeds remaining supply")]
    RewardExceedsSupply,
}
