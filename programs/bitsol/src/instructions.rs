use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Burn, Mint, MintTo, Token, TokenAccount, Transfer},
};

use crate::{constants::*, errors::BitSolError, helpers::*, state::*};

/// ────────────────────────────────────────────────────────────────────────────
/// INTERNAL: update the global accumulator
/// ────────────────────────────────────────────────────────────────────────────
fn update_pool(gs: &mut GlobalState, slot_now: u64) {
    if slot_now <= gs.last_reward_slot || gs.total_hashpower == 0 {
        gs.last_reward_slot = slot_now;
        return;
    }
    // Calculate theoretical halvings based on elapsed slots
    let raw_halvings = calculate_halvings(slot_now, gs.start_slot, gs.halving_interval);

    // Limit halvings to the maximum meaningful value
    let max_halvings = calculate_max_halvings(gs.initial_reward_rate);
    let halvings = raw_halvings.min(max_halvings);

    let rate_now = reward_after_halvings(gs.initial_reward_rate, halvings);

    /* remaining supply after accounting for burns */
    let minted_minus_burn = gs.cumulative_rewards.saturating_sub(gs.burned_tokens);
    let remaining_supply = gs.total_supply.saturating_sub(minted_minus_burn);

    let dust_threshold = gs.total_supply / 1000; // 0.1% of total supply
                                                 // Check if we're close to depleting the supply
    if remaining_supply <= dust_threshold || rate_now == 0 {
        // Then set rate to zero to prevent future mining
        gs.current_reward_rate = 0;
        gs.last_reward_slot = slot_now;
        return;
    }

    let slots_elapsed = (slot_now - gs.last_reward_slot) as u128;
    let mut reward = slots_elapsed * rate_now as u128;
    reward = reward.min(remaining_supply as u128); // clamp to cap

    gs.acc_bits_per_hash += reward * ACC_SCALE / gs.total_hashpower as u128;
    gs.cumulative_rewards = gs.cumulative_rewards.saturating_add(reward as u64);

    gs.current_reward_rate = if remaining_supply > 0 { rate_now } else { 0 };

    gs.last_reward_slot = slot_now;
    gs.last_processed_halvings = halvings;
}

/// Helper to settle and mint rewards for a player.
/// Returns Ok(amount_claimed) or Ok(0) if nothing to claim.
fn settle_and_mint_rewards<'info>(
    player: &mut Account<'info, Player>,
    gs: &mut Account<'info, GlobalState>,
    now: u64,
    player_token_account: &AccountInfo<'info>,
    referrer_token_account: Option<&AccountInfo<'info>>,
    governance_pda: &AccountInfo<'info>,
    token_mint: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    global_state_bump: u8,
) -> Result<u64> {
    // update pool to now
    update_pool(gs, now);

    require!(
        now > player.last_claim_slot,
        BitSolError::CooldownNotExpired
    );

    // calculate pending
    let pending_u128 = player.hashpower as u128
        * (gs.acc_bits_per_hash - player.last_acc_bits_per_hash)
        / ACC_SCALE;
    let mut pending = pending_u128 as u64;

    // Clamp pending to remaining supply
    let minted_minus_burn = gs.cumulative_rewards.saturating_sub(gs.burned_tokens);
    let remaining_supply = gs.total_supply.saturating_sub(minted_minus_burn);
    if pending > remaining_supply {
        pending = remaining_supply;
    }

    if pending == 0 {
        player.last_claim_slot = now;
        player.last_acc_bits_per_hash = gs.acc_bits_per_hash;
        return Ok(0);
    }

    // update player bookkeeping
    player.last_claim_slot = now;
    player.last_acc_bits_per_hash = gs.acc_bits_per_hash;

    // split referral based on configured fee (per-mille)
    let referral_amount = pending * gs.referral_fee as u64 / 1000;
    let player_amount = pending - referral_amount;

    // signer seeds
    let token_mint_key = &token_mint.key();
    let seeds = &[
        GLOBAL_STATE_SEED,
        token_mint_key.as_ref(),
        &[global_state_bump],
    ];
    let signer = &[&seeds[..]];

    // mint to player
    token::mint_to(
        CpiContext::new_with_signer(
            token_program.clone(),
            MintTo {
                mint: token_mint.clone(),
                to: player_token_account.clone(),
                authority: gs.to_account_info(),
            },
            signer,
        ),
        player_amount,
    )?;

    // referral / governance
    if let Some(referrer_account) = referrer_token_account {
        token::mint_to(
            CpiContext::new_with_signer(
                token_program.clone(),
                MintTo {
                    mint: token_mint.clone(),
                    to: referrer_account.clone(),
                    authority: gs.to_account_info(),
                },
                signer,
            ),
            referral_amount,
        )?;
    } else {
        token::mint_to(
            CpiContext::new_with_signer(
                token_program.clone(),
                MintTo {
                    mint: token_mint.clone(),
                    to: governance_pda.clone(),
                    authority: gs.to_account_info(),
                },
                signer,
            ),
            referral_amount,
        )?;
    }

    player.total_rewards = player.total_rewards.saturating_add(player_amount);

    Ok(pending)
}

