use anchor_lang::__private::bytemuck::Zeroable;
use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount};
use std::mem::size_of;
use voter_weight_record::VoterWeightRecord;

mod voter_weight_record;

declare_id!("Fg6PaFpoGXkYsidMpWTK6W2BeZ7FEfcYkg476zPFsLnS");

/// # Introduction
///
/// The governance registry is an "addin" to the SPL governance program that
/// allows one to both many different ypes of tokens for voting and to scale
/// voting power as a linear function of time locked--subject to some maximum
/// upper bound.
///
/// The overall process is as follows:
///
/// - Create a SPL governance realm.
/// - Create a governance registry account.
/// - Add exchange rates for any tokens one wants to deposit. For example,
///   if one wants to vote with tokens A and B, where token B has twice the
///   voting power of token A, then the exchange rate of B would be 2 and the
///   exchange rate of A would be 1.
/// - Create a voter account.
/// - Deposit tokens into this program, with an optional lockup period.
/// - Vote.
///
/// Upon voting with SPL governance, a client is expected to call
/// `decay_voting_power` to get an up to date measurement of a given `Voter`'s
/// voting power for the given slot. If this is not done, then the transaction
/// will fail (since the SPL governance program will require the measurement
/// to be active for the current slot).
///
/// # Interacting with SPL Governance
///
/// This program does not directly interact with SPL governance via CPI.
/// Instead, it simply writes a `VoterWeightRecord` account with a well defined
/// format, which is then used by SPL governance as the voting power measurement
/// for a given user.
#[program]
pub mod governance_registry {
    use super::*;

    /// Creates a new voting registrar. There can only be a single regsitrar
    /// per governance realm.
    pub fn create_registrar(
        ctx: Context<CreateRegistrar>,
        registrar_bump: u8,
        voting_mint_bump: u8,
        _voting_mint_decimals: u8,
    ) -> Result<()> {
        let registrar = &mut ctx.accounts.registrar.load_init()?;
        registrar.bump = registrar_bump;
        registrar.voting_mint_bump = voting_mint_bump;
        registrar.realm = ctx.accounts.realm.key();
        registrar.voting_mint = ctx.accounts.voting_mint.key();
        registrar.authority = ctx.accounts.authority.key();

        Ok(())
    }

    /// Creates a new voter account. There can only be a single voter per
    /// user wallet.
    pub fn create_voter(ctx: Context<CreateVoter>, voter_bump: u8) -> Result<()> {
        let voter = &mut ctx.accounts.voter.load_init()?;
        voter.voter_bump = voter_bump;
        voter.authority = ctx.accounts.authority.key();
        voter.registrar = ctx.accounts.registrar.key();

        Ok(())
    }

    /// Creates a new exchange rate for a given mint. This allows a voter to
    /// deposit the mint in exchange for vTokens. There can only be a single
    /// exchange rate per mint.
    pub fn create_exchange_rate(
        ctx: Context<CreateExchangeRate>,
        er: ExchangeRateEntry,
    ) -> Result<()> {
        require!(er.rate > 0, InvalidRate);

        let mut er = er;
        er.is_used = false;

        let registrar = &mut ctx.accounts.registrar.load_mut()?;
        let idx = registrar
            .rates
            .iter()
            .position(|r| !r.is_used)
            .ok_or(ErrorCode::RatesFull)?;
        registrar.rates[idx] = er;
        Ok(())
    }

    /// Creates a new deposit entry and updates it by transferring in tokens.
    pub fn create_deposit(
        ctx: Context<CreateDeposit>,
        amount: u64,
        lockup: Option<Lockup>,
    ) -> Result<()> {
        // Creates the new deposit.
        let deposit_id = {
            let registrar = &ctx.accounts.deposit.registrar.load()?;
            let voter = &mut ctx.accounts.deposit.voter.load_mut()?;

            // Get the exchange rate entry associated with this deposit.
            let er_idx = registrar
                .rates
                .iter()
                .position(|r| r.mint == ctx.accounts.deposit.deposit_mint.key())
                .ok_or(ErrorCode::ExchangeRateEntryNotFound)?;

            // Get and set up the first free deposit entry.
            let free_entry_idx = voter
                .deposits
                .iter()
                .position(|d_entry| !d_entry.is_used)
                .ok_or(ErrorCode::DepositEntryFull)?;
            let d_entry = &mut voter.deposits[free_entry_idx];
            d_entry.is_used = true;
            d_entry.rate_idx = free_entry_idx as u8;
            d_entry.rate_idx = er_idx as u8;

            if let Some(l) = lockup {
                d_entry.lockup = l;
            }

            free_entry_idx as u8
        };

        // Updates the entry by transferring in tokens.
        let update_ctx = Context::new(ctx.program_id, &mut ctx.accounts.deposit, &[]);
        update_deposit(update_ctx, deposit_id, amount)?;

        Ok(())
    }

