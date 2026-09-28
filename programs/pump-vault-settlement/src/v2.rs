//! Smart Distribution V2: one atomic seller -> vault -> buyer route per tx.
//!
//! V2 deliberately uses independent PDA seeds and state from the retained V1
//! route mode. Tags 7-11 must appear once, adjacent and in order in the same
//! transaction. Tag 12 is the seller-bound defensive recovery path.

mod amm;

use super::{
    checked_add_u64, pump_buy_v2_instruction, pump_sell_v2_instruction, require_key,
    require_pump_buy_accounts_v2, require_pump_sell_accounts_v2, require_signer,
    system_instruction, transfer_from_program_owned, transfer_from_signer_with_system,
    SettlementError,
};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::{
        instructions::{
            id as instructions_sysvar_id, load_current_index_checked, load_instruction_at_checked,
        },
        Sysvar, SysvarSerialize,
    },
};

#[cfg(not(test))]
use solana_program::instruction::{AccountMeta, Instruction};

pub const INIT_DISTRIBUTION_V2_TAG: u8 = 7;
pub const SELL_TO_VAULT_V2_TAG: u8 = 8;
pub const SETTLE_VAULT_TO_BUYER_V2_TAG: u8 = 9;
pub const BUYBACK_V2_TAG: u8 = 10;
pub const FINALIZE_DISTRIBUTION_V2_TAG: u8 = 11;
pub const REFUND_DISTRIBUTION_V2_TAG: u8 = 12;
/// Additive guarded buyback. Legacy tag 10 retains its zero-minimum contract.
pub const BUYBACK_V3_TAG: u8 = 20;
pub const SELL_AMM_V3_TAG: u8 = 21;
pub const BUY_AMM_V3_TAG: u8 = 22;

const ROUTE_V2_SEED: &[u8] = b"route-v2";
const VAULT_V2_SEED: &[u8] = b"vault-v2";
const STATE_V2_MAGIC: &[u8; 8] = b"SDSTV200";
const STATE_V2_VERSION: u8 = 1;
const STATUS_INITIALIZED: u8 = 1;
const STATUS_SOLD: u8 = 2;
const STATUS_FUNDED: u8 = 3;
const STATUS_BOUGHT: u8 = 4;
const STATUS_TERMINAL: u8 = 5;
const V2_SEQUENCE_LEN: usize = 5;
const MAX_TOP_LEVEL_INSTRUCTIONS: usize = 128;

const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DistributionV2State {
    status: u8,
    bump: u8,
    vault_bump: u8,
    close_run_created_wsol: bool,
    seller: Pubkey,
    buyer: Pubkey,
    mint: Pubkey,
    settlement_id: [u8; 32],
    expires_slot: u64,
    sell_amount_raw: u64,
    quoted_buy_amount_raw: u64,
    seller_pre_sell_lamports: u64,
    buyer_baseline_lamports: u64,
    vault_funded_lamports: u64,
    buyer_funded_lamports: u64,
    actual_spent_lamports: u64,
    residual_lamports: u64,
}

impl DistributionV2State {
    // magic + version/status/bumps/cleanup/reserved + three pubkeys +
    // settlement id + nine u64 accounting fields.
    const LEN: usize = 8 + 8 + (32 * 4) + (8 * 9);

    fn pack(&self, data: &mut [u8]) -> ProgramResult {
        if data.len() != Self::LEN {
            return Err(SettlementError::AccountTooSmall.into());
        }
        data.fill(0);
        let mut offset = 0;
        data[offset..offset + 8].copy_from_slice(STATE_V2_MAGIC);
        offset += 8;
        data[offset] = STATE_V2_VERSION;
        data[offset + 1] = self.status;
        data[offset + 2] = self.bump;
        data[offset + 3] = self.vault_bump;
        data[offset + 4] = u8::from(self.close_run_created_wsol);
        offset += 8;
        write_pubkey(data, &mut offset, &self.seller);
        write_pubkey(data, &mut offset, &self.buyer);
        write_pubkey(data, &mut offset, &self.mint);
        data[offset..offset + 32].copy_from_slice(&self.settlement_id);
        offset += 32;
        for value in [
            self.expires_slot,
            self.sell_amount_raw,
            self.quoted_buy_amount_raw,
            self.seller_pre_sell_lamports,
            self.buyer_baseline_lamports,
            self.vault_funded_lamports,
            self.buyer_funded_lamports,
            self.actual_spent_lamports,
            self.residual_lamports,
        ] {
            write_u64(data, &mut offset, value);
        }
        Ok(())
    }