/// ────────────────────────────────────────────────────────────────────────────
/* ──────────────────────────
INITIALIZE
────────────────────────── */
#[derive(Accounts)]
pub struct InitializeProgram<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        init,
        payer = authority,
        space = 8  /* discriminator */
        + 32 + 32 + 32          /* authority + mint + gov acct */
        + 8  + 8                /* total_supply + burned_tokens */
        + 8  + 8                /* cumulative_rewards + start_slot */
        + 8  + 8  + 8           /* halving_interval + last_halvings + initial_rate */
        + 8  + 16 + 8           /* current_rate + acc_bits_per_hash (u128!) + last_reward_slot */
        + 1  + 1 + 1 + 8        /* burn_rate + referral_fee + prod + cooldown */
        + 8                     /* total_hashpower */
        + 25                    /* global_random_reward: Option<GlobalRandomReward> */
        + 12,                   /* last_10_claimed_global_rewards: Vec<ClaimedGlobalReward> */
        seeds=[GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        init,
        payer = authority,
        seeds=[GOVERNANCE_TOKEN_SEED, global_state.key().as_ref(), token_mint.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = global_state
    )]
    pub governance_token_account: Account<'info, TokenAccount>,
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn initialize_program(
    ctx: Context<InitializeProgram>,
    start_slot: u64,
    halving_interval: u64,
    total_supply: u64,
    initial_reward_rate: u64,
    cooldown_slots: Option<u64>,
) -> Result<()> {
    let gs = &mut ctx.accounts.global_state;

    gs.authority = ctx.accounts.authority.key();
    gs.token_mint = ctx.accounts.token_mint.key();
    gs.governance_token_account = ctx.accounts.governance_token_account.key();

    gs.total_supply = total_supply;
    gs.burned_tokens = 0;
    gs.cumulative_rewards = 0;

    gs.start_slot = start_slot;
    gs.halving_interval = halving_interval;
    gs.last_processed_halvings = 0;
    gs.initial_reward_rate = initial_reward_rate;
    gs.current_reward_rate = initial_reward_rate;

    gs.acc_bits_per_hash = 0;
    gs.last_reward_slot = start_slot;

    gs.burn_rate = 75;
    gs.referral_fee = 25;
    gs.production_enabled = true;
    gs.cooldown_slots = cooldown_slots.unwrap_or(108_000); // 12 hours

    gs.total_hashpower = 0;
    gs.global_random_reward = None;

    Ok(())
}

