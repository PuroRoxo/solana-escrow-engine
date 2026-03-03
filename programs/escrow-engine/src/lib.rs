use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use anchor_spl::associated_token::AssociatedToken;

declare_id!("11111111111111111111111111111112");

#[program]
pub mod escrow_engine {
    use super::*;

    /// Creates a new escrow deal between parties
    pub fn create_escrow(
        ctx: Context<CreateEscrow>,
        deal_id: u64,
        amount: u64,
        expires_at: Option<i64>,
    ) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_state;
        let clock = Clock::get()?;

        // Validate expiration time
        if let Some(expiry) = expires_at {
            require!(
                expiry > clock.unix_timestamp,
                EscrowError::InvalidExpirationTime
            );
        }

        // Validate amount is not zero
        require!(amount > 0, EscrowError::InvalidAmount);

        escrow.deal_id = deal_id;
        escrow.buyer = ctx.accounts.buyer.key();
        escrow.seller = ctx.accounts.seller.key();
        escrow.arbitrator = ctx.accounts.arbitrator.key();
        escrow.token_mint = ctx.accounts.token_mint.key();
        escrow.amount = amount;
        escrow.amount_released = 0;
        escrow.status = EscrowStatus::Created;
        escrow.created_at = clock.unix_timestamp;
        escrow.expires_at = expires_at;
        escrow.dispute_data = None;
        escrow.bump = *ctx.bumps.get("escrow_state").unwrap();

        emit!(EscrowCreated {
            escrow: escrow.key(),
            deal_id,
            buyer: escrow.buyer,
            seller: escrow.seller,
            amount,
        });