    fn unpack(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() != Self::LEN || &data[..8] != STATE_V2_MAGIC {
            return Err(SettlementError::InvalidRoute.into());
        }
        if data[8] != STATE_V2_VERSION || data[13..16] != [0, 0, 0] {
            return Err(SettlementError::InvalidRoute.into());
        }
        let status = data[9];
        if !(STATUS_INITIALIZED..=STATUS_TERMINAL).contains(&status) {
            return Err(SettlementError::InvalidStatus.into());
        }
        let bump = data[10];
        let vault_bump = data[11];
        let close_run_created_wsol = match data[12] {
            0 => false,
            1 => true,
            _ => return Err(SettlementError::InvalidRoute.into()),
        };
        let mut offset = 16;
        let seller = read_pubkey(data, &mut offset)?;
        let buyer = read_pubkey(data, &mut offset)?;
        let mint = read_pubkey(data, &mut offset)?;
        let mut settlement_id = [0_u8; 32];
        settlement_id.copy_from_slice(&data[offset..offset + 32]);
        offset += 32;
        Ok(Self {
            status,
            bump,
            vault_bump,
            close_run_created_wsol,
            seller,
            buyer,
            mint,
            settlement_id,
            expires_slot: read_u64(data, &mut offset)?,
            sell_amount_raw: read_u64(data, &mut offset)?,
            quoted_buy_amount_raw: read_u64(data, &mut offset)?,
            seller_pre_sell_lamports: read_u64(data, &mut offset)?,
            buyer_baseline_lamports: read_u64(data, &mut offset)?,
            vault_funded_lamports: read_u64(data, &mut offset)?,
            buyer_funded_lamports: read_u64(data, &mut offset)?,
            actual_spent_lamports: read_u64(data, &mut offset)?,
            residual_lamports: read_u64(data, &mut offset)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitV2Args {
    settlement_id: [u8; 32],
    expires_slot: u64,
    sell_amount_raw: u64,
    close_run_created_wsol: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SellV2Args {
    settlement_id: [u8; 32],
    sell_amount_raw: u64,
    min_sol_output_raw: u64,
    max_deposit_lamports: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BuybackV2Args {
    settlement_id: [u8; 32],
    min_buy_amount_raw: u64,
}

pub fn process_v2_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let tag = *input.first().ok_or(SettlementError::InvalidInstruction)?;
    match tag {
        INIT_DISTRIBUTION_V2_TAG => process_init(program_id, accounts, unpack_init(input)?),
        SELL_TO_VAULT_V2_TAG => process_sell(program_id, accounts, unpack_sell(input)?),
        SETTLE_VAULT_TO_BUYER_V2_TAG => {
            process_settle(program_id, accounts, unpack_route_only(input, tag)?)
        }
        BUYBACK_V2_TAG => process_buyback(program_id, accounts, unpack_buyback(input)?),
        SELL_AMM_V3_TAG => amm::process_sell(program_id, accounts, unpack_sell(input)?),
        BUY_AMM_V3_TAG => {
            require_exact_len(input, 57)?;
            amm::process_buy(
                program_id,
                accounts,
                read_settlement_id(input, 1)?,
                read_u64_at(input, 33)?,
                read_u64_at(input, 41)?,
                read_u64_at(input, 49)?,
            )
        }
        BUYBACK_V3_TAG => {
            require_exact_len(input, 49)?;
            process_buyback_guarded(
                program_id,
                accounts,
                BuybackV2Args {
                    settlement_id: read_settlement_id(input, 1)?,
                    min_buy_amount_raw: read_u64_at(input, 33)?,
                },
                Some(read_u64_at(input, 41)?),
            )
        }
        FINALIZE_DISTRIBUTION_V2_TAG => {
            process_finalize(program_id, accounts, unpack_route_only(input, tag)?)
        }
        REFUND_DISTRIBUTION_V2_TAG => {
            process_refund(program_id, accounts, unpack_route_only(input, tag)?)
        }
        _ => Err(SettlementError::InvalidInstruction.into()),
    }
}

fn process_init(program_id: &Pubkey, accounts: &[AccountInfo], args: InitV2Args) -> ProgramResult {
    let iter = &mut accounts.iter();
    let seller = next_account_info(iter)?;
    let buyer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let state = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let rent_sysvar = next_account_info(iter)?;
    let instructions_sysvar = next_account_info(iter)?;

    require_signer(seller)?;
    require_signer(buyer)?;
    require_key(system_program.key, &solana_system_interface::program::id())?;
    if seller.key == buyer.key || args.sell_amount_raw == 0 {
        return Err(SettlementError::SameSellerBuyer.into());
    }
    let (expected_state, state_bump) = route_state_pda_v2(
        program_id,
        seller.key,
        buyer.key,
        mint.key,
        &args.settlement_id,
    );
    let (expected_vault, vault_bump) = route_vault_pda_v2(program_id, &expected_state);
    require_key(state.key, &expected_state)?;
    require_key(vault.key, &expected_vault)?;
    require_atomic_sequence(
        program_id,
        instructions_sysvar,
        INIT_DISTRIBUTION_V2_TAG,
        &args.settlement_id,
        seller.key,
        buyer.key,
        mint.key,
        state.key,
        vault.key,
        args.close_run_created_wsol,
    )?;

    let rent = Rent::from_account_info(rent_sysvar)?;
    let state_bump_seed = [state_bump];
    let state_seeds: &[&[u8]] = &[
        ROUTE_V2_SEED,
        seller.key.as_ref(),
        buyer.key.as_ref(),
        mint.key.as_ref(),
        &args.settlement_id,
        &state_bump_seed,
    ];
    let vault_bump_seed = [vault_bump];
    let vault_seeds: &[&[u8]] = &[VAULT_V2_SEED, state.key.as_ref(), &vault_bump_seed];
    if state.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(
                seller.key,
                state.key,
                rent.minimum_balance(DistributionV2State::LEN),
                DistributionV2State::LEN as u64,
                program_id,
            ),
            &[seller.clone(), state.clone(), system_program.clone()],
            &[state_seeds],
        )?;
    }
    if state.owner != program_id || state.data_len() != DistributionV2State::LEN {
        return Err(SettlementError::InvalidRoute.into());
    }
    if state.try_borrow_data()?.iter().any(|byte| *byte != 0) {
        return Err(SettlementError::InvalidStatus.into());
    }
    if vault.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(seller.key, vault.key, 0, 0, program_id),
            &[seller.clone(), vault.clone(), system_program.clone()],
            &[vault_seeds],
        )?;
    }
    if vault.owner != program_id || vault.data_len() != 0 {
        return Err(SettlementError::InvalidRoute.into());
    }
    let clock = Clock::get()?;
    require_not_expired(clock.slot, args.expires_slot)?;
    DistributionV2State {
        status: STATUS_INITIALIZED,
        bump: state_bump,
        vault_bump,
        close_run_created_wsol: args.close_run_created_wsol,
        seller: *seller.key,
        buyer: *buyer.key,
        mint: *mint.key,
        settlement_id: args.settlement_id,
        expires_slot: args.expires_slot,
        sell_amount_raw: args.sell_amount_raw,
        quoted_buy_amount_raw: 0,
        seller_pre_sell_lamports: seller.lamports(),
        buyer_baseline_lamports: buyer.lamports(),
        vault_funded_lamports: 0,
        buyer_funded_lamports: 0,
        actual_spent_lamports: 0,
        residual_lamports: 0,
    }
    .pack(&mut state.try_borrow_mut_data()?)?;
    msg!("Smart Distribution: V2 init");
    Ok(())
}

fn require_sell_limits(expected_sell_amount_raw: u64, args: &SellV2Args) -> ProgramResult {
    if args.sell_amount_raw != expected_sell_amount_raw {
        return Err(SettlementError::InvalidRoute.into());
    }
    if args.min_sol_output_raw == 0 {
        return Err(SettlementError::PositiveSellMinimumRequired.into());
    }
    if args.min_sol_output_raw > args.max_deposit_lamports {
        return Err(SettlementError::InvalidRoute.into());
    }
    Ok(())
}

fn validated_sell_proceeds(
    before: u64,
    after: u64,
    min_sol_output_raw: u64,
    max_deposit_lamports: u64,
) -> Result<u64, ProgramError> {
    let proceeds = after
        .checked_sub(before)
        .ok_or(SettlementError::SlippageExceeded)?;
    if proceeds < min_sol_output_raw {
        return Err(SettlementError::SlippageExceeded.into());
    }
    if proceeds > max_deposit_lamports {
        return Err(SettlementError::DepositCapExceeded.into());
    }
    Ok(proceeds)
}

fn process_sell(program_id: &Pubkey, accounts: &[AccountInfo], args: SellV2Args) -> ProgramResult {
    let iter = &mut accounts.iter();
    let seller = next_account_info(iter)?;
    let buyer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let state = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let clock_sysvar = next_account_info(iter)?;
    let instructions_sysvar = next_account_info(iter)?;
    require_signer(seller)?;
    require_signer(buyer)?;
    let mut route = load_route_v2(
        program_id,
        state,
        vault,
        seller.key,
        buyer.key,
        mint.key,
        &args.settlement_id,
    )?;
    require_status(route.status, STATUS_INITIALIZED)?;
    require_not_expired(
        Clock::from_account_info(clock_sysvar)?.slot,
        route.expires_slot,
    )?;
    require_atomic_sequence_for_state(
        program_id,
        instructions_sysvar,
        SELL_TO_VAULT_V2_TAG,
        state,
        vault,
        &route,
    )?;
    require_sell_limits(route.sell_amount_raw, &args)?;
    if seller.lamports() != route.seller_pre_sell_lamports || args.max_deposit_lamports == 0 {
        return Err(SettlementError::BaselineMismatch.into());
    }
    let pump_accounts: Vec<AccountInfo> = iter.cloned().collect();
    require_pump_sell_accounts_v2(&pump_accounts, seller.key, mint.key)?;
    let before = seller.lamports();
    let ix = pump_sell_v2_instruction(
        &pump_accounts,
        args.sell_amount_raw,
        args.min_sol_output_raw,
    )?;
    super::invoke_pump(&ix, &pump_accounts)?;
    let proceeds = validated_sell_proceeds(
        before,
        seller.lamports(),
        args.min_sol_output_raw,
        args.max_deposit_lamports,
    )?;
    transfer_from_signer_with_system(seller, vault, &pump_accounts[23], proceeds)?;
    route.status = STATUS_SOLD;
    route.vault_funded_lamports = proceeds;
    route.pack(&mut state.try_borrow_mut_data()?)?;
    msg!("Smart Distribution: V2 sell {} lamports", proceeds);
    Ok(())
}

fn process_settle(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    settlement_id: [u8; 32],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let seller = next_account_info(iter)?;
    let buyer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let state = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let clock_sysvar = next_account_info(iter)?;
    let instructions_sysvar = next_account_info(iter)?;
    require_signer(seller)?;
    require_signer(buyer)?;
    let mut route = load_route_v2(
        program_id,
        state,
        vault,
        seller.key,
        buyer.key,
        mint.key,
        &settlement_id,
    )?;
    require_status(route.status, STATUS_SOLD)?;
    require_not_expired(
        Clock::from_account_info(clock_sysvar)?.slot,
        route.expires_slot,
    )?;
    require_atomic_sequence_for_state(
        program_id,
        instructions_sysvar,
        SETTLE_VAULT_TO_BUYER_V2_TAG,
        state,
        vault,
        &route,
    )?;
    if vault.lamports() != route.vault_funded_lamports
        || buyer.lamports() != route.buyer_baseline_lamports
    {
        return Err(SettlementError::BaselineMismatch.into());
    }
    transfer_from_program_owned(vault, buyer, route.vault_funded_lamports)?;
    route.status = STATUS_FUNDED;
    route.buyer_funded_lamports = route.vault_funded_lamports;
    route.pack(&mut state.try_borrow_mut_data()?)?;
    msg!(
        "Smart Distribution: V2 settle {} lamports",
        route.buyer_funded_lamports
    );
    Ok(())
}

fn process_buyback(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: BuybackV2Args,
) -> ProgramResult {
    process_buyback_guarded(program_id, accounts, args, None)
}

fn process_buyback_guarded(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: BuybackV2Args,
    quote_expires_slot: Option<u64>,
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let seller = next_account_info(iter)?;
    let buyer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let state = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let clock_sysvar = next_account_info(iter)?;
    let instructions_sysvar = next_account_info(iter)?;
    require_signer(seller)?;
    require_signer(buyer)?;
    let mut route = load_route_v2(
        program_id,
        state,
        vault,
        seller.key,
        buyer.key,
        mint.key,
        &args.settlement_id,
    )?;
    require_status(route.status, STATUS_FUNDED)?;
    require_not_expired(
        Clock::from_account_info(clock_sysvar)?.slot,
        route.expires_slot,
    )?;
    require_atomic_sequence_for_state(
        program_id,
        instructions_sysvar,
        if quote_expires_slot.is_some() {
            BUYBACK_V3_TAG
        } else {
            BUYBACK_V2_TAG
        },
        state,
        vault,
        &route,
    )?;
    if let Some(expires_slot) = quote_expires_slot {
        require_buyback_guard(
            args.min_buy_amount_raw,
            expires_slot,
            route.expires_slot,
            Clock::from_account_info(clock_sysvar)?.slot,
        )?;
    } else if args.min_buy_amount_raw != 0 {
        return Err(SettlementError::ZeroMinimumRequired.into());
    }
    let expected_before =
        checked_add_u64(route.buyer_baseline_lamports, route.buyer_funded_lamports)?;
    if buyer.lamports() != expected_before || vault.lamports() != 0 {
        return Err(SettlementError::BaselineMismatch.into());
    }
    let pump_accounts: Vec<AccountInfo> = iter.cloned().collect();
    require_pump_buy_accounts_v2(&pump_accounts, buyer.key, mint.key)?;
    let quote = super::dynamic_buy_quote(
        &pump_accounts[10].try_borrow_data()?,
        &pump_accounts[0].try_borrow_data()?,
        &pump_accounts[22].try_borrow_data()?,
        route.buyer_funded_lamports,
    )?;
    if quote.amount_raw == 0 {
        return Err(SettlementError::VaultEmpty.into());
    }
    if quote.amount_raw < args.min_buy_amount_raw {
        return Err(SettlementError::SlippageExceeded.into());
    }
    // Pump buy_v2 requests this exact token amount, at least the signed floor.
    // CPI failure rolls back the entire five-instruction route.
    let before = buyer.lamports();
    let ix = pump_buy_v2_instruction(
        &pump_accounts,
        quote.amount_raw,
        route.buyer_funded_lamports,
    )?;
    super::invoke_pump(&ix, &pump_accounts)?;
    let after = buyer.lamports();
    let actual_spent = before
        .checked_sub(after)
        .ok_or(SettlementError::BaselineMismatch)?;
    if actual_spent > route.buyer_funded_lamports || after < route.buyer_baseline_lamports {
        return Err(SettlementError::BaselineMismatch.into());
    }
    route.status = STATUS_BOUGHT;
    route.quoted_buy_amount_raw = quote.amount_raw;
    route.actual_spent_lamports = actual_spent;
    route.residual_lamports = after - route.buyer_baseline_lamports;
    route.pack(&mut state.try_borrow_mut_data()?)?;
    msg!(
        "Smart Distribution: V2 buy spent={} residual={}",
        actual_spent,
        route.residual_lamports
    );
    Ok(())
}

fn process_finalize(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    settlement_id: [u8; 32],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let seller = next_account_info(iter)?;
    let buyer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let state = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let buyer_wsol = next_account_info(iter)?;
    let quote_token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    let instructions_sysvar = next_account_info(iter)?;
    require_signer(seller)?;
    require_signer(buyer)?;
    require_key(quote_token_program.key, &TOKEN_PROGRAM_ID)?;
    require_key(system_program.key, &solana_system_interface::program::id())?;
    let mut route = load_route_v2(
        program_id,
        state,
        vault,
        seller.key,
        buyer.key,
        mint.key,
        &settlement_id,
    )?;
    require_status(route.status, STATUS_BOUGHT)?;
    require_atomic_sequence_for_state(
        program_id,
        instructions_sysvar,
        FINALIZE_DISTRIBUTION_V2_TAG,
        state,
        vault,
        &route,
    )?;
    let expected_wsol = associated_token_address(buyer.key, &TOKEN_PROGRAM_ID, &WSOL_MINT);
    require_key(buyer_wsol.key, &expected_wsol)?;
    let expected_before = checked_add_u64(route.buyer_baseline_lamports, route.residual_lamports)?;
    if buyer.lamports() != expected_before || vault.lamports() != 0 {
        return Err(SettlementError::BaselineMismatch.into());
    }
    if route.residual_lamports > 0 {
        transfer_from_signer_with_system(buyer, seller, system_program, route.residual_lamports)?;
    }
    if route.close_run_created_wsol && buyer_wsol.lamports() > 0 {
        close_token_account_via_signer(buyer_wsol, seller, buyer, quote_token_program)?;
    }
    if buyer.lamports() != route.buyer_baseline_lamports {
        return Err(SettlementError::CleanupMismatch.into());
    }
    route.status = STATUS_TERMINAL;
    route.pack(&mut state.try_borrow_mut_data()?)?;
    let vault_lamports = vault.lamports();
    if vault_lamports > 0 {
        transfer_from_program_owned(vault, seller, vault_lamports)?;
    }
    let state_lamports = state.lamports();
    if state_lamports > 0 {
        transfer_from_program_owned(state, seller, state_lamports)?;
    }
    msg!("Smart Distribution: V2 finalize");
    Ok(())
}

fn process_refund(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    settlement_id: [u8; 32],
) -> ProgramResult {
    let iter = &mut accounts.iter();
    let seller = next_account_info(iter)?;
    let buyer = next_account_info(iter)?;
    let mint = next_account_info(iter)?;
    let state = next_account_info(iter)?;
    let vault = next_account_info(iter)?;
    let buyer_wsol = next_account_info(iter)?;
    let quote_token_program = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;
    require_signer(seller)?;
    require_signer(buyer)?;
    require_key(quote_token_program.key, &TOKEN_PROGRAM_ID)?;
    require_key(system_program.key, &solana_system_interface::program::id())?;
    let route = load_route_v2(
        program_id,
        state,
        vault,
        seller.key,
        buyer.key,
        mint.key,
        &settlement_id,
    )?;
    if route.status == STATUS_TERMINAL {
        return Err(SettlementError::RouteClosed.into());
    }
    let expected_wsol = associated_token_address(buyer.key, &TOKEN_PROGRAM_ID, &WSOL_MINT);
    require_key(buyer_wsol.key, &expected_wsol)?;
    if buyer.lamports() < route.buyer_baseline_lamports {
        return Err(SettlementError::BaselineMismatch.into());
    }
    let buyer_residual = buyer.lamports() - route.buyer_baseline_lamports;
    if buyer_residual > 0 {
        transfer_from_signer_with_system(buyer, seller, system_program, buyer_residual)?;
    }
    if route.close_run_created_wsol && buyer_wsol.lamports() > 0 {
        close_token_account_via_signer(buyer_wsol, seller, buyer, quote_token_program)?;
    }
    let vault_lamports = vault.lamports();
    if vault_lamports > 0 {
        transfer_from_program_owned(vault, seller, vault_lamports)?;
    }
    let state_lamports = state.lamports();
    if state_lamports > 0 {
        transfer_from_program_owned(state, seller, state_lamports)?;
    }
    msg!("Smart Distribution: V2 refund");
    Ok(())
}

fn unpack_init(input: &[u8]) -> Result<InitV2Args, ProgramError> {
    require_exact_len(input, 50)?;
    let settlement_id = read_settlement_id(input, 1)?;
    let close_run_created_wsol = match input[49] {
        0 => false,
        1 => true,
        _ => return Err(SettlementError::InvalidInstruction.into()),
    };
    Ok(InitV2Args {
        settlement_id,
        expires_slot: read_u64_at(input, 33)?,
        sell_amount_raw: read_u64_at(input, 41)?,
        close_run_created_wsol,
    })
}

fn unpack_sell(input: &[u8]) -> Result<SellV2Args, ProgramError> {
    require_exact_len(input, 57)?;
    Ok(SellV2Args {
        settlement_id: read_settlement_id(input, 1)?,
        sell_amount_raw: read_u64_at(input, 33)?,
        min_sol_output_raw: read_u64_at(input, 41)?,
        max_deposit_lamports: read_u64_at(input, 49)?,
    })
}

fn unpack_buyback(input: &[u8]) -> Result<BuybackV2Args, ProgramError> {
    require_exact_len(input, 41)?;
    Ok(BuybackV2Args {
        settlement_id: read_settlement_id(input, 1)?,
        min_buy_amount_raw: read_u64_at(input, 33)?,
    })
}

fn unpack_route_only(input: &[u8], expected_tag: u8) -> Result<[u8; 32], ProgramError> {
    require_exact_len(input, 33)?;
    if input[0] != expected_tag {
        return Err(SettlementError::InvalidInstruction.into());
    }
    read_settlement_id(input, 1)
}

#[allow(clippy::too_many_arguments)]
fn require_atomic_sequence(
    program_id: &Pubkey,
    instructions_sysvar: &AccountInfo,
    current_tag: u8,
    settlement_id: &[u8; 32],
    seller: &Pubkey,
    buyer: &Pubkey,
    mint: &Pubkey,
    state: &Pubkey,
    vault: &Pubkey,
    close_run_created_wsol: bool,
) -> ProgramResult {
    require_key(instructions_sysvar.key, &instructions_sysvar_id())?;
    let current = load_current_index_checked(instructions_sysvar)? as usize;
    let offset = match current_tag {
        SELL_AMM_V3_TAG => 1,
        BUYBACK_V3_TAG | BUY_AMM_V3_TAG => 3,
        INIT_DISTRIBUTION_V2_TAG..=FINALIZE_DISTRIBUTION_V2_TAG => {
            usize::from(current_tag - INIT_DISTRIBUTION_V2_TAG)
        }
        _ => return Err(SettlementError::InvalidSequence.into()),
    };
    let start = current
        .checked_sub(offset)
        .ok_or(SettlementError::InvalidSequence)?;
    let sell_tag = load_instruction_at_checked(start + 1, instructions_sysvar)?
        .data
        .first()
        .copied();
    let buy_tag = load_instruction_at_checked(start + 3, instructions_sysvar)?
        .data
        .first()
        .copied();
    let sequence = match (sell_tag, buy_tag) {
        (Some(8), Some(10)) => [7, 8, 9, 10, 11],
        (Some(8), Some(20)) => [7, 8, 9, 20, 11],
        (Some(21), Some(22)) if !close_run_created_wsol => [7, 21, 9, 22, 11],
        _ => return Err(SettlementError::InvalidSequence.into()),
    };
    let expected_keys = [seller, buyer, mint, state, vault];
    for position in 0..V2_SEQUENCE_LEN {
        let ix = load_instruction_at_checked(start + position, instructions_sysvar)
            .map_err(|_| SettlementError::InvalidSequence)?;
        let tag_matches = ix.data.first().copied() == Some(sequence[position]);
        if ix.program_id != *program_id
            || !tag_matches
            || ix.data.len() < 33
            || read_settlement_id(&ix.data, 1)? != *settlement_id
            || ix.accounts.len() < expected_keys.len()
        {
            return Err(SettlementError::InvalidSequence.into());
        }
        for (meta, expected) in ix.accounts.iter().zip(expected_keys) {
            if meta.pubkey != *expected {
                return Err(SettlementError::InvalidSequence.into());
            }
        }
    }
    if current != start + offset {
        return Err(SettlementError::InvalidSequence.into());
    }
    let mut v2_count = 0_usize;
    for index in 0..MAX_TOP_LEVEL_INSTRUCTIONS {
        let Ok(ix) = load_instruction_at_checked(index, instructions_sysvar) else {
            break;
        };
        if ix.program_id == *program_id {
            let tag = ix
                .data
                .first()
                .copied()
                .ok_or(SettlementError::InvalidSequence)?;
            if !(INIT_DISTRIBUTION_V2_TAG..=FINALIZE_DISTRIBUTION_V2_TAG).contains(&tag)
                && ![BUYBACK_V3_TAG, SELL_AMM_V3_TAG, BUY_AMM_V3_TAG].contains(&tag)
            {
                return Err(SettlementError::InvalidSequence.into());
            }
            v2_count += 1;
        }
    }
    if v2_count != V2_SEQUENCE_LEN {
        return Err(SettlementError::InvalidSequence.into());
    }
    if close_run_created_wsol {
        require_prior_wsol_create(instructions_sysvar, start, seller, buyer)?;
    }
    Ok(())
}

fn require_atomic_sequence_for_state(
    program_id: &Pubkey,
    instructions_sysvar: &AccountInfo,
    current_tag: u8,
    state: &AccountInfo,
    vault: &AccountInfo,
    route: &DistributionV2State,
) -> ProgramResult {
    require_atomic_sequence(
        program_id,
        instructions_sysvar,
        current_tag,
        &route.settlement_id,
        &route.seller,
        &route.buyer,
        &route.mint,
        state.key,
        vault.key,
        route.close_run_created_wsol,
    )
}

fn require_prior_wsol_create(
    instructions_sysvar: &AccountInfo,
    v2_start: usize,
    seller: &Pubkey,
    buyer: &Pubkey,
) -> ProgramResult {
    let expected_ata = associated_token_address(buyer, &TOKEN_PROGRAM_ID, &WSOL_MINT);
    for index in 0..v2_start {
        let ix = load_instruction_at_checked(index, instructions_sysvar)
            .map_err(|_| SettlementError::InvalidSequence)?;
        if ix.program_id == ASSOCIATED_TOKEN_PROGRAM_ID
            && matches!(ix.data.as_slice(), [] | [0] | [1])
            && ix.accounts.len() >= 6
            && ix.accounts[0].pubkey == *seller
            && ix.accounts[1].pubkey == expected_ata
            && ix.accounts[2].pubkey == *buyer
            && ix.accounts[3].pubkey == WSOL_MINT
            && ix.accounts[5].pubkey == TOKEN_PROGRAM_ID
        {
            return Ok(());
        }
    }
    Err(SettlementError::CleanupMismatch.into())
}

fn load_route_v2(
    program_id: &Pubkey,
    state: &AccountInfo,
    vault: &AccountInfo,
    seller: &Pubkey,
    buyer: &Pubkey,
    mint: &Pubkey,
    settlement_id: &[u8; 32],
) -> Result<DistributionV2State, ProgramError> {
    if state.owner != program_id || vault.owner != program_id || vault.data_len() != 0 {
        return Err(ProgramError::IncorrectProgramId);
    }
    let route = DistributionV2State::unpack(&state.try_borrow_data()?)?;
    if route.seller != *seller
        || route.buyer != *buyer
        || route.mint != *mint
        || route.settlement_id != *settlement_id
        || seller == buyer
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    let (expected_state, bump) = route_state_pda_v2(program_id, seller, buyer, mint, settlement_id);
    let (expected_vault, vault_bump) = route_vault_pda_v2(program_id, &expected_state);
    if route.bump != bump || route.vault_bump != vault_bump {
        return Err(SettlementError::InvalidPda.into());
    }
    require_key(state.key, &expected_state)?;
    require_key(vault.key, &expected_vault)?;
    Ok(route)
}

fn route_state_pda_v2(
    program_id: &Pubkey,
    seller: &Pubkey,
    buyer: &Pubkey,
    mint: &Pubkey,
    settlement_id: &[u8; 32],
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            ROUTE_V2_SEED,
            seller.as_ref(),
            buyer.as_ref(),
            mint.as_ref(),
            settlement_id,
        ],
        program_id,
    )
}