/// ────────────────────────────────────────────────────────────────────────────
///  PURCHASE INITIAL FACILITY
/// ────────────────────────────────────────────────────────────────────────────
#[derive(Accounts)]
#[instruction(referrer: Option<Pubkey>)]
pub struct PurchaseInitialFacility<'info> {
    #[account(mut)]
    pub player_wallet: Signer<'info>,
    #[account(
        init,
        payer = player_wallet,
        space = 8      // discriminator
            + 32       // owner
            + 10       // facility
            + 4        // miners vec header
            + (10 * 17)// miners (10 × (1 + 8 + 8))
            + 8        // hashpower
            + 33       // referrer
            + 16       // last_acc_bits_per_hash
            + 8        // last_claim_slot
            + 8        // last_upgrade_slot
            + 8        // total_rewards
            + 4        // last_10_claimed_global_rewards vec header
            + (10 * 24),// last_10_claimed_global_rewards (10 × (8 + 8 + 8))
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = token_mint.key() == global_state.token_mint @ BitSolError::InvalidTokenMint
    )]
    pub token_mint: Account<'info, Mint>,
    #[account(
        init_if_needed,
        payer = player_wallet,
        associated_token::mint = token_mint,
        associated_token::authority = player_wallet,
    )]
    pub player_token_account: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn purchase_initial_facility(
    ctx: Context<PurchaseInitialFacility>,
    referrer: Option<Pubkey>,
) -> Result<()> {
    let clock = &ctx.accounts.clock;
    let player = &mut ctx.accounts.player;
    let gs = &mut ctx.accounts.global_state;

    require!(gs.production_enabled, BitSolError::ProductionDisabled);
    require!(
        player.miners.is_empty(),
        BitSolError::InitialFacilityAlreadyPurchased
    );

    // Make sure pool is up to date
    update_pool(gs, clock.slot);

    // transfer 0.25 SOL to governance PDA
    anchor_lang::system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.player_wallet.to_account_info(),
                to: gs.to_account_info(),
            },
        ),
        250_000_000,
    )?;

    // player bootstrap
    player.owner = ctx.accounts.player_wallet.key();
    player.facility = Facility {
        facility_type: 0, // Starter Shack
        total_miners: 2,  // From facilities.json
        power_output: 15, // From facilities.json
    };
    player.miners = vec![Miner {
        miner_type: 0,        // Nano Rig
        hashrate: 1_500,      // From miners.json
        power_consumption: 3, // From miners.json
    }];
    player.hashpower = 1_500; // Match the initial miner's hashrate
    player.referrer = referrer;
    player.last_claim_slot = clock.slot;
    player.last_upgrade_slot = clock.slot;
    player.total_rewards = 0;
    player.last_acc_bits_per_hash = gs.acc_bits_per_hash;

    // global stats
    gs.total_hashpower += 1_500;

    Ok(())
}

/// ────────────────────────────────────────────────────────────────────────────
///  BUY MINER
/// ────────────────────────────────────────────────────────────────────────────
#[derive(Accounts)]
#[instruction(miner_type: u8, facility_slot: u8)]
pub struct BuyMiner<'info> {
    #[account(mut)]
    pub player_wallet: Signer<'info>,
    #[account(
        mut,
        constraint = player.owner == player_wallet.key() @ BitSolError::Unauthorized,
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    #[account(
        mut,
        seeds = [GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump,
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = player_token_account.mint == global_state.token_mint
    )]
    pub player_token_account: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = governance_token_account.mint == global_state.token_mint,
        constraint = governance_token_account.key() == global_state.governance_token_account @ BitSolError::Unauthorized
    )]
    pub governance_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
    pub system_program: Program<'info, System>,
}