    /// Updates a deposit entry by depositing tokens into the registrar in
    /// exchange for *frozen* voting tokens. These tokens are not used for
    /// anything other than displaying the amount in wallets.
    pub fn update_deposit(ctx: Context<UpdateDeposit>, id: u8, amount: u64) -> Result<()> {
        let registrar = &ctx.accounts.registrar.load()?;
        let voter = &mut ctx.accounts.voter.load_mut()?;

        // Get the exchange rate entry associated with this deposit.
        let er_idx = registrar
            .rates
            .iter()
            .position(|r| r.mint == ctx.accounts.deposit_mint.key())
            .ok_or(ErrorCode::ExchangeRateEntryNotFound)?;
        let er_entry = registrar.rates[er_idx];

        require!(voter.deposits.len() > id as usize, InvalidDepositId);
        let d_entry = &mut voter.deposits[id as usize];

        d_entry.amount += amount;

        // Calculate the amount of voting tokens to mint at the specified
        // exchange rate.
        let scaled_amount = er_entry.rate * amount;

        // Deposit tokens into the registrar.
        token::transfer(ctx.accounts.transfer_ctx(), amount)?;

        // Mint vote tokens to the depositor.
        token::mint_to(
            ctx.accounts
                .mint_to_ctx()
                .with_signer(&[&[registrar.realm.as_ref(), &[registrar.bump]]]),
            scaled_amount,
        )?;

        // Freeze the vote tokens; they are just used for UIs + accounting.
        token::freeze_account(
            ctx.accounts
                .freeze_ctx()
                .with_signer(&[&[registrar.realm.as_ref(), &[registrar.bump]]]),
        )?;

        Ok(())
    }

    /// Withdraws tokens from a deposit entry, if they are unlocked according
    /// to a vesting schedule.
    ///
    /// `amount` is in units of the native currency being withdrawn.
    pub fn withdraw(ctx: Context<Withdraw>, deposit_id: u8, amount: u64) -> Result<()> {
        let registrar = &ctx.accounts.registrar.load()?;
        let voter = &mut ctx.accounts.voter.load_mut()?;
        require!(voter.deposits.len() > deposit_id.into(), InvalidDepositId);

        // Update the deposit bookkeeping.
        let deposit_entry = &mut voter.deposits[deposit_id as usize];
        require!(deposit_entry.is_used, InvalidDepositId);
        require!(deposit_entry.vested() >= amount, InsufficientVestedTokens);
        deposit_entry.amount -= amount;

        // Get the exchange rate for the token being withdrawn.
        let er_idx = registrar
            .rates
            .iter()
            .position(|r| r.mint == ctx.accounts.withdraw_mint.key())
            .ok_or(ErrorCode::ExchangeRateEntryNotFound)?;
        let er_entry = registrar.rates[er_idx];

        let scaled_amount = er_entry.rate * amount;

        // Transfer the tokens to withdraw.
        token::transfer(
            ctx.accounts
                .transfer_ctx()
                .with_signer(&[&[registrar.realm.as_ref(), &[registrar.bump]]]),
            amount,
        )?;

        // Unfreeze the voting mint.
        token::thaw_account(
            ctx.accounts
                .thaw_ctx()
                .with_signer(&[&[registrar.realm.as_ref(), &[registrar.bump]]]),
        )?;

        // Burn the voting tokens.
        token::burn(ctx.accounts.burn_ctx(), scaled_amount)?;

        Ok(())
    }

    /// Updates a vesting schedule. Can only increase the lockup time or reduce
    /// the period count (since that has the effect of increasing lockup time).
    /// If all tokens are unlocked, then both can be updated arbitrarily.
    pub fn update_schedule(ctx: Context<UpdateSchedule>) -> Result<()> {
        // todo
        Ok(())
    }