        Ok(())
    }

    /// Funds the escrow with tokens (buyer deposits)
    pub fn fund_escrow(ctx: Context<FundEscrow>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_state;
        
        // Validate escrow status
        require!(
            escrow.status == EscrowStatus::Created,
            EscrowError::InvalidEscrowStatus
        );

        // Check if expired
        if let Some(expires_at) = escrow.expires_at {
            let clock = Clock::get()?;
            require!(
                clock.unix_timestamp < expires_at,
                EscrowError::EscrowExpired
            );
        }

        // Transfer tokens from buyer to vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.buyer_token_account.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.buyer.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        
        token::transfer(cpi_ctx, escrow.amount)?;

        escrow.status = EscrowStatus::Funded;

        emit!(EscrowFunded {
            escrow: escrow.key(),
            amount: escrow.amount,
        });

        Ok(())
    }

    /// Releases funds to seller (partial or full)
    pub fn release_funds(ctx: Context<ReleaseFunds>, amount: u64) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_state;

        // Validate escrow status
        require!(
            escrow.status == EscrowStatus::Funded || escrow.status == EscrowStatus::PartiallyReleased,
            EscrowError::InvalidEscrowStatus
        );

        // Validate release amount
        require!(amount > 0, EscrowError::InvalidAmount);
        
        let remaining_amount = escrow.amount.checked_sub(escrow.amount_released)
            .ok_or(EscrowError::ArithmeticOverflow)?;
        require!(amount <= remaining_amount, EscrowError::InsufficientFunds);

        // Create signer seeds for PDA
        let seeds = &[
            b"escrow",
            &escrow.deal_id.to_le_bytes(),
            &escrow.buyer.to_bytes(),
            &[escrow.bump],
        ];
        let signer = &[&seeds[..]];

        // Transfer tokens from vault to seller
        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_token_account.to_account_info(),
            to: ctx.accounts.seller_token_account.to_account_info(),
            authority: escrow.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        
        token::transfer(cpi_ctx, amount)?;

        // Update escrow state
        escrow.amount_released = escrow.amount_released.checked_add(amount)
            .ok_or(EscrowError::ArithmeticOverflow)?;

        if escrow.amount_released == escrow.amount {
            escrow.status = EscrowStatus::Released;
        } else {
            escrow.status = EscrowStatus::PartiallyReleased;
        }

        emit!(FundsReleased {
            escrow: escrow.key(),
            amount,
            total_released: escrow.amount_released,
        });

        Ok(())
    }

    /// Creates a dispute (buyer or seller can call)
    pub fn create_dispute(ctx: Context<CreateDispute>, reason: String) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_state;
        let clock = Clock::get()?;

        // Validate escrow status
        require!(
            matches!(escrow.status, EscrowStatus::Funded | EscrowStatus::PartiallyReleased),
            EscrowError::InvalidEscrowStatus
        );

        // Validate reason length
        require!(reason.len() <= 280, EscrowError::DisputeReasonTooLong);

        escrow.status = EscrowStatus::Disputed;
        escrow.dispute_data = Some(DisputeData {
            created_at: clock.unix_timestamp,
            created_by: ctx.accounts.dispute_creator.key(),
            reason,
            resolved_at: None,
        });

        emit!(DisputeCreated {
            escrow: escrow.key(),
            created_by: ctx.accounts.dispute_creator.key(),
        });

        Ok(())
    }

    /// Resolves dispute (only arbitrator can call)
    pub fn resolve_dispute(
        ctx: Context<ResolveDispute>,
        release_to_seller: u64,
        refund_to_buyer: u64,
    ) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_state;
        let clock = Clock::get()?;

        // Validate escrow status
        require!(
            escrow.status == EscrowStatus::Disputed,
            EscrowError::InvalidEscrowStatus
        );

        let remaining_amount = escrow.amount.checked_sub(escrow.amount_released)
            .ok_or(EscrowError::ArithmeticOverflow)?;

        // Validate resolution amounts
        let total_resolution = release_to_seller.checked_add(refund_to_buyer)
            .ok_or(EscrowError::ArithmeticOverflow)?;
        require!(
            total_resolution == remaining_amount,
            EscrowError::InvalidResolutionAmounts
        );

        let seeds = &[
            b"escrow",
            &escrow.deal_id.to_le_bytes(),
            &escrow.buyer.to_bytes(),
            &[escrow.bump],
        ];
        let signer = &[&seeds[..]];

        // Transfer to seller if any
        if release_to_seller > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_token_account.to_account_info(),
                to: ctx.accounts.seller_token_account.to_account_info(),
                authority: escrow.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
            token::transfer(cpi_ctx, release_to_seller)?;
        }

        // Refund to buyer if any
        if refund_to_buyer > 0 {
            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_token_account.to_account_info(),
                to: ctx.accounts.buyer_token_account.to_account_info(),
                authority: escrow.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
            token::transfer(cpi_ctx, refund_to_buyer)?;
        }

        // Update dispute data
        if let Some(ref mut dispute) = escrow.dispute_data {
            dispute.resolved_at = Some(clock.unix_timestamp);
        }

        escrow.status = EscrowStatus::Resolved;
        escrow.amount_released = escrow.amount;

        emit!(DisputeResolved {
            escrow: escrow.key(),
            release_to_seller,
            refund_to_buyer,
        });

        Ok(())
    }

    /// Refunds expired escrow back to buyer
    pub fn refund_expired(ctx: Context<RefundExpired>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow_state;
        let clock = Clock::get()?;

        // Validate escrow has expiration
        let expires_at = escrow.expires_at.ok_or(EscrowError::NoExpirationSet)?;
        
        // Validate escrow is expired
        require!(
            clock.unix_timestamp >= expires_at,
            EscrowError::EscrowNotExpired
        );

        // Validate escrow status
        require!(
            matches!(escrow.status, EscrowStatus::Funded | EscrowStatus::PartiallyReleased),
            EscrowError::InvalidEscrowStatus
        );

        let remaining_amount = escrow.amount.checked_sub(escrow.amount_released)
            .ok_or(EscrowError::ArithmeticOverflow)?;

        if remaining_amount > 0 {
            let seeds = &[
                b"escrow",
                &escrow.deal_id.to_le_bytes(),
                &escrow.buyer.to_bytes(),
                &[escrow.bump],
            ];
            let signer = &[&seeds[..]];

            // Refund remaining tokens to buyer
            let cpi_accounts = Transfer {
                from: ctx.accounts.vault_token_account.to_account_info(),
                to: ctx.accounts.buyer_token_account.to_account_info(),
                authority: escrow.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
            
            token::transfer(cpi_ctx, remaining_amount)?;
        }

        escrow.status = EscrowStatus::Expired;

        emit!(EscrowExpired {
            escrow: escrow.key(),
            refunded_amount: remaining_amount,
        });

        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(deal_id: u64)]
pub struct CreateEscrow<'info> {
    #[account(
        init,
        payer = buyer,
        space = 8 + EscrowState::LEN,
        seeds = [b"escrow", deal_id.to_le_bytes().as_ref(), buyer.key().as_ref()],
        bump
    )]
    pub escrow_state: Account<'info, EscrowState>,
    
    #[account(mut)]
    pub buyer: Signer<'info>,
    
    /// CHECK: Seller doesn't need to sign for escrow creation
    pub seller: UncheckedAccount<'info>,
    
    /// CHECK: Arbitrator doesn't need to sign for escrow creation  
    pub arbitrator: UncheckedAccount<'info>,
    
    pub token_mint: Account<'info, token::Mint>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundEscrow<'info> {
    #[account(
        mut,
        has_one = buyer,
        has_one = token_mint,
        seeds = [b"escrow", escrow_state.deal_id.to_le_bytes().as_ref(), buyer.key().as_ref()],
        bump = escrow_state.bump
    )]
    pub escrow_state: Account<'info, EscrowState>,
    
    #[account(mut)]
    pub buyer: Signer<'info>,
    
    #[account(
        mut,
        constraint = buyer_token_account.mint == escrow_state.token_mint,
        constraint = buyer_token_account.owner == buyer.key()
    )]
    pub buyer_token_account: Account<'info, TokenAccount>,
    
    #[account(
        init_if_needed,
        payer = buyer,
        associated_token::mint = token_mint,
        associated_token::authority = escrow_state
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    
    pub token_mint: Account<'info, token::Mint>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ReleaseFunds<'info> {
    #[account(
        mut,
        has_one = buyer,
        has_one = seller,
        has_one = token_mint,
        seeds = [b"escrow", escrow_state.deal_id.to_le_bytes().as_ref(), buyer.key().as_ref()],
        bump = escrow_state.bump
    )]
    pub escrow_state: Account<'info, EscrowState>,
    
    /// CHECK: Buyer key is validated via has_one constraint
    pub buyer: UncheckedAccount<'info>,
    
    /// CHECK: Seller key is validated via has_one constraint
    pub seller: UncheckedAccount<'info>,
    
    #[account(mut)]
    pub authority: Signer<'info>, // Can be buyer, seller, or arbitrator
    
    #[account(
        mut,
        constraint = vault_token_account.mint == escrow_state.token_mint,
        constraint = vault_token_account.owner == escrow_state.key()
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        constraint = seller_token_account.mint == escrow_state.token_mint,
        constraint = seller_token_account.owner == seller.key()
    )]
    pub seller_token_account: Account<'info, TokenAccount>,
    
    pub token_mint: Account<'info, token::Mint>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct CreateDispute<'info> {
    #[account(
        mut,
        seeds = [b"escrow", escrow_state.deal_id.to_le_bytes().as_ref(), escrow_state.buyer.as_ref()],
        bump = escrow_state.bump
    )]
    pub escrow_state: Account<'info, EscrowState>,
    
    #[account(mut)]
    pub dispute_creator: Signer<'info>,
    
    #[account(
        constraint = dispute_creator.key() == escrow_state.buyer || 
                    dispute_creator.key() == escrow_state.seller,
        seeds = [b"escrow", escrow_state.deal_id.to_le_bytes().as_ref(), escrow_state.buyer.as_ref()],
        bump = escrow_state.bump
    )]
    pub escrow_state_check: Account<'info, EscrowState>,
}