pub fn buy_miner(ctx: Context<BuyMiner>, miner_type: u8, _facility_slot: u8) -> Result<()> {
    require!(
        miner_type < MINER_CONFIGS.len() as u8,
        BitSolError::InvalidMinerType
    );

    let clock = &ctx.accounts.clock;
    let player = &mut ctx.accounts.player;
    let gs = &mut ctx.accounts.global_state;

    // guards
    require!(gs.production_enabled, BitSolError::ProductionDisabled);

    require!(
        player.miners.len() < player.facility.total_miners as usize,
        BitSolError::MinerCapacityExceeded
    );

    settle_and_mint_rewards(
        player,
        gs,
        clock.slot,
        &ctx.accounts.player_token_account.to_account_info(),
        None,
        &ctx.accounts.governance_token_account.to_account_info(),
        &ctx.accounts.token_mint.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        ctx.bumps.global_state,
    )?;

    // configs
    let (hashrate, power_consumption, cost) = MINER_CONFIGS[miner_type as usize];
    let total_power = player
        .miners
        .iter()
        .map(|m| m.power_consumption)
        .sum::<u64>()
        + power_consumption;
    require!(
        total_power <= player.facility.power_output,
        BitSolError::PowerCapacityExceeded
    );
    require!(
        ctx.accounts.player_token_account.amount >= cost,
        BitSolError::InsufficientBits
    );

    // burn/transfer BITS
    let burn_amount = cost * gs.burn_rate as u64 / 100;
    let governance_amount = cost - burn_amount;

    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.token_mint.to_account_info(),
                from: ctx.accounts.player_token_account.to_account_info(),
                authority: ctx.accounts.player_wallet.to_account_info(),
            },
        ),
        burn_amount,
    )?;
    gs.burned_tokens = gs.burned_tokens.saturating_add(burn_amount);

    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.player_token_account.to_account_info(),
                to: ctx.accounts.governance_token_account.to_account_info(),
                authority: ctx.accounts.player_wallet.to_account_info(),
            },
        ),
        governance_amount,
    )?;

    // change hash-power
    player.miners.push(Miner {
        miner_type,
        hashrate,
        power_consumption,
    });
    player.hashpower += hashrate;
    gs.total_hashpower += hashrate;

    player.last_acc_bits_per_hash = gs.acc_bits_per_hash;

    Ok(())
}

/// ────────────────────────────────────────────────────────────────────────────
///  SELL MINER
/// ────────────────────────────────────────────────────────────────────────────
#[derive(Accounts)]
#[instruction(miner_index: u8)]
pub struct SellMiner<'info> {
    #[account(mut)]
    pub player_wallet: Signer<'info>,
    #[account(
        mut,
        constraint = player.owner == player_wallet.key() @ BitSolError::Unauthorized,
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    #[account(
        mut,
        seeds = [GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump,
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = player_token_account.mint == global_state.token_mint
    )]
    pub player_token_account: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = governance_token_account.mint == global_state.token_mint,
        constraint = governance_token_account.key() == global_state.governance_token_account @ BitSolError::Unauthorized
    )]
    pub governance_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn sell_miner(ctx: Context<SellMiner>, miner_index: u8) -> Result<()> {
    let clock = &ctx.accounts.clock;
    let player = &mut ctx.accounts.player;
    let gs = &mut ctx.accounts.global_state;

    require!(gs.production_enabled, BitSolError::ProductionDisabled);

    require!(
        miner_index < player.miners.len() as u8,
        BitSolError::InvalidMinerType
    );

    settle_and_mint_rewards(
        player,
        gs,
        clock.slot,
        &ctx.accounts.player_token_account.to_account_info(),
        None,
        &ctx.accounts.governance_token_account.to_account_info(),
        &ctx.accounts.token_mint.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        ctx.bumps.global_state,
    )?;

    let miner = player.miners.remove(miner_index as usize);

    player.hashpower = player.hashpower.saturating_sub(miner.hashrate);
    gs.total_hashpower = gs.total_hashpower.saturating_sub(miner.hashrate);

    player.last_acc_bits_per_hash = gs.acc_bits_per_hash;

    Ok(())
}