fn route_vault_pda_v2(program_id: &Pubkey, state: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_V2_SEED, state.as_ref()], program_id)
}

fn associated_token_address(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

#[cfg(not(test))]
fn close_token_account_via_signer<'a>(
    account: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    owner: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
) -> ProgramResult {
    solana_program::program::invoke(
        &Instruction {
            program_id: *token_program.key,
            accounts: vec![
                AccountMeta::new(*account.key, false),
                AccountMeta::new(*destination.key, false),
                AccountMeta::new_readonly(*owner.key, true),
            ],
            data: vec![9],
        },
        &[
            account.clone(),
            destination.clone(),
            owner.clone(),
            token_program.clone(),
        ],
    )
}

#[cfg(test)]
fn close_token_account_via_signer<'a>(
    account: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    owner: &AccountInfo<'a>,
    _token_program: &AccountInfo<'a>,
) -> ProgramResult {
    require_signer(owner)?;
    transfer_from_program_owned(account, destination, account.lamports())
}

fn require_status(actual: u8, expected: u8) -> ProgramResult {
    if actual != expected {
        return Err(SettlementError::InvalidStatus.into());
    }
    Ok(())
}

fn require_buyback_guard(
    minimum: u64,
    expires_slot: u64,
    route_expires_slot: u64,
    current_slot: u64,
) -> ProgramResult {
    if minimum == 0 {
        return Err(SettlementError::PositiveBuyMinimumRequired.into());
    }
    if expires_slot > route_expires_slot {
        return Err(SettlementError::RouteExpired.into());
    }
    require_not_expired(current_slot, expires_slot)
}