    /// Calculates the lockup-scaled, time-decayed voting power for the given
    /// voter and writes it into a `VoteWeightRecord` account to be used by
    /// the SPL governance program.
    ///
    /// When a voter locks up tokens with a vesting schedule, the voter's
    /// voting power is scaled with a linear multiplier, but as time goes on,
    /// that multiplier is decreased since the remaining lockup decreases.
    pub fn decay_voting_power(ctx: Context<DecayVotingPower>) -> Result<()> {
        // todo
        Ok(())
    }

    /// Closes the voter account, allowing one to retrieve rent exemption SOL.
    pub fn close_voter(ctx: Context<CloseVoter>) -> Result<()> {
        require!(ctx.accounts.voting_token.amount > 0, VotingTokenNonZero);
        Ok(())
    }
}

// Contexts.

#[derive(Accounts)]
#[instruction(registrar_bump: u8, voting_mint_bump: u8, voting_mint_decimals: u8)]
pub struct CreateRegistrar<'info> {
    #[account(
        init,
        seeds = [realm.key().as_ref()],
        bump = registrar_bump,
        payer = payer,
        space = 8 + size_of::<Registrar>()
    )]
    registrar: AccountLoader<'info, Registrar>,
    #[account(
        init,
        seeds = [registrar.key().as_ref()],
        bump = voting_mint_bump,
        payer = payer,
        mint::authority = registrar,
        mint::freeze_authority = registrar,
        mint::decimals = voting_mint_decimals,
    )]
    voting_mint: Account<'info, Mint>,
    realm: UncheckedAccount<'info>,
    authority: UncheckedAccount<'info>,
    payer: Signer<'info>,
    system_program: Program<'info, System>,
    token_program: Program<'info, Token>,
    rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(voter_bump: u8)]
pub struct CreateVoter<'info> {
    #[account(
        init,
        seeds = [registrar.key().as_ref(), authority.key().as_ref()],
        bump = voter_bump,
        payer = authority,
        space = 8 + size_of::<Voter>(),
    )]
    voter: AccountLoader<'info, Voter>,
    #[account(
        init,
        payer = authority,
        associated_token::authority = authority,
        associated_token::mint = voting_mint,
    )]
    voting_token: Account<'info, TokenAccount>,
    voting_mint: Account<'info, Mint>,
    registrar: AccountLoader<'info, Registrar>,
    authority: Signer<'info>,
    token_program: Program<'info, Token>,
    associated_token_program: Program<'info, AssociatedToken>,
    system_program: Program<'info, System>,
    rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(rate: ExchangeRateEntry)]
pub struct CreateExchangeRate<'info> {
    #[account(
        init,
        payer = payer,
        associated_token::authority = registrar,
        associated_token::mint = deposit_mint,
    )]
    exchange_vault: Account<'info, TokenAccount>,
    deposit_mint: Account<'info, Mint>,
    #[account(mut, has_one = authority)]
    registrar: AccountLoader<'info, Registrar>,
    authority: Signer<'info>,
    payer: Signer<'info>,
    rent: Sysvar<'info, Rent>,
    token_program: Program<'info, Token>,
    associated_token_program: Program<'info, AssociatedToken>,
    system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CreateDeposit<'info> {
    deposit: UpdateDeposit<'info>,
}

impl<'info> UpdateDeposit<'info> {
    fn transfer_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::Transfer<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::Transfer {
            from: self.deposit_token.to_account_info(),
            to: self.exchange_vault.to_account_info(),
            authority: self.authority.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }

    fn mint_to_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::MintTo<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::MintTo {
            mint: self.voting_mint.to_account_info(),
            to: self.voting_token.to_account_info(),
            authority: self.registrar.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }

    fn freeze_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::FreezeAccount<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::FreezeAccount {
            account: self.voting_token.to_account_info(),
            mint: self.voting_mint.to_account_info(),
            authority: self.registrar.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }
}