/// ────────────────────────────────────────────────────────────────────────────
///  UPGRADE FACILITY  (hp does not change → only update pool/debt if you add hp)
/// ────────────────────────────────────────────────────────────────────────────
#[derive(Accounts)]
#[instruction(facility_type: u8)]
pub struct UpgradeFacility<'info> {
    #[account(mut)]
    pub player_wallet: Signer<'info>,
    #[account(
        mut,
        constraint = player.owner == player_wallet.key() @ BitSolError::Unauthorized,
        constraint = facility_type > player.facility.facility_type @ BitSolError::InvalidFacilityType,
        constraint = facility_type <= HIGH_RISE_APARTMENT @ BitSolError::InvalidFacilityType,
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    #[account(
        mut,
        seeds = [GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump,
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = player_token_account.mint == global_state.token_mint
    )]
    pub player_token_account: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = governance_token_account.mint == global_state.token_mint,
        constraint = governance_token_account.key() == global_state.governance_token_account @ BitSolError::Unauthorized
    )]
    pub governance_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn upgrade_facility(ctx: Context<UpgradeFacility>, facility_type: u8) -> Result<()> {
    require!(
        facility_type >= LOW_PROFILE_STORAGE && facility_type <= HIGH_RISE_APARTMENT,
        BitSolError::InvalidFacilityType
    );

    let clock = &ctx.accounts.clock;
    let player = &mut ctx.accounts.player;
    let gs = &mut ctx.accounts.global_state;

    update_pool(gs, clock.slot);

    require!(gs.production_enabled, BitSolError::ProductionDisabled);
    require!(
        clock.slot >= player.last_upgrade_slot + gs.cooldown_slots,
        BitSolError::CooldownNotExpired
    );

    settle_and_mint_rewards(
        player,
        gs,
        clock.slot,
        &ctx.accounts.player_token_account.to_account_info(),
        None,
        &ctx.accounts.governance_token_account.to_account_info(),
        &ctx.accounts.token_mint.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        ctx.bumps.global_state,
    )?;

    let (total_miners, power_output, cost) = FACILITY_CONFIGS[facility_type as usize];

    require!(
        ctx.accounts.player_token_account.amount >= cost,
        BitSolError::InsufficientBits
    );

    let burn_amount = cost * gs.burn_rate as u64 / 100;
    let governance_amount = cost - burn_amount;

    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.token_mint.to_account_info(),
                from: ctx.accounts.player_token_account.to_account_info(),
                authority: ctx.accounts.player_wallet.to_account_info(),
            },
        ),
        burn_amount,
    )?;
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.player_token_account.to_account_info(),
                to: ctx.accounts.governance_token_account.to_account_info(),
                authority: ctx.accounts.player_wallet.to_account_info(),
            },
        ),
        governance_amount,
    )?;

    player.facility.facility_type = facility_type;
    player.facility.total_miners = total_miners;
    player.facility.power_output = power_output;
    player.last_upgrade_slot = clock.slot;

    player.last_acc_bits_per_hash = gs.acc_bits_per_hash;

    Ok(())
}

/// ────────────────────────────────────────────────────────────────────────────
///  CLAIM REWARDS
/// ────────────────────────────────────────────────────────────────────────────
#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    #[account(mut)]
    pub player_wallet: Signer<'info>,
    #[account(
        mut,
        constraint = player.owner == player_wallet.key() @ BitSolError::Unauthorized,
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    #[account(
        mut,
        seeds = [GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump,
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = player_token_account.owner == player_wallet.key(),
        constraint = player_token_account.mint == global_state.token_mint
    )]
    pub player_token_account: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        constraint = player.referrer.is_some() && referrer_token_account.owner == player.referrer.unwrap() @ BitSolError::InvalidReferrer,
        constraint = referrer_token_account.mint == global_state.token_mint @ BitSolError::InvalidMinerType
    )]
    pub referrer_token_account: Option<Box<Account<'info, TokenAccount>>>,
    #[account(
        mut,
        seeds = [
            GOVERNANCE_TOKEN_SEED,
            global_state.key().as_ref(),
            global_state.token_mint.key().as_ref()
        ],
        bump,
        constraint = governance_pda.key() == global_state.governance_token_account @ BitSolError::Unauthorized
    )]
    pub governance_pda: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
    let now = ctx.accounts.clock.slot;

    settle_and_mint_rewards(
        &mut ctx.accounts.player,
        &mut ctx.accounts.global_state,
        now,
        &ctx.accounts.player_token_account.to_account_info(),
        ctx.accounts
            .referrer_token_account
            .as_ref()
            .map(|a| a.to_account_info())
            .as_ref(),
        &ctx.accounts.governance_pda.to_account_info(),
        &ctx.accounts.token_mint.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        ctx.bumps.global_state,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct ToggleProduction<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
}

pub fn toggle_production(ctx: Context<ToggleProduction>, enable: bool) -> Result<()> {
    let global_state = &mut ctx.accounts.global_state;
    global_state.production_enabled = enable;
    Ok(())
}