fn require_not_expired(current_slot: u64, expires_slot: u64) -> ProgramResult {
    if expires_slot == 0 || current_slot > expires_slot {
        return Err(SettlementError::RouteExpired.into());
    }
    Ok(())
}

fn require_exact_len(input: &[u8], expected: usize) -> ProgramResult {
    if input.len() != expected {
        return Err(SettlementError::InvalidInstruction.into());
    }
    Ok(())
}

fn read_settlement_id(input: &[u8], offset: usize) -> Result<[u8; 32], ProgramError> {
    if input.len() < offset + 32 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut value = [0; 32];
    value.copy_from_slice(&input[offset..offset + 32]);
    Ok(value)
}

fn read_u64_at(input: &[u8], offset: usize) -> Result<u64, ProgramError> {
    if input.len() < offset + 8 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&input[offset..offset + 8]);
    Ok(u64::from_le_bytes(bytes))
}

fn write_pubkey(data: &mut [u8], offset: &mut usize, value: &Pubkey) {
    data[*offset..*offset + 32].copy_from_slice(value.as_ref());
    *offset += 32;
}

fn read_pubkey(data: &[u8], offset: &mut usize) -> Result<Pubkey, ProgramError> {
    if data.len() < *offset + 32 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    let mut bytes = [0; 32];
    bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    Ok(Pubkey::new_from_array(bytes))
}