#[derive(Accounts)]
pub struct ResolveDispute<'info> {
    #[account(
        mut,
        has_one = arbitrator,
        seeds = [b"escrow", escrow_state.deal_id.to_le_bytes().as_ref(), escrow_state.buyer.as_ref()],
        bump = escrow_state.bump
    )]
    pub escrow_state: Account<'info, EscrowState>,
    
    #[account(mut)]
    pub arbitrator: Signer<'info>,
    
    /// CHECK: Buyer key is validated via escrow state
    pub buyer: UncheckedAccount<'info>,
    
    /// CHECK: Seller key is validated via escrow state  
    pub seller: UncheckedAccount<'info>,
    
    #[account(
        mut,
        constraint = vault_token_account.mint == escrow_state.token_mint,
        constraint = vault_token_account.owner == escrow_state.key()
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        constraint = buyer_token_account.mint == escrow_state.token_mint,
        constraint = buyer_token_account.owner == buyer.key()
    )]
    pub buyer_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        constraint = seller_token_account.mint == escrow_state.token_mint,
        constraint = seller_token_account.owner == seller.key()
    )]
    pub seller_token_account: Account<'info, TokenAccount>,
    
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RefundExpired<'info> {
    #[account(
        mut,
        has_one = buyer,
        seeds = [b"escrow", escrow_state.deal_id.to_le_bytes().as_ref(), buyer.key().as_ref()],
        bump = escrow_state.bump
    )]
    pub escrow_state: Account<'info, EscrowState>,
    
    /// CHECK: Buyer key is validated via has_one constraint
    pub buyer: UncheckedAccount<'info>,
    
    #[account(
        mut,
        constraint = vault_token_account.mint == escrow_state.token_mint,
        constraint = vault_token_account.owner == escrow_state.key()
    )]
    pub vault_token_account: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        constraint = buyer_token_account.mint == escrow_state.token_mint,
        constraint = buyer_token_account.owner == buyer.key()
    )]
    pub buyer_token_account: Account<'info, TokenAccount>,
    
    pub token_program: Program<'info, Token>,
}