#[derive(Accounts)]
pub struct UpdateParameters<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
}

pub fn update_parameters(
    ctx: Context<UpdateParameters>,
    referral_fee: Option<u8>,
    burn_rate: Option<u8>,
    cooldown_slots: Option<u64>,
    halving_interval: Option<u64>,
) -> Result<()> {
    let global_state = &mut ctx.accounts.global_state;

    if let Some(fee) = referral_fee {
        require!(fee <= 50, BitSolError::InvalidMinerType); // Max 5%
        global_state.referral_fee = fee;
    }

    if let Some(rate) = burn_rate {
        global_state.burn_rate = rate;
    }

    if let Some(slots) = cooldown_slots {
        require!(slots > 0, BitSolError::InvalidMinerType);
        global_state.cooldown_slots = slots;
    }

    if let Some(halving) = halving_interval {
        require!(halving > 0, BitSolError::InvalidMinerType);
        global_state.halving_interval = halving;
    }

    Ok(())
}

#[derive(Accounts)]
pub struct UpdatePool<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized,
        constraint = authority.key() == global_state.authority @ BitSolError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn update_pool_manual(ctx: Context<UpdatePool>) -> Result<()> {
    let global_state = &mut ctx.accounts.global_state;
    let slot_now = ctx.accounts.clock.slot;

    update_pool(global_state, slot_now);

    Ok(())
}

#[derive(Accounts)]
pub struct WithdrawFees<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized,
        seeds = [GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = governance_token_account.mint == global_state.token_mint,
        constraint = governance_token_account.key() == global_state.governance_token_account @ BitSolError::Unauthorized
    )]
    pub governance_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub destination: Box<Account<'info, TokenAccount>>,
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

pub fn withdraw_fees(ctx: Context<WithdrawFees>, amount: u64) -> Result<()> {
    let token_mint = ctx.accounts.token_mint.key();
    let seeds = &[
        GLOBAL_STATE_SEED,
        token_mint.as_ref(),
        &[ctx.bumps.global_state],
    ];
    let signer = &[&seeds[..]];

    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            token::Transfer {
                from: ctx.accounts.governance_token_account.to_account_info(),
                to: ctx.accounts.destination.to_account_info(),
                authority: ctx.accounts.global_state.to_account_info(),
            },
            signer,
        ),
        amount,
    )?;
    Ok(())
}

#[derive(Accounts)]
pub struct WithdrawSolFees<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    /// CHECK: This is just a system account
    #[account(mut)]
    pub destination: AccountInfo<'info>,
    pub system_program: Program<'info, System>,
}

pub fn withdraw_sol_fees(ctx: Context<WithdrawSolFees>, amount: u64) -> Result<()> {
    let gs = ctx.accounts.global_state.to_account_info();
    let destination = ctx.accounts.destination.to_account_info();

    // Get the minimum balance required for rent-exemption
    let rent = Rent::get()?;
    let min_balance = rent.minimum_balance(gs.data_len());

    // Only allow withdrawal if enough lamports remain for rent-exemption
    let available = **gs.lamports.borrow();
    require!(available > min_balance, BitSolError::InsufficientLamports);
    let withdrawable = available.saturating_sub(min_balance);
    require!(amount <= withdrawable, BitSolError::InsufficientLamports);

    **gs.try_borrow_mut_lamports()? -= amount;
    **destination.try_borrow_mut_lamports()? += amount;

    Ok(())
}