fn write_u64(data: &mut [u8], offset: &mut usize, value: u64) {
    data[*offset..*offset + 8].copy_from_slice(&value.to_le_bytes());
    *offset += 8;
}

fn read_u64(data: &[u8], offset: &mut usize) -> Result<u64, ProgramError> {
    let value = read_u64_at(data, *offset)?;
    *offset += 8;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MOCK_SELL_OUTPUT_LAMPORTS, PUMP_PROGRAM_ID};
    use solana_program::{
        instruction::{AccountMeta, Instruction},
        sysvar::{
            instructions::{construct_instructions_data, BorrowedAccountMeta, BorrowedInstruction},
            SysvarSerialize,
        },
    };
    use solana_system_interface::program as system_program;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn account_info(
        key: Pubkey,
        owner: Pubkey,
        lamports: u64,
        data: Vec<u8>,
        is_signer: bool,
        is_writable: bool,
    ) -> AccountInfo<'static> {
        AccountInfo::new(
            Box::leak(Box::new(key)),
            is_signer,
            is_writable,
            Box::leak(Box::new(lamports)),
            Box::leak(data.into_boxed_slice()),
            Box::leak(Box::new(owner)),
            false,
        )
    }

    fn clock_account(slot: u64) -> AccountInfo<'static> {
        let mut account = account_info(
            solana_program::sysvar::clock::id(),
            system_program::id(),
            1,
            vec![0; Clock::size_of()],
            false,
            false,
        );
        Clock {
            slot,
            ..Clock::default()
        }
        .to_account_info(&mut account)
        .unwrap();
        account
    }

    fn v2_data(tag: u8, settlement_id: [u8; 32]) -> Vec<u8> {
        let mut data = vec![tag];
        data.extend_from_slice(&settlement_id);
        match tag {
            INIT_DISTRIBUTION_V2_TAG => {
                data.extend_from_slice(&100_u64.to_le_bytes());
                data.extend_from_slice(&10_u64.to_le_bytes());
                data.push(0);
            }
            SELL_TO_VAULT_V2_TAG => {
                data.extend_from_slice(&10_u64.to_le_bytes());
                data.extend_from_slice(&0_u64.to_le_bytes());
                data.extend_from_slice(&MOCK_SELL_OUTPUT_LAMPORTS.to_le_bytes());
            }
            BUYBACK_V2_TAG => {
                data.extend_from_slice(&0_u64.to_le_bytes());
            }
            SETTLE_VAULT_TO_BUYER_V2_TAG | FINALIZE_DISTRIBUTION_V2_TAG => {}
            _ => unreachable!(),
        }
        data
    }

    fn instruction_sysvar(
        current: u16,
        program_id: Pubkey,
        seller: Pubkey,
        buyer: Pubkey,
        mint: Pubkey,
        state: Pubkey,
        vault: Pubkey,
        settlement_id: [u8; 32],
        mutate: Option<(usize, u8, Option<[u8; 32]>)>,
    ) -> AccountInfo<'static> {
        let identity = [seller, buyer, mint, state, vault];
        let mut instructions = (0..V2_SEQUENCE_LEN)
            .map(|position| Instruction {
                program_id,
                accounts: identity
                    .iter()
                    .enumerate()
                    .map(|(index, pubkey)| AccountMeta {
                        pubkey: *pubkey,
                        is_signer: index < 2,
                        is_writable: index != 2,
                    })
                    .collect(),
                data: v2_data(INIT_DISTRIBUTION_V2_TAG + position as u8, settlement_id),
            })
            .collect::<Vec<_>>();
        if let Some((position, tag, replacement_settlement)) = mutate {
            instructions[position].data[0] = tag;
            if let Some(replacement) = replacement_settlement {
                instructions[position].data[1..33].copy_from_slice(&replacement);
            }
        }
        let borrowed = instructions
            .iter()
            .map(|instruction| BorrowedInstruction {
                program_id: &instruction.program_id,
                accounts: instruction
                    .accounts
                    .iter()
                    .map(|meta| BorrowedAccountMeta {
                        pubkey: &meta.pubkey,
                        is_signer: meta.is_signer,
                        is_writable: meta.is_writable,
                    })
                    .collect(),
                data: &instruction.data,
            })
            .collect::<Vec<_>>();
        let mut data = construct_instructions_data(&borrowed);
        let end = data.len();
        data[end - 2..].copy_from_slice(&current.to_le_bytes());
        account_info(
            instructions_sysvar_id(),
            solana_program::sysvar::id(),
            1,
            data,
            false,
            false,
        )
    }

    fn seeded_route(
        program_id: &Pubkey,
        seller: &AccountInfo<'static>,
        buyer: &AccountInfo<'static>,
        mint: &AccountInfo<'static>,
        settlement_id: [u8; 32],
        status: u8,
        vault_lamports: u64,
        buyer_baseline: u64,
        residual: u64,
    ) -> (AccountInfo<'static>, AccountInfo<'static>) {
        let (state_key, bump) =
            route_state_pda_v2(program_id, seller.key, buyer.key, mint.key, &settlement_id);
        let (vault_key, vault_bump) = route_vault_pda_v2(program_id, &state_key);
        let mut state_data = vec![0; DistributionV2State::LEN];
        DistributionV2State {
            status,
            bump,
            vault_bump,
            close_run_created_wsol: false,
            seller: *seller.key,
            buyer: *buyer.key,
            mint: *mint.key,
            settlement_id,
            expires_slot: 100,
            sell_amount_raw: 10,
            quoted_buy_amount_raw: if status >= STATUS_BOUGHT { 22 } else { 0 },
            seller_pre_sell_lamports: seller.lamports(),
            buyer_baseline_lamports: buyer_baseline,
            vault_funded_lamports: if status >= STATUS_SOLD {
                MOCK_SELL_OUTPUT_LAMPORTS
            } else {
                0
            },
            buyer_funded_lamports: if status >= STATUS_FUNDED {
                MOCK_SELL_OUTPUT_LAMPORTS
            } else {
                0
            },
            actual_spent_lamports: if status >= STATUS_BOUGHT {
                MOCK_SELL_OUTPUT_LAMPORTS.saturating_sub(residual)
            } else {
                0
            },
            residual_lamports: residual,
        }
        .pack(&mut state_data)
        .unwrap();
        (
            account_info(state_key, *program_id, 1_000_000, state_data, false, true),
            account_info(vault_key, *program_id, vault_lamports, vec![], false, true),
        )
    }

    fn pump_accounts(
        user: &AccountInfo<'static>,
        mint: &AccountInfo<'static>,
        is_buy: bool,
    ) -> Vec<AccountInfo<'static>> {
        // Pump.fun V2: buy = 27 account (có global_volume_accumulator @19, program @26);
        // sell = 26 account (Pump đã bỏ global_volume_accumulator, program @25).
        let count = if is_buy { 27 } else { 26 };
        let system_index = if is_buy { 24 } else { 23 };
        let mut accounts = (0..count)
            .map(|index| {
                account_info(
                    key(40 + index as u8),
                    system_program::id(),
                    1,
                    vec![],
                    false,
                    true,
                )
            })
            .collect::<Vec<_>>();
        accounts[0] = account_info(key(39), system_program::id(), 1, vec![0; 162], false, false);
        accounts[1] = mint.clone();
        let mut bonding_curve = vec![0; 49];
        bonding_curve[8..16].copy_from_slice(&1_073_000_000_000_000_u64.to_le_bytes());
        bonding_curve[16..24].copy_from_slice(&30_000_000_000_000_u64.to_le_bytes());
        bonding_curve[24..32].copy_from_slice(&793_100_000_000_000_u64.to_le_bytes());
        bonding_curve[40..48].copy_from_slice(&1_000_000_000_000_000_u64.to_le_bytes());
        accounts[10] = account_info(key(70), crate::id(), 10_000_000, bonding_curve, false, true);
        accounts[13] = user.clone();
        accounts[system_index] = account_info(
            system_program::id(),
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        accounts[count - 1] = account_info(
            PUMP_PROGRAM_ID,
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        accounts
    }

    #[test]
    fn guarded_buyback_checks_floor_expiry_and_dispatch_before_spending() {
        let program_id = crate::id();
        let seller = account_info(
            key(1),
            system_program::id(),
            100_000_000,
            vec![],
            true,
            true,
        );
        let baseline = 1_000_000;
        let buyer = account_info(
            key(2),
            system_program::id(),
            baseline + MOCK_SELL_OUTPUT_LAMPORTS,
            vec![],
            true,
            true,
        );
        let mint = account_info(key(3), TOKEN_PROGRAM_ID, 1, vec![], false, false);
        let settlement = [4; 32];
        let (state, vault) = seeded_route(
            &program_id,
            &seller,
            &buyer,
            &mint,
            settlement,
            STATUS_FUNDED,
            0,
            baseline,
            0,
        );
        let sysvar = instruction_sysvar(
            3,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            *state.key,
            *vault.key,
            settlement,
            Some((3, BUYBACK_V3_TAG, None)),
        );
        let mut accounts = vec![
            seller,
            buyer.clone(),
            mint.clone(),
            state.clone(),
            vault,
            clock_account(10),
            sysvar,
        ];
        accounts.extend(pump_accounts(&buyer, &mint, true));
        let data = |minimum: u64, expires: u64| {
            let mut bytes = vec![BUYBACK_V3_TAG];
            bytes.extend_from_slice(&settlement);
            bytes.extend_from_slice(&minimum.to_le_bytes());
            bytes.extend_from_slice(&expires.to_le_bytes());
            bytes
        };
        let initial = buyer.lamports();
        for (minimum, expires, expected) in [
            (0, 100, SettlementError::PositiveBuyMinimumRequired),
            (1, 9, SettlementError::RouteExpired),
            (1, 101, SettlementError::RouteExpired),
            (u64::MAX, 100, SettlementError::SlippageExceeded),
        ] {
            assert_eq!(
                crate::process_instruction(&program_id, &accounts, &data(minimum, expires)),
                Err(expected.into())
            );
            assert_eq!(buyer.lamports(), initial);
            assert_eq!(
                DistributionV2State::unpack(&state.try_borrow_data().unwrap())
                    .unwrap()
                    .status,
                STATUS_FUNDED
            );
        }
        for length in [0, 1, 40, 41, 48, 50] {
            let mut invalid = data(1, 100);
            invalid.resize(length, 0);
            assert!(crate::process_instruction(&program_id, &accounts, &invalid).is_err());
        }
        crate::process_instruction(&program_id, &accounts, &data(1, 10)).unwrap();
        assert!(buyer.lamports() < initial);
        assert!(
            DistributionV2State::unpack(&state.try_borrow_data().unwrap())
                .unwrap()
                .quoted_buy_amount_raw
                >= 1
        );
    }

    #[test]
    fn guarded_sequence_accepts_only_buyback_position() {
        let program = crate::id();
        let identities = [key(1), key(2), key(3), key(4), key(5)];
        for position in 0..5 {
            let sysvar = instruction_sysvar(
                position as u16,
                program,
                identities[0],
                identities[1],
                identities[2],
                identities[3],
                identities[4],
                [6; 32],
                Some((3, BUYBACK_V3_TAG, None)),
            );
            let current_tag = if position == 3 {
                BUYBACK_V3_TAG
            } else {
                INIT_DISTRIBUTION_V2_TAG + position as u8
            };
            assert!(require_atomic_sequence(
                &program,
                &sysvar,
                current_tag,
                &[6; 32],
                &identities[0],
                &identities[1],
                &identities[2],
                &identities[3],
                &identities[4],
                false
            )
            .is_ok());
        }
        for (position, tag) in [
            (0, BUYBACK_V3_TAG),
            (1, BUYBACK_V3_TAG),
            (2, BUYBACK_V3_TAG),
            (4, BUYBACK_V3_TAG),
            (3, REFUND_DISTRIBUTION_V2_TAG),
            (3, 19),
        ] {
            let sysvar = instruction_sysvar(
                0,
                program,
                identities[0],
                identities[1],
                identities[2],
                identities[3],
                identities[4],
                [6; 32],
                Some((position, tag, None)),
            );
            assert!(require_atomic_sequence(
                &program,
                &sysvar,
                INIT_DISTRIBUTION_V2_TAG,
                &[6; 32],
                &identities[0],
                &identities[1],
                &identities[2],
                &identities[3],
                &identities[4],
                false
            )
            .is_err());
        }
    }

    #[test]
    fn state_round_trip_preserves_atomic_accounting() {
        let state = DistributionV2State {
            status: STATUS_BOUGHT,
            bump: 254,
            vault_bump: 253,
            close_run_created_wsol: true,
            seller: key(1),
            buyer: key(2),
            mint: key(3),
            settlement_id: [4; 32],
            expires_slot: 10,
            sell_amount_raw: 20,
            quoted_buy_amount_raw: 30,
            seller_pre_sell_lamports: 40,
            buyer_baseline_lamports: 50,
            vault_funded_lamports: 60,
            buyer_funded_lamports: 60,
            actual_spent_lamports: 55,
            residual_lamports: 5,
        };
        let mut data = vec![0; DistributionV2State::LEN];
        state.pack(&mut data).unwrap();
        assert_eq!(DistributionV2State::unpack(&data).unwrap(), state);
        assert_eq!(DistributionV2State::LEN, 216);
    }

    #[test]
    fn route_is_unique_per_settlement_and_vault_is_bound_to_state() {
        let program = key(9);
        let seller = key(1);
        let buyer = key(2);
        let mint = key(3);
        let (state_a, _) = route_state_pda_v2(&program, &seller, &buyer, &mint, &[4; 32]);
        let (state_b, _) = route_state_pda_v2(&program, &seller, &buyer, &mint, &[5; 32]);
        let (vault_a, _) = route_vault_pda_v2(&program, &state_a);
        let (vault_b, _) = route_vault_pda_v2(&program, &state_b);
        assert_ne!(state_a, state_b);
        assert_ne!(vault_a, vault_b);
    }

    #[test]
    fn v2_decoder_rejects_trailing_data_and_minimum_contracts_are_bound() {
        let mut init = vec![INIT_DISTRIBUTION_V2_TAG];
        init.extend_from_slice(&[1; 32]);
        init.extend_from_slice(&9_u64.to_le_bytes());
        init.extend_from_slice(&10_u64.to_le_bytes());
        init.push(1);
        assert_eq!(unpack_init(&init).unwrap().sell_amount_raw, 10);
        init.push(0);
        assert_eq!(
            unpack_init(&init).unwrap_err(),
            ProgramError::Custom(SettlementError::InvalidInstruction as u32)
        );

        let mut buy = vec![BUYBACK_V2_TAG];
        buy.extend_from_slice(&[2; 32]);
        buy.extend_from_slice(&0_u64.to_le_bytes());
        assert_eq!(unpack_buyback(&buy).unwrap().min_buy_amount_raw, 0);

        let sell = SellV2Args {
            settlement_id: [3; 32],
            sell_amount_raw: 10,
            min_sol_output_raw: 0,
            max_deposit_lamports: 20,
        };
        assert_eq!(
            require_sell_limits(10, &sell).unwrap_err(),
            ProgramError::Custom(SettlementError::PositiveSellMinimumRequired as u32)
        );
        assert_eq!(
            require_sell_limits(
                11,
                &SellV2Args {
                    min_sol_output_raw: 1,
                    ..sell
                }
            )
            .unwrap_err(),
            ProgramError::Custom(SettlementError::InvalidRoute as u32)
        );
        require_sell_limits(
            10,
            &SellV2Args {
                min_sol_output_raw: 1,
                ..sell
            },
        )
        .unwrap();
        assert_eq!(
            require_sell_limits(
                10,
                &SellV2Args {
                    min_sol_output_raw: 21,
                    ..sell
                }
            )
            .unwrap_err(),
            ProgramError::Custom(SettlementError::InvalidRoute as u32)
        );
    }

    #[test]
    fn sell_postcondition_enforces_minimum_cap_zero_and_underflow() {
        let before = 10_000;
        let minimum = 100;
        let maximum = 200;

        assert_eq!(
            validated_sell_proceeds(before, before + minimum, minimum, maximum).unwrap(),
            minimum
        );
        assert_eq!(
            validated_sell_proceeds(before, before + minimum - 1, minimum, maximum).unwrap_err(),
            ProgramError::Custom(SettlementError::SlippageExceeded as u32)
        );
        assert_eq!(
            validated_sell_proceeds(before, before + maximum + 1, minimum, maximum).unwrap_err(),
            ProgramError::Custom(SettlementError::DepositCapExceeded as u32)
        );
        assert_eq!(
            validated_sell_proceeds(before, before, minimum, maximum).unwrap_err(),
            ProgramError::Custom(SettlementError::SlippageExceeded as u32)
        );
        assert_eq!(
            validated_sell_proceeds(before, before - 1, minimum, maximum).unwrap_err(),
            ProgramError::Custom(SettlementError::SlippageExceeded as u32)
        );
    }

    #[test]
    fn sell_rejects_mock_cpi_output_one_lamport_below_the_positive_minimum() {
        let program_id = crate::id();
        let settlement = [6; 32];
        let seller = account_info(key(1), system_program::id(), 5_000_000, vec![], true, true);
        let buyer = account_info(key(2), system_program::id(), 2_000_000, vec![], true, true);
        let mint = account_info(key(3), system_program::id(), 1, vec![], false, false);
        let (state, vault) = seeded_route(
            &program_id,
            &seller,
            &buyer,
            &mint,
            settlement,
            STATUS_INITIALIZED,
            0,
            buyer.lamports(),
            0,
        );
        let sell_sysvar = instruction_sysvar(
            1,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            *state.key,
            *vault.key,
            settlement,
            None,
        );
        let pump = pump_accounts(&seller, &mint, false);
        let mut accounts = vec![
            seller,
            buyer,
            mint.clone(),
            state.clone(),
            vault.clone(),
            clock_account(10),
            sell_sysvar,
        ];
        accounts.extend(pump);

        assert_eq!(
            process_sell(
                &program_id,
                &accounts,
                SellV2Args {
                    settlement_id: settlement,
                    sell_amount_raw: 10,
                    min_sol_output_raw: MOCK_SELL_OUTPUT_LAMPORTS + 1,
                    max_deposit_lamports: MOCK_SELL_OUTPUT_LAMPORTS + 1,
                },
            )
            .unwrap_err(),
            ProgramError::Custom(SettlementError::SlippageExceeded as u32)
        );
        assert_eq!(vault.lamports(), 0);
        assert_eq!(
            DistributionV2State::unpack(&state.try_borrow_data().unwrap())
                .unwrap()
                .status,
            STATUS_INITIALIZED
        );
    }

    #[test]
    fn associated_wsol_address_is_owner_and_token_program_bound() {
        let owner = key(1);
        let expected = associated_token_address(&owner, &TOKEN_PROGRAM_ID, &WSOL_MINT);
        assert_ne!(
            expected,
            associated_token_address(&key(2), &TOKEN_PROGRAM_ID, &WSOL_MINT)
        );
        assert_ne!(
            expected,
            associated_token_address(&owner, &key(3), &WSOL_MINT)
        );
    }

    #[test]
    fn instructions_sysvar_accepts_only_one_adjacent_route_identical_sequence() {
        let program_id = crate::id();
        let seller = key(1);
        let buyer = key(2);
        let mint = key(3);
        let settlement = [4; 32];
        let (state, _) = route_state_pda_v2(&program_id, &seller, &buyer, &mint, &settlement);
        let (vault, _) = route_vault_pda_v2(&program_id, &state);
        for position in 0..V2_SEQUENCE_LEN {
            let sysvar = instruction_sysvar(
                position as u16,
                program_id,
                seller,
                buyer,
                mint,
                state,
                vault,
                settlement,
                None,
            );
            require_atomic_sequence(
                &program_id,
                &sysvar,
                INIT_DISTRIBUTION_V2_TAG + position as u8,
                &settlement,
                &seller,
                &buyer,
                &mint,
                &state,
                &vault,
                false,
            )
            .unwrap();
        }

        let out_of_order = instruction_sysvar(
            1,
            program_id,
            seller,
            buyer,
            mint,
            state,
            vault,
            settlement,
            Some((1, BUYBACK_V2_TAG, None)),
        );
        assert_eq!(
            require_atomic_sequence(
                &program_id,
                &out_of_order,
                SELL_TO_VAULT_V2_TAG,
                &settlement,
                &seller,
                &buyer,
                &mint,
                &state,
                &vault,
                false,
            )
            .unwrap_err(),
            ProgramError::Custom(SettlementError::InvalidSequence as u32)
        );

        let cross_route = instruction_sysvar(
            2,
            program_id,
            seller,
            buyer,
            mint,
            state,
            vault,
            settlement,
            Some((2, SETTLE_VAULT_TO_BUYER_V2_TAG, Some([9; 32]))),
        );
        assert_eq!(
            require_atomic_sequence(
                &program_id,
                &cross_route,
                SETTLE_VAULT_TO_BUYER_V2_TAG,
                &settlement,
                &seller,
                &buyer,
                &mint,
                &state,
                &vault,
                false,
            )
            .unwrap_err(),
            ProgramError::Custom(SettlementError::InvalidSequence as u32)
        );
    }

    #[test]
    fn dynamic_buy_uses_actual_drifted_proceeds_then_restores_buyer_baseline() {
        let program_id = crate::id();
        let settlement = [7; 32];
        let stale_planned_estimate = MOCK_SELL_OUTPUT_LAMPORTS - 123_000;
        let seller = account_info(key(1), system_program::id(), 5_000_000, vec![], true, true);
        let buyer = account_info(key(2), system_program::id(), 2_000_000, vec![], true, true);
        let mint = account_info(key(3), system_program::id(), 1, vec![], false, false);
        let buyer_baseline = buyer.lamports();
        let (state, vault) = seeded_route(
            &program_id,
            &seller,
            &buyer,
            &mint,
            settlement,
            STATUS_INITIALIZED,
            0,
            buyer_baseline,
            0,
        );
        let state_key = *state.key;
        let vault_key = *vault.key;
        let clock = clock_account(10);

        let sell_sysvar = instruction_sysvar(
            1,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            state_key,
            vault_key,
            settlement,
            None,
        );
        let mut sell_accounts = vec![
            seller.clone(),
            buyer.clone(),
            mint.clone(),
            state.clone(),
            vault.clone(),
            clock.clone(),
            sell_sysvar,
        ];
        sell_accounts.extend(pump_accounts(&seller, &mint, false));
        process_sell(
            &program_id,
            &sell_accounts,
            SellV2Args {
                settlement_id: settlement,
                sell_amount_raw: 10,
                min_sol_output_raw: MOCK_SELL_OUTPUT_LAMPORTS,
                max_deposit_lamports: MOCK_SELL_OUTPUT_LAMPORTS,
            },
        )
        .unwrap();
        assert_eq!(vault.lamports(), MOCK_SELL_OUTPUT_LAMPORTS);
        assert_ne!(vault.lamports(), stale_planned_estimate);
        assert_eq!(
            DistributionV2State::unpack(&state.try_borrow_data().unwrap())
                .unwrap()
                .status,
            STATUS_SOLD
        );

        let settle_sysvar = instruction_sysvar(
            2,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            state_key,
            vault_key,
            settlement,
            None,
        );
        process_settle(
            &program_id,
            &[
                seller.clone(),
                buyer.clone(),
                mint.clone(),
                state.clone(),
                vault.clone(),
                clock.clone(),
                settle_sysvar,
            ],
            settlement,
        )
        .unwrap();
        assert_eq!(buyer.lamports(), buyer_baseline + MOCK_SELL_OUTPUT_LAMPORTS);
        assert_eq!(vault.lamports(), 0);

        let buy_sysvar = instruction_sysvar(
            3,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            state_key,
            vault_key,
            settlement,
            None,
        );
        let mut buy_accounts = vec![
            seller.clone(),
            buyer.clone(),
            mint.clone(),
            state.clone(),
            vault.clone(),
            clock,
            buy_sysvar,
        ];
        buy_accounts.extend(pump_accounts(&buyer, &mint, true));
        process_buyback(
            &program_id,
            &buy_accounts,
            BuybackV2Args {
                settlement_id: settlement,
                min_buy_amount_raw: 0,
            },
        )
        .unwrap();
        let buyback_reserve = MOCK_SELL_OUTPUT_LAMPORTS
            - crate::conservative_buy_quote_budget(MOCK_SELL_OUTPUT_LAMPORTS).unwrap();
        assert_eq!(buyer.lamports(), buyer_baseline + buyback_reserve);
        let bought = DistributionV2State::unpack(&state.try_borrow_data().unwrap()).unwrap();
        assert!(bought.quoted_buy_amount_raw > 0);
        assert_eq!(
            bought.actual_spent_lamports,
            MOCK_SELL_OUTPUT_LAMPORTS - buyback_reserve
        );
        assert_eq!(bought.residual_lamports, buyback_reserve);

        let finalize_sysvar = instruction_sysvar(
            4,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            state_key,
            vault_key,
            settlement,
            None,
        );
        let buyer_wsol = account_info(
            associated_token_address(buyer.key, &TOKEN_PROGRAM_ID, &WSOL_MINT),
            TOKEN_PROGRAM_ID,
            0,
            vec![],
            false,
            true,
        );
        let token_program = account_info(
            TOKEN_PROGRAM_ID,
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        let system = account_info(
            system_program::id(),
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        process_finalize(
            &program_id,
            &[
                seller.clone(),
                buyer.clone(),
                mint,
                state.clone(),
                vault.clone(),
                buyer_wsol,
                token_program,
                system,
                finalize_sysvar,
            ],
            settlement,
        )
        .unwrap();
        assert_eq!(buyer.lamports(), buyer_baseline);
        assert_eq!(state.lamports(), 0);
        assert_eq!(vault.lamports(), 0);
    }

    #[test]
    fn finalize_and_refund_are_seller_bound_and_recover_residual() {
        let program_id = crate::id();
        let settlement = [8; 32];
        let seller = account_info(key(1), system_program::id(), 5_000_000, vec![], true, true);
        let buyer = account_info(key(2), system_program::id(), 2_000_500, vec![], true, true);
        let mint = account_info(key(3), system_program::id(), 1, vec![], false, false);
        let (state, vault) = seeded_route(
            &program_id,
            &seller,
            &buyer,
            &mint,
            settlement,
            STATUS_BOUGHT,
            0,
            2_000_000,
            500,
        );
        let finalize_sysvar = instruction_sysvar(
            4,
            program_id,
            *seller.key,
            *buyer.key,
            *mint.key,
            *state.key,
            *vault.key,
            settlement,
            None,
        );
        let buyer_wsol = account_info(
            associated_token_address(buyer.key, &TOKEN_PROGRAM_ID, &WSOL_MINT),
            TOKEN_PROGRAM_ID,
            0,
            vec![],
            false,
            true,
        );
        let token_program = account_info(
            TOKEN_PROGRAM_ID,
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        let system = account_info(
            system_program::id(),
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        let seller_before = seller.lamports();
        process_finalize(
            &program_id,
            &[
                seller.clone(),
                buyer.clone(),
                mint.clone(),
                state.clone(),
                vault.clone(),
                buyer_wsol.clone(),
                token_program.clone(),
                system.clone(),
                finalize_sysvar,
            ],
            settlement,
        )
        .unwrap();
        assert_eq!(buyer.lamports(), 2_000_000);
        assert_eq!(seller.lamports(), seller_before + 500 + 1_000_000);

        let refund_settlement = [9; 32];
        let refund_buyer =
            account_info(key(5), system_program::id(), 3_000_300, vec![], true, true);
        let (refund_state, refund_vault) = seeded_route(
            &program_id,
            &seller,
            &refund_buyer,
            &mint,
            refund_settlement,
            STATUS_FUNDED,
            700,
            3_000_000,
            0,
        );
        let refund_wsol = account_info(
            associated_token_address(refund_buyer.key, &TOKEN_PROGRAM_ID, &WSOL_MINT),
            TOKEN_PROGRAM_ID,
            0,
            vec![],
            false,
            true,
        );
        let seller_before_refund = seller.lamports();
        process_refund(
            &program_id,
            &[
                seller.clone(),
                refund_buyer.clone(),
                mint,
                refund_state.clone(),
                refund_vault.clone(),
                refund_wsol,
                token_program,
                system,
            ],
            refund_settlement,
        )
        .unwrap();
        assert_eq!(refund_buyer.lamports(), 3_000_000);
        assert_eq!(refund_state.lamports(), 0);
        assert_eq!(refund_vault.lamports(), 0);
        assert_eq!(
            seller.lamports(),
            seller_before_refund + 300 + 700 + 1_000_000
        );
    }
}