#[derive(Accounts)]
pub struct UpdateDeposit<'info> {
    #[account(has_one = voting_mint)]
    registrar: AccountLoader<'info, Registrar>,
    #[account(mut, has_one = authority, has_one = registrar)]
    voter: AccountLoader<'info, Voter>,
    #[account(
        mut,
        associated_token::authority = registrar,
        associated_token::mint = deposit_mint,
    )]
    exchange_vault: Account<'info, TokenAccount>,
    #[account(
        mut,
        constraint = deposit_token.mint == deposit_mint.key(),
    )]
    deposit_token: Account<'info, TokenAccount>,
    #[account(
        mut,
        associated_token::authority = authority,
        associated_token::mint = voting_mint,
    )]
    voting_token: Account<'info, TokenAccount>,
    authority: Signer<'info>,
    deposit_mint: Account<'info, Mint>,
    #[account(mut)]
    voting_mint: Account<'info, Mint>,
    token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(has_one = voting_mint)]
    registrar: AccountLoader<'info, Registrar>,
    #[account(mut, has_one = registrar, has_one = authority)]
    voter: AccountLoader<'info, Voter>,
    #[account(
        mut,
        associated_token::authority = registrar,
        associated_token::mint = withdraw_mint,
    )]
    exchange_vault: Account<'info, TokenAccount>,
    withdraw_mint: Account<'info, Mint>,
    #[account(
        mut,
        associated_token::authority = authority,
        associated_token::mint = voting_mint,
    )]
    voting_token: Account<'info, TokenAccount>,
    #[account(mut)]
    voting_mint: Account<'info, Mint>,
    #[account(mut)]
    destination: Account<'info, TokenAccount>,
    authority: Signer<'info>,
    token_program: Program<'info, Token>,
}

impl<'info> Withdraw<'info> {
    pub fn transfer_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::Transfer<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::Transfer {
            from: self.exchange_vault.to_account_info(),
            to: self.destination.to_account_info(),
            authority: self.registrar.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }

    pub fn thaw_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::ThawAccount<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::ThawAccount {
            account: self.voting_token.to_account_info(),
            mint: self.voting_mint.to_account_info(),
            authority: self.registrar.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }

    pub fn burn_ctx(&self) -> CpiContext<'_, '_, '_, 'info, token::Burn<'info>> {
        let program = self.token_program.to_account_info();
        let accounts = token::Burn {
            mint: self.voting_mint.to_account_info(),
            to: self.voting_token.to_account_info(),
            authority: self.authority.to_account_info(),
        };
        CpiContext::new(program, accounts)
    }
}

#[derive(Accounts)]
pub struct UpdateSchedule {
    // todo
}

#[derive(Accounts)]
pub struct DecayVotingPower<'info> {
    vote_weight_record: Account<'info, VoterWeightRecord>,
}

#[derive(Accounts)]
pub struct CloseVoter<'info> {
    #[account(mut, has_one = authority, close = sol_destination)]
    voter: AccountLoader<'info, Voter>,
    authority: Signer<'info>,
    voting_token: Account<'info, TokenAccount>,
    sol_destination: UncheckedAccount<'info>,
}

// Accounts.

/// Instance of a voting rights distributor.
#[account(zero_copy)]
pub struct Registrar {
    pub authority: Pubkey,
    pub realm: Pubkey,
    pub voting_mint: Pubkey,
    pub voting_mint_bump: u8,
    pub bump: u8,
    pub rates: [ExchangeRateEntry; 32],
}

/// User account for minting voting rights.
#[account(zero_copy)]
pub struct Voter {
    pub authority: Pubkey,
    pub registrar: Pubkey,
    pub voter_bump: u8,
    pub deposits: [DepositEntry; 32],
}

/// Exchange rate for an asset that can be used to mint voting rights.
#[zero_copy]
#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct ExchangeRateEntry {
    // True if the exchange rate entry is being used.
    pub is_used: bool,

    pub mint: Pubkey,
    pub rate: u64,
}

unsafe impl Zeroable for ExchangeRateEntry {}

/// Bookkeeping for a single deposit for a given mint and lockup schedule.
#[zero_copy]
pub struct DepositEntry {
    // True if the deposit entry is being used.
    pub is_used: bool,

    // Points to the ExchangeRate this deposit uses.
    pub rate_idx: u8,
    pub amount: u64,

    // Locked state.
    pub lockup: Lockup,
}

