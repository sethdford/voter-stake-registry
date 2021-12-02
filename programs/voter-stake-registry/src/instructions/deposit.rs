use crate::error::*;
use crate::state::lockup::*;
use crate::state::registrar::Registrar;
use crate::state::voter::Voter;
use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};
use std::convert::TryFrom;

#[derive(Accounts)]
pub struct Deposit<'info> {
    pub registrar: Box<Account<'info, Registrar>>,

    #[account(mut, has_one = registrar)]
    pub voter: AccountLoader<'info, Voter>,

    #[account(
        mut,
        associated_token::authority = registrar,
        associated_token::mint = deposit_mint,
    )]
    pub vault: Box<Account<'info, TokenAccount>>,
    pub deposit_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        constraint = deposit_token.mint == deposit_mint.key(),
        constraint = deposit_token.owner == deposit_authority.key(),
    )]
    pub deposit_token: Box<Account<'info, TokenAccount>>,
    pub deposit_authority: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

impl<'info> Deposit<'info> {
    pub fn transfer_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::Transfer<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::Transfer {
            from: self.deposit_token.to_account_info(),
            to: self.vault.to_account_info(),
            authority: self.deposit_authority.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }
}

/// Adds tokens to a deposit entry by depositing tokens into the registrar.
pub fn deposit(ctx: Context<Deposit>, id: u8, amount: u64) -> Result<()> {
    msg!("--------update_deposit--------");
    let registrar = &ctx.accounts.registrar;
    let voter = &mut ctx.accounts.voter.load_mut()?;

    voter.last_deposit_slot = Clock::get()?.slot;

    // Get the exchange rate entry associated with this deposit.
    let er_idx = registrar
        .rates
        .iter()
        .position(|r| r.mint == ctx.accounts.deposit_mint.key())
        .ok_or(ErrorCode::ExchangeRateEntryNotFound)?;
    let _er_entry = registrar.rates[er_idx];

    require!(voter.deposits.len() > id as usize, InvalidDepositId);
    let d_entry = &mut voter.deposits[id as usize];
    require!(d_entry.is_used, InvalidDepositId);

    // Deposit tokens into the registrar.
    token::transfer(ctx.accounts.transfer_ctx(), amount)?;
    d_entry.amount_deposited_native += amount;

    // Adding funds to a lockup that is already in progress can be complicated
    // for linear vesting schedules because all added funds should be paid out
    // gradually over the remaining lockup duration.
    // The logic used is to wrap up the current lockup, and create a new one
    // for the expected remainder duration.
    let curr_ts = registrar.clock_unix_timestamp();
    d_entry.amount_initially_locked_native -= d_entry.vested(curr_ts)?;
    d_entry.amount_initially_locked_native += amount;
    d_entry.lockup.start_ts = d_entry
        .lockup
        .start_ts
        .checked_add(
            i64::try_from(d_entry.lockup.period_current(curr_ts)?)
                .unwrap()
                .checked_mul(d_entry.lockup.kind.period_secs())
                .unwrap(),
        )
        .unwrap();

    Ok(())
}