#[derive(Accounts)]
pub struct GenerateGlobalRandomReward<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn generate_global_random_reward(
    ctx: Context<GenerateGlobalRandomReward>,
    amount: u64,
    expiry_slots: u64,
) -> Result<()> {
    let clock = &ctx.accounts.clock;
    let gs = &mut ctx.accounts.global_state;

    gs.global_random_reward = Some(GlobalRandomReward {
        amount,
        generated_slot: clock.slot,
        expiry_slot: clock.slot + expiry_slots,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct ClaimGlobalRandomReward<'info> {
    #[account(mut)]
    pub player_wallet: Signer<'info>,
    #[account(
        mut,
        constraint = player.owner == player_wallet.key() @ BitSolError::Unauthorized,
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    #[account(
        mut,
        seeds = [GLOBAL_STATE_SEED, token_mint.key().as_ref()],
        bump,
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        constraint = player_token_account.owner == player_wallet.key(),
        constraint = player_token_account.mint == global_state.token_mint
    )]
    pub player_token_account: Box<Account<'info, TokenAccount>>,
    #[account(mut)]
    pub token_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn claim_global_random_reward(ctx: Context<ClaimGlobalRandomReward>) -> Result<()> {
    let clock = &ctx.accounts.clock;
    let player = &mut ctx.accounts.player;
    let gs = &mut ctx.accounts.global_state;
    let token_mint = &ctx.accounts.token_mint.key();

    let reward = gs
        .global_random_reward
        .as_ref()
        .ok_or(BitSolError::NoPendingReward)?;
    require!(clock.slot <= reward.expiry_slot, BitSolError::RewardExpired);

    // Check if player already claimed this reward (by generated_slot)
    let already_claimed = player
        .last_10_claimed_global_rewards
        .iter()
        .any(|r| r.generated_slot == reward.generated_slot);
    require!(!already_claimed, BitSolError::RewardAlreadyClaimed);

    // Ensure reward does not exceed remaining supply
    let minted_minus_burn = gs.cumulative_rewards.saturating_sub(gs.burned_tokens);
    let remaining_supply = gs.total_supply.saturating_sub(minted_minus_burn);
    require!(reward.amount <= remaining_supply, BitSolError::RewardExceedsSupply);

    // Mint reward to player
    let seeds = &[
        GLOBAL_STATE_SEED,
        token_mint.as_ref(),
        &[ctx.bumps.global_state],
    ];
    let signer = &[&seeds[..]];

    token::mint_to(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.token_mint.to_account_info(),
                to: ctx.accounts.player_token_account.to_account_info(),
                authority: gs.to_account_info(),
            },
            signer,
        ),
        reward.amount,
    )?;

    // track supply and clear reward
    gs.cumulative_rewards = gs
        .cumulative_rewards
        .saturating_add(reward.amount);
    gs.global_random_reward = None;

    // Record claim
    let claimed = ClaimedGlobalReward {
        generated_slot: reward.generated_slot,
        claimed_slot: clock.slot,
        amount: reward.amount,
    };
    if player.last_10_claimed_global_rewards.len() >= 10 {
        player.last_10_claimed_global_rewards.remove(0);
    }
    player.last_10_claimed_global_rewards.push(claimed);

    Ok(())
}

#[derive(Accounts)]
pub struct ResetPlayer<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        mut,
        has_one = authority @ BitSolError::Unauthorized
    )]
    pub global_state: Account<'info, GlobalState>,
    #[account(
        mut,
        seeds = [PLAYER_SEED, player_wallet.key().as_ref()],
        bump
    )]
    pub player: Account<'info, Player>,
    /// CHECK: This is just a system account
    pub player_wallet: AccountInfo<'info>,
    pub clock: Sysvar<'info, Clock>,
}

pub fn reset_player(ctx: Context<ResetPlayer>) -> Result<()> {
    let player = &mut ctx.accounts.player;
    let gs = &mut ctx.accounts.global_state;
    let clock = &ctx.accounts.clock;

    // Update pool to current slot
    update_pool(gs, clock.slot);

    // Store the old hashpower to update global state
    let old_hashpower = player.hashpower;

    // Reset player's hashpower and facility
    player.hashpower = 0;
    player.facility = Facility {
        facility_type: 0,
        total_miners: 2,
        power_output: 15,
    };
    player.miners = vec![]; // Clear all miners

    // Update global hashpower
    gs.total_hashpower = gs.total_hashpower.saturating_sub(old_hashpower);

    // Update player's last claim slot and accumulator
    player.last_claim_slot = clock.slot;
    player.last_acc_bits_per_hash = gs.acc_bits_per_hash;

    Ok(())
}