impl DepositEntry {
    /// # Voting Power Caclulation
    ///
    /// Returns the voting power for the deposit, giving locked tokens boosted
    /// voting power that scales linearly with the lockup.
    ///
    /// The minimum lockup period is a single day. The max lockup period is
    /// seven years. And so a one day lockup has 1/2 the voting power as a two
    /// day lockup, which has 1/2555 the voting power of a 7 year lockup--
    /// assuming the amount locked up is equal.
    ///
    /// To achieve this with the SPL governance program--which requires a "max
    /// vote weight"--we attach what amounts to a scalar multiplier between 0
    /// and 1 to normalize voting power. This multiplier is a function of
    /// the lockup schedule. Here we will describe two, a one time
    /// cliff and a linear vesting schedule unlocking daily.
    ///
    /// ## Cliff Lockup
    ///
    /// The cliff lockup allows one to lockup their tokens for a set period
    /// of time, unlocking all at once on a given date.
    ///
    /// The calculation for this is straigtforward
    ///
    /// ```
    /// voting_power = (number_days / 2555) * amount
    /// ```
    ///
    /// ### Decay
    ///
    /// As time passes, the voting power should decay proportionally, in which
    /// case one can substitute for `number_days` the number of days remaining
    /// on the lockup.
    ///
    /// ## Daily Vesting Lockup
    ///
    /// Daily vesting can be calculated with simple series sum.
    ///
    /// For the sake of example, suppose we locked up 10 tokens for two days,
    /// vesting linearly once a day. In other words, we have 5 tokens locked for
    /// 1 day and 5 tokens locked for two days.
    ///
    /// Visually, we can see this in a two year timeline
    ///
    /// 0      5      10   amount unlocked
    /// | ---- | ---- |
    /// 0      1      2   days
    ///
    /// Then, to calculate the voting power at any time in the first day, we
    /// have
    ///
    /// ```
    /// voting_power = 1/2555 * 5 + 2/2555 * 5
    /// ```
    ///
    /// Notice the scalar multipliers used to normalize the amounts.
    /// Since 7 years is the maximum lock, and 1 day is the minimum, we have
    /// a scalar of 1/2555 for a one day lock, 2/2555 for a two day lock,
    /// 2555/2555 for a 7 year lock, and 0 for no lock.
    ///
    /// We can rewrite the equation above as
    ///
    /// ```
    /// voting_power = 1/2555 * 5 + 2/2555 * 5
    ///              = 1/2555 * 10/2 + 2/2555 * 10/2
    /// ```
    ///
    /// Let's now generalize this to a daily vesting schedule over seven years.
    /// Let "amount" be the total amount for vesting. Then the total voting
    /// power to start is
    ///
    /// ```
    /// voting_power = 1/2555*(amount/2555) + 2/2555*(amount/2555) + ... + (2555/2555)*(amount/2555)
    ///              = 1/2555 * [1*(amount/2555) + 2*(amount/2555) + ... + 2555*(amount/255)]
    ///              = (1/2555) * (amount/2555) * (1 + 2 + ... + 2555)
    ///              = (1/2555) * (amount/2555) * [(2555 * [2555 + 1]) / 2]
    ///              = (1 / m) * (amount / n) * [(n * [n + 1]) / 2],
    /// ```
    ///
    /// where `m` is the max number of lockup days and `n` is the number of
    /// days for the entire vesting schedule.
    ///
    /// ### Decay
    ///
    /// To calculate the decay, we can simply re-use the above sum to caculate
    /// the amount vested from the start up until the current day, and subtract
    /// that from the total.
    ///
    /// ## Voting Power Warmup
    ///
    /// To prevent the case where one borrows tokens to suddenly vote on a
    /// favorable proposal, one can introduce a "warmup" period, where the
    /// lockup calculation doesn't start until a specific date, so that
    /// the voting power of all new depositors remains zero for an initial
    /// period of time, say, two weeks.
    pub fn voting_power(&self) -> u64 {
        let locked_multiplier = 1; // todo
        self.amount * locked_multiplier
    }

    /// Returns the amount of unlocked tokens for this deposit.
    pub fn vested(&self) -> u64 {
        // todo
        self.amount
    }
}

#[zero_copy]
#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct Lockup {
    pub kind: LockupKind,
    pub start_ts: i64,
    pub end_ts: i64,
}

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Debug, Clone, Copy)]
pub enum LockupKind {
    Daily = 1u8 << 0,
    Yearly = 1u8 << 1,
    Cliff = 1u8 << 2,
}

// Error.

#[error]
pub enum ErrorCode {
    #[msg("Exchange rate must be greater than zero")]
    InvalidRate,
    #[msg("")]
    RatesFull,
    #[msg("")]
    ExchangeRateEntryNotFound,
    #[msg("")]
    DepositEntryNotFound,
    DepositEntryFull,
    VotingTokenNonZero,
    InvalidDepositId,
    InsufficientVestedTokens,
}