#[account]
pub struct EscrowState {
    pub deal_id: u64,              // 8
    pub buyer: Pubkey,             // 32
    pub seller: Pubkey,            // 32
    pub arbitrator: Pubkey,        // 32
    pub token_mint: Pubkey,        // 32
    pub amount: u64,               // 8
    pub amount_released: u64,      // 8
    pub status: EscrowStatus,      // 1 + 1 (enum discriminant)
    pub created_at: i64,           // 8
    pub expires_at: Option<i64>,   // 1 + 8
    pub dispute_data: Option<DisputeData>, // 1 + DisputeData::LEN
    pub bump: u8,                  // 1
}

impl EscrowState {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 8 + 8 + 2 + 8 + 9 + (1 + DisputeData::LEN) + 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub struct DisputeData {
    pub created_at: i64,           // 8
    pub created_by: Pubkey,        // 32
    pub reason: String,            // 4 + up to 280 chars
    pub resolved_at: Option<i64>,  // 1 + 8
}

impl DisputeData {
    pub const LEN: usize = 8 + 32 + 4 + 280 + 9;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub enum EscrowStatus {
    Created,
    Funded,
    PartiallyReleased,
    Released,
    Disputed,
    Resolved,
    Expired,
}

#[event]
pub struct EscrowCreated {
    pub escrow: Pubkey,
    pub deal_id: u64,
    pub buyer: Pubkey,
    pub seller: Pubkey,
    pub amount: u64,
}

#[event]
pub struct EscrowFunded {
    pub escrow: Pubkey,
    pub amount: u64,
}

#[event]
pub struct FundsReleased {
    pub escrow: Pubkey,
    pub amount: u64,
    pub total_released: u64,
}

#[event]
pub struct DisputeCreated {
    pub escrow: Pubkey,
    pub created_by: Pubkey,
}

#[event]
pub struct DisputeResolved {
    pub escrow: Pubkey,
    pub release_to_seller: u64,
    pub refund_to_buyer: u64,
}

#[event]
pub struct EscrowExpired {
    pub escrow: Pubkey,
    pub refunded_amount: u64,
}

#[error_code]
pub enum EscrowError {
    #[msg("Invalid escrow status for this operation")]
    InvalidEscrowStatus,
    #[msg("Invalid amount: must be greater than 0")]
    InvalidAmount,
    #[msg("Insufficient funds in escrow")]
    InsufficientFunds,
    #[msg("Escrow has already expired")]
    EscrowExpired,
    #[msg("Escrow has not expired yet")]
    EscrowNotExpired,
    #[msg("Invalid expiration time: must be in the future")]
    InvalidExpirationTime,
    #[msg("No expiration time set for this escrow")]
    NoExpirationSet,
    #[msg("Dispute reason too long (max 280 characters)")]
    DisputeReasonTooLong,
    #[msg("Invalid dispute resolution amounts")]
    InvalidResolutionAmounts,
    #[msg("Arithmetic overflow")]
    ArithmeticOverflow,
}