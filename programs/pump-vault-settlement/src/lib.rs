//! # Smart Distribution — Pump.fun Bonding-Curve Settlement Program
//!
//! A native (no-Anchor) Solana SBF program that settles Pump.fun token trades
//! through a PDA vault: a **seller** sells token into the vault (via Pump
//! `SellV2`), the program forwards the dynamic SOL output to a **buyer**, and
//! the buyer buys back token (via Pump `BuyV2`) — all in one or two
//! transactions.
//!
//! ## Instructions
//!
//! | Tag | Instruction | Signers | What it does |
//! |-----|-------------|---------|--------------|
//! | 0 | `init_route_vault` | authority + payer | Creates the route state + vault PDAs (idempotent) |
//! | 1 | `sell_to_vault` | authority + seller | Sells token via Pump SellV2, forwards SOL to vault |
//! | 2 | `settle_and_buy` | authority + buyer | (legacy) Forward + buyback in one ix — runtime-broken, prefer split |
//! | 3 | `refund_vault` | authority | Returns vault SOL to seller (recipient bound to seller) |
//! | 4 | `close_route_vault` | authority | Reclaims vault + state rent to seller, marks route closed |
//! | 5 | `settle_vault_to_buyer` | authority + buyer | Forwards vault SOL to buyer (split step 1) |
//! | 6 | `buy_with_buyer_balance` | authority + buyer | Buyer buys back with settled SOL (split step 2) |
//! | 7-12 | Smart Distribution V2 | seller + buyer | Atomic one-transaction sell, settle, buyback, finalize/refund |
//!
//! ## Security
//!
//! - Every fund-moving instruction requires the route `authority` to sign.
//! - `refund_vault` and `close_route_vault` bind their recipient to
//!   `state.seller` on-chain — a leaked authority key cannot redirect vault
//!   SOL or rent to an arbitrary wallet.
//! - PDA derivation uses `[route/vault, authority, mint, seller, buyer]`;
//!   routes are isolated per (authority, mint, seller, buyer) tuple.

#[cfg(not(test))]
use solana_program::program::invoke;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    declare_id,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::{Sysvar, SysvarSerialize},
};
use solana_system_interface::instruction as system_instruction;

mod v2;

declare_id!("45TYikBDuxngJzkxqiMpuzudnA5VaW47renkjE5XFCge");

/// Embedded security.txt (Neodyme standard).
///
/// Security researchers inspecting this program on-chain can read the
/// `security.txt` payload from the program binary to find responsible-disclosure
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Smart Distribution",
    project_url: "https://github.com/devibeee/smart-distribution-program",
    contacts: "link:https://github.com/devibeee/smart-distribution-program/security/advisories/new",
    policy: "https://github.com/devibeee/smart-distribution-program/security/policy",
    source_code: "https://github.com/devibeee/smart-distribution-program",
    auditors: "",
    source_release_signature: "",
    expiry: "2027-12-31"
}

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

// ──────────────────────────────────────────────────────────────────────────
// Section 1 — Constants, program IDs, Pump AMM discriminators
// ──────────────────────────────────────────────────────────────────────────

const STATE_MAGIC: &[u8; 8] = b"PVLTSTL1";
const STATE_VERSION: u8 = 2;
const ROUTE_SEED: &[u8] = b"route";
const VAULT_SEED: &[u8] = b"vault";

// Tag 13/19 mode: stealth PDA derivation. Each round derives a unique PDA from
// [STEALTH_SEED, authority, mint, round_nonce]. The PDA signs the Pump SellV2
// CPI via invoke_signed. Distinct seed prevents PDA collision with route/vault.
const STEALTH_SEED: &[u8] = b"stealth";

const PUMP_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const PUMP_BUY_V2_DISCRIMINATOR: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
const PUMP_SELL_V2_DISCRIMINATOR: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
const BASIS_POINTS_DENOMINATOR: u128 = 10_000;

#[cfg(test)]
const MOCK_SELL_OUTPUT_LAMPORTS: u64 = 777_000;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// ──────────────────────────────────────────────────────────────────────────
// Section 2 — Error codes & RouteState account layout
// ──────────────────────────────────────────────────────────────────────────

pub enum SettlementError {
    InvalidInstruction = 1,
    InvalidPda = 2,
    InvalidSigner = 3,
    InvalidRoute = 4,
    RouteClosed = 5,
    RouteExpired = 6,
    SameSellerBuyer = 7,
    MathOverflow = 8,
    SlippageExceeded = 9,
    DepositCapExceeded = 10,
    VaultEmpty = 11,
    AccountMismatch = 12,
    AccountTooSmall = 13,
    CustodyNotEmpty = 14,
    InvalidSequence = 15,
    InvalidStatus = 16,
    BaselineMismatch = 17,
    CleanupMismatch = 18,
    ZeroMinimumRequired = 19,
}

impl From<SettlementError> for ProgramError {
    fn from(value: SettlementError) -> Self {
        ProgramError::Custom(value as u32)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteState {
    pub bump: u8,
    pub vault_bump: u8,
    pub closed: bool,
    pub authority: Pubkey,
    pub mint: Pubkey,
    pub seller: Pubkey,
    pub buyer: Pubkey,
    pub settlement_id: [u8; 32],
    pub expires_slot: u64,
    pub created_slot: u64,
    pub last_deposit_lamports: u64,
    pub pending_buy_lamports: u64,
    pub total_deposited_lamports: u64,
    pub total_spent_lamports: u64,
}

impl RouteState {
    pub const LEN: usize = 8 + 1 + 1 + 1 + 1 + (32 * 5) + (8 * 6);

    pub fn pack(&self, data: &mut [u8]) -> ProgramResult {
        if data.len() < Self::LEN {
            return Err(SettlementError::AccountTooSmall.into());
        }
        data[..Self::LEN].fill(0);
        let mut offset = 0;
        data[offset..offset + 8].copy_from_slice(STATE_MAGIC);
        offset += 8;
        data[offset] = STATE_VERSION;
        offset += 1;
        data[offset] = self.bump;
        offset += 1;
        data[offset] = self.vault_bump;
        offset += 1;
        data[offset] = u8::from(self.closed);
        offset += 1;
        write_pubkey(data, &mut offset, &self.authority);
        write_pubkey(data, &mut offset, &self.mint);
        write_pubkey(data, &mut offset, &self.seller);
        write_pubkey(data, &mut offset, &self.buyer);
        data[offset..offset + 32].copy_from_slice(&self.settlement_id);
        offset += 32;
        write_u64(data, &mut offset, self.expires_slot);
        write_u64(data, &mut offset, self.created_slot);
        write_u64(data, &mut offset, self.last_deposit_lamports);
        write_u64(data, &mut offset, self.pending_buy_lamports);
        write_u64(data, &mut offset, self.total_deposited_lamports);
        write_u64(data, &mut offset, self.total_spent_lamports);
        Ok(())
    }

    pub fn unpack(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() < Self::LEN {
            return Err(SettlementError::AccountTooSmall.into());
        }
        let mut offset = 0;
        if &data[offset..offset + 8] != STATE_MAGIC {
            return Err(SettlementError::InvalidRoute.into());
        }
        offset += 8;
        if data[offset] != STATE_VERSION {
            return Err(SettlementError::InvalidRoute.into());
        }
        offset += 1;
        let bump = data[offset];
        offset += 1;
        let vault_bump = data[offset];
        offset += 1;
        let closed = data[offset] == 1;
        offset += 1;
        let authority = read_pubkey(data, &mut offset)?;
        let mint = read_pubkey(data, &mut offset)?;
        let seller = read_pubkey(data, &mut offset)?;
        let buyer = read_pubkey(data, &mut offset)?;
        let mut settlement_id = [0_u8; 32];
        settlement_id.copy_from_slice(&data[offset..offset + 32]);
        offset += 32;
        let expires_slot = read_u64(data, &mut offset)?;
        let created_slot = read_u64(data, &mut offset)?;
        let last_deposit_lamports = read_u64(data, &mut offset)?;
        let pending_buy_lamports = read_u64(data, &mut offset)?;
        let total_deposited_lamports = read_u64(data, &mut offset)?;
        let total_spent_lamports = read_u64(data, &mut offset)?;
        Ok(Self {
            bump,
            vault_bump,
            closed,
            authority,
            mint,
            seller,
            buyer,
            settlement_id,
            expires_slot,
            created_slot,
            last_deposit_lamports,
            pending_buy_lamports,
            total_deposited_lamports,
            total_spent_lamports,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitRouteVaultArgs {
    pub settlement_id: [u8; 32],
    pub expires_slot: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SellToVaultArgs {
    pub settlement_id: [u8; 32],
    pub expires_slot: u64,
    pub sell_amount_raw: u64,
    pub min_sol_output_raw: u64,
    pub max_deposit_lamports: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettleAndBuyArgs {
    pub settlement_id: [u8; 32],
    pub expires_slot: u64,
    pub max_buy_lamports: u64,
    pub min_buy_amount_raw: u64,
    pub jito_tip_lamports: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettleVaultArgs {
    pub settlement_id: [u8; 32],
    pub expires_slot: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteOnlyArgs {
    pub settlement_id: [u8; 32],
}

// Tag 13: sell token through an ephemeral PDA that signs the Pump SellV2 CPI
// via invoke_signed. After the ix the PDA's ATAs are closed and residual SOL
// is forwarded. The round_nonce (8-byte LE) makes every PDA unique per round.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StealthPdaArgs {
    pub round_nonce: u64,
    pub sell_amount_raw: u64,
    pub min_sol_output_raw: u64,
    pub recipient: Pubkey,
}

// Tag 15: reclaim all lamports from a system-owned signer account to a
// recipient via System.assign reassignment. Used to clean up the tx payer
// account so it leaves no residual balance after the transaction.
//
// Flow: caller invokes with [signer, recipient, system_program]. Program
// calls System.assign(signer, program_id) via the signer's signature.
// Then drains all lamports to recipient. Account ends at 0 lamports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloseBurnerArgs {
    pub recipient: Pubkey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// ──────────────────────────────────────────────────────────────────────────
// Section 3 — Instruction enum + unpack (instruction discriminator parser)
// ──────────────────────────────────────────────────────────────────────────

pub enum VaultInstruction {
    // Tag 0-6: single-seller route mode
    InitRouteVault(InitRouteVaultArgs),
    SellToVault(SellToVaultArgs),
    SettleAndBuy(SettleAndBuyArgs),
    RefundVault(RouteOnlyArgs),
    CloseRouteVault(RouteOnlyArgs),
    SettleVaultToBuyer(SettleVaultArgs),
    BuyWithBuyerBalance(SettleAndBuyArgs),
    // Tag 13: ephemeral PDA sells via Pump SellV2 (CPI signed by the PDA),
    // then closes both ATAs back to recipient.
    StealthSellPda(StealthPdaArgs),
    // Tag 15: reclaim all lamports from a system-owned signer account.
    CloseBurner(CloseBurnerArgs),
    // Tag 19: sell via PDA but KEEP PDA + base ATA alive for reuse.
    StealthSellPdaKeep(StealthPdaArgs),
}

impl VaultInstruction {
    pub fn unpack(input: &[u8]) -> Result<Self, ProgramError> {
        let (tag, rest) = input
            .split_first()
            .ok_or(SettlementError::InvalidInstruction)?;
        match *tag {
            0 => {
                let (settlement_id, offset) = read_settlement_id(rest, 0)?;
                Ok(Self::InitRouteVault(InitRouteVaultArgs {
                    settlement_id,
                    expires_slot: read_u64_at(rest, offset)?,
                }))
            }
            1 => {
                let (settlement_id, mut offset) = read_settlement_id(rest, 0)?;
                let expires_slot = read_u64_at(rest, offset)?;
                offset += 8;
                let sell_amount_raw = read_u64_at(rest, offset)?;
                offset += 8;
                let min_sol_output_raw = read_u64_at(rest, offset)?;
                offset += 8;
                let max_deposit_lamports = read_u64_at(rest, offset)?;
                Ok(Self::SellToVault(SellToVaultArgs {
                    settlement_id,
                    expires_slot,
                    sell_amount_raw,
                    min_sol_output_raw,
                    max_deposit_lamports,
                }))
            }
            2 => {
                let (settlement_id, mut offset) = read_settlement_id(rest, 0)?;
                let expires_slot = read_u64_at(rest, offset)?;
                offset += 8;
                let max_buy_lamports = read_u64_at(rest, offset)?;
                offset += 8;
                let min_buy_amount_raw = read_u64_at(rest, offset)?;
                offset += 8;
                let jito_tip_lamports = read_optional_u64_at(rest, offset)?;
                Ok(Self::SettleAndBuy(SettleAndBuyArgs {
                    settlement_id,
                    expires_slot,
                    max_buy_lamports,
                    min_buy_amount_raw,
                    jito_tip_lamports,
                }))
            }
            3 => {
                let (settlement_id, _) = read_settlement_id(rest, 0)?;
                Ok(Self::RefundVault(RouteOnlyArgs { settlement_id }))
            }
            4 => {
                let (settlement_id, _) = read_settlement_id(rest, 0)?;
                Ok(Self::CloseRouteVault(RouteOnlyArgs { settlement_id }))
            }
            5 => {
                let (settlement_id, offset) = read_settlement_id(rest, 0)?;
                let expires_slot = read_u64_at(rest, offset)?;
                Ok(Self::SettleVaultToBuyer(SettleVaultArgs {
                    settlement_id,
                    expires_slot,
                }))
            }
            6 => {
                let (settlement_id, mut offset) = read_settlement_id(rest, 0)?;
                let expires_slot = read_u64_at(rest, offset)?;
                offset += 8;
                let max_buy_lamports = read_u64_at(rest, offset)?;
                offset += 8;
                let min_buy_amount_raw = read_u64_at(rest, offset)?;
                offset += 8;
                let jito_tip_lamports = read_optional_u64_at(rest, offset)?;
                Ok(Self::BuyWithBuyerBalance(SettleAndBuyArgs {
                    settlement_id,
                    expires_slot,
                    max_buy_lamports,
                    min_buy_amount_raw,
                    jito_tip_lamports,
                }))
            }
            // Tag 13: ephemeral PDA sell.
            // Layout: [round_nonce:u64, sell_amount_raw:u64, min_sol_output_raw:u64, recipient:Pubkey].
            13 => {
                let round_nonce = read_u64_at(rest, 0)?;
                let sell_amount_raw = read_u64_at(rest, 8)?;
                let min_sol_output_raw = read_u64_at(rest, 16)?;
                let (recipient, _) = read_pubkey_at(rest, 24)?;
                Ok(Self::StealthSellPda(StealthPdaArgs {
                    round_nonce,
                    sell_amount_raw,
                    min_sol_output_raw,
                    recipient,
                }))
            }
            // Tag 15: reclaim signer lamports to recipient.
            // Layout: [recipient:Pubkey].
            15 => {
                let (recipient, _) = read_pubkey_at(rest, 0)?;
                Ok(Self::CloseBurner(CloseBurnerArgs { recipient }))
            }
            19 => {
                let round_nonce = read_u64_at(rest, 0)?;
                let sell_amount_raw = read_u64_at(rest, 8)?;
                let min_sol_output_raw = read_u64_at(rest, 16)?;
                let (recipient, _) = read_pubkey_at(rest, 24)?;
                Ok(Self::StealthSellPdaKeep(StealthPdaArgs {
                    round_nonce,
                    sell_amount_raw,
                    min_sol_output_raw,
                    recipient,
                }))
            }
            _ => Err(SettlementError::InvalidInstruction.into()),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Section 4 — Entrypoint + instruction handlers
// ──────────────────────────────────────────────────────────────────────────

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.first().is_some_and(|tag| {
        (v2::INIT_DISTRIBUTION_V2_TAG..=v2::REFUND_DISTRIBUTION_V2_TAG).contains(tag)
    }) {
        return v2::process_v2_instruction(program_id, accounts, instruction_data);
    }
    match VaultInstruction::unpack(instruction_data)? {
        VaultInstruction::InitRouteVault(args) => {
            process_init_route_vault(program_id, accounts, args)
        }
        VaultInstruction::SellToVault(args) => process_sell_to_vault(program_id, accounts, args),
        VaultInstruction::SettleAndBuy(args) => process_settle_and_buy(program_id, accounts, args),
        VaultInstruction::RefundVault(args) => process_refund_vault(program_id, accounts, args),
        VaultInstruction::CloseRouteVault(args) => {
            process_close_route_vault(program_id, accounts, args)
        }
        VaultInstruction::SettleVaultToBuyer(args) => {
            process_settle_vault_to_buyer(program_id, accounts, args)
        }
        VaultInstruction::BuyWithBuyerBalance(args) => {
            process_buy_with_buyer_balance(program_id, accounts, args)
        }
        VaultInstruction::StealthSellPda(args) => {
            process_stealth_sell_pda(program_id, accounts, args)
        }
        VaultInstruction::CloseBurner(args) => process_close_burner(program_id, accounts, args),
        VaultInstruction::StealthSellPdaKeep(args) => {
            process_stealth_sell_pda_keep(program_id, accounts, args)
        }
    }
}

fn process_init_route_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: InitRouteVaultArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let payer = next_account_info(account_iter)?;
    let authority = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let mint = next_account_info(account_iter)?;
    let seller = next_account_info(account_iter)?;
    let buyer = next_account_info(account_iter)?;
    let system_program = next_account_info(account_iter)?;
    let rent_sysvar = next_account_info(account_iter)?;

    require_signer(payer)?;
    require_signer(authority)?;
    if seller.key == buyer.key {
        return Err(SettlementError::SameSellerBuyer.into());
    }
    let (expected_state, state_bump) =
        route_state_pda(program_id, authority.key, mint.key, seller.key, buyer.key);
    let (expected_vault, vault_bump) =
        route_vault_pda(program_id, authority.key, mint.key, seller.key, buyer.key);
    require_key(state.key, &expected_state)?;
    require_key(vault.key, &expected_vault)?;

    let rent = Rent::from_account_info(rent_sysvar)?;
    let route_bump_seed = [state_bump];
    let route_seeds: &[&[u8]] = &[
        ROUTE_SEED,
        authority.key.as_ref(),
        mint.key.as_ref(),
        seller.key.as_ref(),
        buyer.key.as_ref(),
        &route_bump_seed,
    ];
    let vault_bump_seed = [vault_bump];
    let vault_seeds: &[&[u8]] = &[
        VAULT_SEED,
        authority.key.as_ref(),
        mint.key.as_ref(),
        seller.key.as_ref(),
        buyer.key.as_ref(),
        &vault_bump_seed,
    ];

    if state.lamports() == 0 {
        let lamports = rent.minimum_balance(RouteState::LEN);
        invoke_signed(
            &system_instruction::create_account(
                payer.key,
                state.key,
                lamports,
                RouteState::LEN as u64,
                program_id,
            ),
            &[payer.clone(), state.clone(), system_program.clone()],
            &[route_seeds],
        )?;
    }
    if state.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if state.data_len() < RouteState::LEN {
        return Err(SettlementError::AccountTooSmall.into());
    }

    if vault.lamports() == 0 {
        let lamports = rent.minimum_balance(0);
        invoke_signed(
            &system_instruction::create_account(payer.key, vault.key, lamports, 0, program_id),
            &[payer.clone(), vault.clone(), system_program.clone()],
            &[vault_seeds],
        )?;
    }
    if vault.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Idempotent re-initialisation: if a valid route already exists for this
    // (authority, mint, seller, buyer) tuple we MUST NOT reset the running
    // counters — doing so would erase a pending buy budget and lifetime totals
    // if init is re-invoked between sell_to_vault and settle. Preserve the
    // existing state and only refresh the expiry window.
    let state_value = if state.data_len() >= RouteState::LEN
        && RouteState::unpack(&state.try_borrow_data()?).is_ok()
    {
        let mut existing = RouteState::unpack(&state.try_borrow_data()?)?;
        existing.bump = state_bump;
        existing.vault_bump = vault_bump;
        existing.closed = false;
        existing.authority = *authority.key;
        existing.mint = *mint.key;
        existing.seller = *seller.key;
        existing.buyer = *buyer.key;
        existing.settlement_id = args.settlement_id;
        existing.expires_slot = args.expires_slot;
        existing
    } else {
        let created_slot = Clock::get().map(|clock| clock.slot).unwrap_or(0);
        RouteState {
            bump: state_bump,
            vault_bump,
            closed: false,
            authority: *authority.key,
            mint: *mint.key,
            seller: *seller.key,
            buyer: *buyer.key,
            settlement_id: args.settlement_id,
            expires_slot: args.expires_slot,
            created_slot,
            last_deposit_lamports: 0,
            pending_buy_lamports: 0,
            total_deposited_lamports: 0,
            total_spent_lamports: 0,
        }
    };
    state_value.pack(&mut state.try_borrow_mut_data()?)?;

    msg!("Smart Distribution: Step 1");
    Ok(())
}

fn process_sell_to_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SellToVaultArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let seller = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let clock_sysvar = next_account_info(account_iter)?;
    require_signer(authority)?;
    require_signer(seller)?;

    let mut state_value = load_route(
        program_id,
        state,
        vault,
        authority.key,
        Some(seller.key),
        None,
    )?;
    if state_value.closed {
        return Err(SettlementError::RouteClosed.into());
    }
    if state_value.seller != *seller.key {
        return Err(SettlementError::InvalidRoute.into());
    }
    let clock = Clock::from_account_info(clock_sysvar)?;
    require_not_expired(clock.slot, args.expires_slot)?;

    let pump_accounts: Vec<AccountInfo> = account_iter.cloned().collect();
    require_pump_sell_accounts(&pump_accounts, &state_value)?;

    let seller_lamports_before = seller.lamports();
    let sell_ix = pump_sell_v2_instruction(
        &pump_accounts,
        args.sell_amount_raw,
        args.min_sol_output_raw,
    )?;
    invoke_pump(&sell_ix, &pump_accounts)?;
    let seller_lamports_after = seller.lamports();
    let deposited_lamports = seller_lamports_after
        .checked_sub(seller_lamports_before)
        .ok_or(SettlementError::SlippageExceeded)?;
    if deposited_lamports < args.min_sol_output_raw {
        return Err(SettlementError::SlippageExceeded.into());
    }
    if deposited_lamports > args.max_deposit_lamports {
        return Err(SettlementError::DepositCapExceeded.into());
    }

    let system_program = &pump_accounts[23];
    transfer_from_signer_with_system(seller, vault, system_program, deposited_lamports)?;

    state_value.settlement_id = args.settlement_id;
    state_value.expires_slot = args.expires_slot;
    state_value.last_deposit_lamports = deposited_lamports;
    state_value.pending_buy_lamports = 0;
    state_value.total_deposited_lamports =
        checked_add_u64(state_value.total_deposited_lamports, deposited_lamports)?;
    state_value.pack(&mut state.try_borrow_mut_data()?)?;

    msg!("Smart Distribution: Step 2");
    Ok(())
}

fn process_settle_and_buy(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SettleAndBuyArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let buyer = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let clock_sysvar = next_account_info(account_iter)?;
    require_signer(authority)?;
    require_signer(buyer)?;

    let mut state_value = load_route(
        program_id,
        state,
        vault,
        authority.key,
        None,
        Some(buyer.key),
    )?;
    if state_value.closed {
        return Err(SettlementError::RouteClosed.into());
    }
    if state_value.settlement_id != args.settlement_id {
        return Err(SettlementError::InvalidRoute.into());
    }
    let clock = Clock::from_account_info(clock_sysvar)?;
    require_not_expired(clock.slot, args.expires_slot)?;

    let remaining_accounts: Vec<AccountInfo> = account_iter.cloned().collect();
    let (pump_accounts, tip_recipient) =
        split_buy_and_tip_accounts(remaining_accounts, args.jito_tip_lamports)?;
    require_pump_buy_accounts(&pump_accounts, &state_value)?;

    let rent = current_rent()?;
    let available_lamports = vault_available_lamports(vault, &rent)?;
    if available_lamports == 0 {
        return Err(SettlementError::VaultEmpty.into());
    }
    let total_budget_lamports = available_lamports.min(args.max_buy_lamports);
    if total_budget_lamports == 0 || total_budget_lamports <= args.jito_tip_lamports {
        return Err(SettlementError::VaultEmpty.into());
    }
    let buy_budget_lamports = total_budget_lamports
        .checked_sub(args.jito_tip_lamports)
        .ok_or(SettlementError::MathOverflow)?;

    let quote = dynamic_buy_quote(
        &pump_accounts[10].try_borrow_data()?,
        &pump_accounts[0].try_borrow_data()?,
        &pump_accounts[22].try_borrow_data()?,
        buy_budget_lamports,
    )?;
    if quote.amount_raw < args.min_buy_amount_raw {
        return Err(SettlementError::SlippageExceeded.into());
    }

    transfer_from_program_owned(vault, buyer, total_budget_lamports)?;

    let buyer_lamports_after_funding = buyer.lamports();
    let buy_ix = pump_buy_v2_instruction(&pump_accounts, quote.amount_raw, buy_budget_lamports)?;
    invoke_pump(&buy_ix, &pump_accounts)?;
    let buyer_lamports_after_buy = buyer.lamports();
    let actual_spent = buyer_lamports_after_funding
        .checked_sub(buyer_lamports_after_buy)
        .unwrap_or(buy_budget_lamports)
        .min(buy_budget_lamports);
    maybe_tip_from_buyer(
        buyer,
        tip_recipient.as_ref(),
        &pump_accounts[24],
        args.jito_tip_lamports,
    )?;
    let total_spent_with_tip = checked_add_u64(actual_spent, args.jito_tip_lamports)?;

    state_value.expires_slot = args.expires_slot;
    state_value.pending_buy_lamports = 0;
    state_value.total_spent_lamports =
        checked_add_u64(state_value.total_spent_lamports, total_spent_with_tip)?;
    state_value.pack(&mut state.try_borrow_mut_data()?)?;

    msg!("Smart Distribution: Step 3");
    Ok(())
}

fn process_settle_vault_to_buyer(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SettleVaultArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let buyer = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let clock_sysvar = next_account_info(account_iter)?;
    require_signer(authority)?;
    require_signer(buyer)?;

    let mut state_value = load_route(
        program_id,
        state,
        vault,
        authority.key,
        None,
        Some(buyer.key),
    )?;
    if state_value.closed {
        return Err(SettlementError::RouteClosed.into());
    }
    if state_value.settlement_id != args.settlement_id {
        return Err(SettlementError::InvalidRoute.into());
    }
    let clock = Clock::from_account_info(clock_sysvar)?;
    require_not_expired(clock.slot, args.expires_slot)?;

    let rent = current_rent()?;
    let available_lamports = vault_available_lamports(vault, &rent)?;
    if available_lamports == 0 {
        return Err(SettlementError::VaultEmpty.into());
    }
    transfer_from_program_owned(vault, buyer, available_lamports)?;
    state_value.pending_buy_lamports = available_lamports;
    state_value.expires_slot = args.expires_slot;
    state_value.pack(&mut state.try_borrow_mut_data()?)?;

    msg!("Smart Distribution: Step 4");
    Ok(())
}

fn process_buy_with_buyer_balance(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SettleAndBuyArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let buyer = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let clock_sysvar = next_account_info(account_iter)?;
    require_signer(authority)?;
    require_signer(buyer)?;

    let mut state_value = load_route(
        program_id,
        state,
        vault,
        authority.key,
        None,
        Some(buyer.key),
    )?;
    if state_value.closed {
        return Err(SettlementError::RouteClosed.into());
    }
    if state_value.settlement_id != args.settlement_id {
        return Err(SettlementError::InvalidRoute.into());
    }
    let clock = Clock::from_account_info(clock_sysvar)?;
    require_not_expired(clock.slot, args.expires_slot)?;

    let remaining_accounts: Vec<AccountInfo> = account_iter.cloned().collect();
    let (pump_accounts, tip_recipient) =
        split_buy_and_tip_accounts(remaining_accounts, args.jito_tip_lamports)?;
    require_pump_buy_accounts(&pump_accounts, &state_value)?;

    let total_budget_lamports = buyer
        .lamports()
        .min(state_value.pending_buy_lamports)
        .min(args.max_buy_lamports);
    if total_budget_lamports == 0 || total_budget_lamports <= args.jito_tip_lamports {
        return Err(SettlementError::VaultEmpty.into());
    }
    let buy_budget_lamports = total_budget_lamports
        .checked_sub(args.jito_tip_lamports)
        .ok_or(SettlementError::MathOverflow)?;
    let quote = dynamic_buy_quote(
        &pump_accounts[10].try_borrow_data()?,
        &pump_accounts[0].try_borrow_data()?,
        &pump_accounts[22].try_borrow_data()?,
        buy_budget_lamports,
    )?;
    if quote.amount_raw < args.min_buy_amount_raw {
        return Err(SettlementError::SlippageExceeded.into());
    }

    let buyer_lamports_before_buy = buyer.lamports();
    let buy_ix = pump_buy_v2_instruction(&pump_accounts, quote.amount_raw, buy_budget_lamports)?;
    invoke_pump(&buy_ix, &pump_accounts)?;
    let buyer_lamports_after_buy = buyer.lamports();
    let actual_spent = buyer_lamports_before_buy
        .checked_sub(buyer_lamports_after_buy)
        .unwrap_or(buy_budget_lamports)
        .min(buy_budget_lamports);
    maybe_tip_from_buyer(
        buyer,
        tip_recipient.as_ref(),
        &pump_accounts[24],
        args.jito_tip_lamports,
    )?;
    let total_spent_with_tip = checked_add_u64(actual_spent, args.jito_tip_lamports)?;

    state_value.expires_slot = args.expires_slot;
    state_value.pending_buy_lamports = 0;
    state_value.total_spent_lamports =
        checked_add_u64(state_value.total_spent_lamports, total_spent_with_tip)?;
    state_value.pack(&mut state.try_borrow_mut_data()?)?;

    msg!("Smart Distribution: Step 5");
    Ok(())
}

fn process_refund_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: RouteOnlyArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let refund_recipient = next_account_info(account_iter)?;
    require_signer(authority)?;

    let state_value = load_route(program_id, state, vault, authority.key, None, None)?;
    if state_value.closed {
        return Err(SettlementError::RouteClosed.into());
    }
    if state_value.settlement_id != args.settlement_id {
        return Err(SettlementError::InvalidRoute.into());
    }
    // SECURITY: the refund recipient MUST be the route's seller. The seller
    // sold their tokens into the vault; only they are entitled to receive the
    // SOL back if a settlement is aborted. This prevents the authority from
    // redirecting vault SOL to an arbitrary wallet (loss-of-key drain).
    require_key(refund_recipient.key, &state_value.seller)?;
    let rent = current_rent()?;
    let refundable = vault_available_lamports(vault, &rent)?;
    if refundable > 0 {
        transfer_from_program_owned(vault, refund_recipient, refundable)?;
    }
    msg!("Smart Distribution: Step 6");
    Ok(())
}

fn process_close_route_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: RouteOnlyArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let state = next_account_info(account_iter)?;
    let vault = next_account_info(account_iter)?;
    let close_recipient = next_account_info(account_iter)?;
    require_signer(authority)?;

    let mut state_value = load_route(program_id, state, vault, authority.key, None, None)?;
    if state_value.settlement_id != args.settlement_id {
        return Err(SettlementError::InvalidRoute.into());
    }
    // SECURITY: the close recipient MUST be the route's seller. The seller
    // (acting as the route's payer) funded the RouteState + vault rent at
    // init; close reclaims that rent, so it must return to the same wallet.
    // The vault SOL is empty after a successful buyback, so binding the
    // recipient to the seller only constrains the rent refund — which is
    // exactly what we want. This prevents the authority from sweeping
    // residual vault lamports or rent to an arbitrary wallet.
    require_key(close_recipient.key, &state_value.seller)?;
    let vault_lamports = vault.lamports();
    if vault_lamports > 0 {
        transfer_from_program_owned(vault, close_recipient, vault_lamports)?;
    }
    state_value.closed = true;
    state_value.pack(&mut state.try_borrow_mut_data()?)?;
    let state_lamports = state.lamports();
    if state_lamports > 0 {
        transfer_from_program_owned(state, close_recipient, state_lamports)?;
    }
    msg!("Smart Distribution: Step 7");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Tag 13 — Stealth Sell PDA
// ──────────────────────────────────────────────────────────────────────────
//
// One-shot PDA sells token through Pump SellV2 then self-destructs:
//
//   authority        — signer, owns the program (gating only)
//   mint             — token mint (must match pump_accounts[1])
//   proxy_pda        — derived PDA [STEALTH_SEED, authority, mint, round_nonce]
//                       this account does NOT need to exist as a system
//                       account or have any lamports: Pump SellV2 + Token
//                       CloseAccount only require the *signer bit*, which the
//                       program supplies via invoke_signed.
//   recipient        — receives wSOL ATA close + base ATA close + drain.
//                       Bound to args.recipient on-chain.
//   pump_accounts    — 26-account Pump SellV2 layout. pump_accounts[13] must
//                       equal proxy_pda (the PDA is the user/seller).
//   quote_ata        — wSOL ATA owned by proxy_pda (pump_accounts[14])
//   base_ata         — base token ATA owned by proxy_pda (pump_accounts[14] base)
//   token_program    — SPL token program id
//   system_program   — System program (for drain fallback)
//
// Flow:
//   1. verify authority signed
//   2. verify proxy_pda matches derived PDA for (authority, mint, nonce)
//   3. verify recipient key == args.recipient
//   4. verify pump_accounts[1] == mint, pump_accounts[13] == proxy_pda
//   5. Pump SellV2 (CPI signed by PDA via invoke_signed) — PDA receives wSOL
//   6. CloseAccount quote ATA (wSOL) → recipient (PDA signs via invoke_signed)
//   7. CloseAccount base ATA → recipient (PDA signs via invoke_signed)
//   8. Forward any residual SOL in proxy_pda → recipient (program-owned: 0 floor)
//
// After step 8 the PDA has 0 lamports. No rent is left behind.

fn stealth_pda(
    program_id: &Pubkey,
    authority: &Pubkey,
    mint: &Pubkey,
    round_nonce: u64,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            STEALTH_SEED,
            authority.as_ref(),
            mint.as_ref(),
            &round_nonce.to_le_bytes(),
        ],
        program_id,
    )
}

fn process_stealth_sell_pda(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: StealthPdaArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let mint = next_account_info(account_iter)?;
    let proxy_pda = next_account_info(account_iter)?;
    let recipient = next_account_info(account_iter)?;

    require_signer(authority)?;
    let (expected_pda, pda_bump) =
        stealth_pda(program_id, authority.key, mint.key, args.round_nonce);
    require_key(proxy_pda.key, &expected_pda)?;
    // Bind recipient on-chain to args.recipient so a leaked authority cannot
    // redirect wSOL/base-ATA close + drain to an attacker wallet.
    require_key(recipient.key, &args.recipient)?;

    // Pump SellV2 CPI accounts (26-account layout, same as route/pool sell).
    // Followed by [base_ata] for the post-sell base ATA close.
    let pump_accounts: Vec<AccountInfo> = account_iter.cloned().collect();
    if pump_accounts.len() < 26 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    require_key(pump_accounts[1].key, mint.key)?;
    require_key(pump_accounts[13].key, proxy_pda.key)?;
    require_key(pump_accounts[25].key, &PUMP_PROGRAM_ID)?;

    let pda_bump_seed = [pda_bump];
    let nonce_bytes = args.round_nonce.to_le_bytes();
    let signer_seeds: &[&[u8]] = &[
        STEALTH_SEED,
        authority.key.as_ref(),
        mint.key.as_ref(),
        &nonce_bytes,
        &pda_bump_seed,
    ];

    // Step 1: Pump SellV2 — PDA signs via invoke_signed.
    // The PDA is System-owned (funded by payer in prep tx) so Pump can
    // System.transfer ATA rent from it.
    let sell_ix = pump_sell_v2_instruction(
        &pump_accounts,
        args.sell_amount_raw,
        args.min_sol_output_raw,
    )?;
    invoke_signed(&sell_ix, &pump_accounts, &[signer_seeds])?;

    // Pump account layout (see pump_sell_v2_instruction):
    //   [13] user (PDA)         [14] base ATA of user   [15] quote ATA of user
    //   [3]  base token program  [4]  quote token program
    // After SellV2: base ATA has 0 tokens (we sell full balance), quote ATA
    // (wSOL) holds sell output as lamports (rent + sol value).

    // Step 2: Close quote (wSOL) ATA → recipient. Reclaims sell output + rent.
    let quote_ata = &pump_accounts[15];
    let quote_token_program = &pump_accounts[4];
    close_token_account_via_pda(
        quote_ata,
        recipient,
        proxy_pda,
        quote_token_program,
        signer_seeds,
    )?;

    // Step 3: Close base ATA → recipient. Reclaims base ATA rent.
    let base_ata = &pump_accounts[14];
    let base_token_program = &pump_accounts[3];
    close_token_account_via_pda(
        base_ata,
        recipient,
        proxy_pda,
        base_token_program,
        signer_seeds,
    )?;

    // Step 4: Reassign PDA ownership from System program → our program.
    // This lets us drain PDA lamports to 0 (program-owned accounts are NOT
    // subject to the System rent-exempt floor). After reassign + drain, the
    // Forward residual SOL to recipient. Program-owned: 0 lamport floor.
    //
    // System.assign requires the account to sign. PDA signs via invoke_signed.
    // System program validates: account.owner == System AND account.is_signer.
    let system_program = &pump_accounts[23];
    invoke_signed(
        &system_instruction::assign(proxy_pda.key, program_id),
        &[proxy_pda.clone(), system_program.clone()],
        &[signer_seeds],
    )?;

    // Step 5: Drain ALL residual SOL from PDA → recipient. Now that PDA is
    // program-owned, we can move its lamports directly to 0 (no rent floor).
    if proxy_pda.lamports() > 0 {
        transfer_from_program_owned(proxy_pda, recipient, proxy_pda.lamports())?;
    }

    msg!("Smart Distribution: Step 14");
    Ok(())
}

// Tag 19: stealth_sell_pda_keep — sell token via Pump SellV2, reclaim wSOL,
// but KEEP PDA + base ATA alive for reuse. Only closes quote (wSOL) ATA.
//
// Same as tag 13 BUT skips steps 3-5 (no close base ATA, no System.assign,
// no drain PDA). PDA survives with base ATA (0 token after sell, but ATA
// exists). Next sell: transfer token into PDA base ATA → call tag 19 again.
//
// PDA must have enough lamports for rent-exempt (~890K) to stay alive.
// Caller must ensure PDA is funded with rent BEFORE calling this ix.
fn process_stealth_sell_pda_keep(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: StealthPdaArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let authority = next_account_info(account_iter)?;
    let mint = next_account_info(account_iter)?;
    let proxy_pda = next_account_info(account_iter)?;
    let recipient = next_account_info(account_iter)?;

    require_signer(authority)?;
    let (expected_pda, pda_bump) =
        stealth_pda(program_id, authority.key, mint.key, args.round_nonce);
    require_key(proxy_pda.key, &expected_pda)?;
    require_key(recipient.key, &args.recipient)?;

    let pump_accounts: Vec<AccountInfo> = account_iter.cloned().collect();
    if pump_accounts.len() < 26 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    require_key(pump_accounts[1].key, mint.key)?;
    require_key(pump_accounts[13].key, proxy_pda.key)?;
    require_key(pump_accounts[25].key, &PUMP_PROGRAM_ID)?;

    let pda_bump_seed = [pda_bump];
    let nonce_bytes = args.round_nonce.to_le_bytes();
    let signer_seeds: &[&[u8]] = &[
        STEALTH_SEED,
        authority.key.as_ref(),
        mint.key.as_ref(),
        &nonce_bytes,
        &pda_bump_seed,
    ];

    // Step 1: Pump SellV2 — PDA signs via invoke_signed.
    let sell_ix = pump_sell_v2_instruction(
        &pump_accounts,
        args.sell_amount_raw,
        args.min_sol_output_raw,
    )?;
    invoke_signed(&sell_ix, &pump_accounts, &[signer_seeds])?;

    // Step 2: Close quote (wSOL) ATA → recipient. Reclaims sell output.
    let quote_ata = &pump_accounts[15];
    let quote_token_program = &pump_accounts[4];
    close_token_account_via_pda(
        quote_ata,
        recipient,
        proxy_pda,
        quote_token_program,
        signer_seeds,
    )?;

    // NOTE: Steps 3-5 from tag 13 are SKIPPED:
    //   - NO close base ATA (keep alive for reuse)
    //   - NO System.assign PDA (PDA stays System-owned)
    //   - NO drain PDA lamports (PDA stays alive with rent-exempt)
    //
    // After this ix:
    //   - PDA: still exists, System-owned, has ~890K+ lamports (rent-exempt)
    //   - PDA base ATA: still exists, 0 token (sold), owner = PDA
    //   - PDA quote ATA: CLOSED (reclaimed wSOL)
    //   - Recipient: has sell output (SOL from wSOL close)
    //
    // To reuse: fund PDA with more lamports (if needed) + transfer token to
    // base ATA + create new quote ATA (idempotent) → call tag 19 again.
    // Quote ATA needs to be recreated each time (closed by this ix).

    msg!("Smart Distribution: Step 19 (stealth_sell_pda_keep — PDA reused)");
    Ok(())
}

// Tag 15: reclaim signer lamports. Forwards all SOL from a system-owned
// signer account to recipient by:
//   1. System.assign(signer, program_id) — reassign ownership.
//   2. transfer_from_program_owned(signer, recipient, all lamports).
//
// After this, signer = 0 lamports. Used to clean up the tx payer account.
fn process_close_burner(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: CloseBurnerArgs,
) -> ProgramResult {
    let account_iter = &mut accounts.iter();
    let burner = next_account_info(account_iter)?;
    let recipient = next_account_info(account_iter)?;
    let system_program = next_account_info(account_iter)?;

    require_signer(burner)?;
    require_key(recipient.key, &args.recipient)?;

    // Step 1: reassign burner ownership System → our program. Burner signs
    // (it signed the tx header as payer, so is_signer is true). System.assign
    // requires the account to be signer AND currently System-owned.
    if burner.owner != system_program.key {
        return Err(ProgramError::IncorrectProgramId);
    }
    system_assign(burner, program_id, system_program)?;

    // Step 2: drain ALL lamports. Now that we own the burner, we can move its
    // entire balance (no rent-exempt floor for program-owned accounts).
    if burner.lamports() > 0 {
        transfer_from_program_owned(burner, recipient, burner.lamports())?;
    }

    msg!("Smart Distribution: Step 15");
    Ok(())
}

/// CPI helper: close a token account owned by a PDA, routing rent + balance to
/// `recipient`. The PDA signs the SPL Token CloseAccount instruction via
/// `invoke_signed` using `signer_seeds`.
fn close_token_account_via_pda<'a>(
    account: &AccountInfo<'a>,
    recipient: &AccountInfo<'a>,
    owner_pda: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    signer_seeds: &[&[u8]],
) -> ProgramResult {
    // SPL Token CloseAccount discriminator is a single byte [9].
    let ix = Instruction {
        program_id: *token_program.key,
        data: vec![9_u8],
        accounts: vec![
            AccountMeta::new(*account.key, false),
            AccountMeta::new(*recipient.key, false),
            AccountMeta::new_readonly(*owner_pda.key, true),
        ],
    };
    invoke_signed(
        &ix,
        &[
            account.clone(),
            recipient.clone(),
            owner_pda.clone(),
            token_program.clone(),
        ],
        &[signer_seeds],
    )
}

// ──────────────────────────────────────────────────────────────────────────
// Section 5 — PDA derivation, route loading, account validation
// ──────────────────────────────────────────────────────────────────────────

fn route_state_pda(
    program_id: &Pubkey,
    authority: &Pubkey,
    mint: &Pubkey,
    seller: &Pubkey,
    buyer: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            ROUTE_SEED,
            authority.as_ref(),
            mint.as_ref(),
            seller.as_ref(),
            buyer.as_ref(),
        ],
        program_id,
    )
}

fn route_vault_pda(
    program_id: &Pubkey,
    authority: &Pubkey,
    mint: &Pubkey,
    seller: &Pubkey,
    buyer: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            VAULT_SEED,
            authority.as_ref(),
            mint.as_ref(),
            seller.as_ref(),
            buyer.as_ref(),
        ],
        program_id,
    )
}

fn load_route(
    program_id: &Pubkey,
    state: &AccountInfo,
    vault: &AccountInfo,
    authority: &Pubkey,
    seller: Option<&Pubkey>,
    buyer: Option<&Pubkey>,
) -> Result<RouteState, ProgramError> {
    if state.owner != program_id || vault.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let state_value = RouteState::unpack(&state.try_borrow_data()?)?;
    if &state_value.authority != authority {
        return Err(SettlementError::InvalidRoute.into());
    }
    if seller.is_some_and(|key| state_value.seller != *key) {
        return Err(SettlementError::InvalidRoute.into());
    }
    if buyer.is_some_and(|key| state_value.buyer != *key) {
        return Err(SettlementError::InvalidRoute.into());
    }
    let (expected_state, _) = route_state_pda(
        program_id,
        &state_value.authority,
        &state_value.mint,
        &state_value.seller,
        &state_value.buyer,
    );
    let (expected_vault, _) = route_vault_pda(
        program_id,
        &state_value.authority,
        &state_value.mint,
        &state_value.seller,
        &state_value.buyer,
    );
    require_key(state.key, &expected_state)?;
    require_key(vault.key, &expected_vault)?;
    Ok(state_value)
}

// ──────────────────────────────────────────────────────────────────────────
// Section 6 — Pump.fun AMM CPI instruction builders (SellV2 / BuyV2)
// ──────────────────────────────────────────────────────────────────────────

fn require_pump_sell_accounts(accounts: &[AccountInfo], route: &RouteState) -> ProgramResult {
    if accounts.len() < 26 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    require_key(accounts[1].key, &route.mint)?;
    require_key(accounts[13].key, &route.seller)?;
    require_key(accounts[25].key, &PUMP_PROGRAM_ID)?;
    Ok(())
}

fn require_pump_buy_accounts(accounts: &[AccountInfo], route: &RouteState) -> ProgramResult {
    if accounts.len() < 27 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    require_key(accounts[1].key, &route.mint)?;
    require_key(accounts[13].key, &route.buyer)?;
    require_key(accounts[26].key, &PUMP_PROGRAM_ID)?;
    Ok(())
}

fn require_pump_sell_accounts_v2(
    accounts: &[AccountInfo],
    seller: &Pubkey,
    mint: &Pubkey,
) -> ProgramResult {
    if accounts.len() != 26 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    require_key(accounts[1].key, mint)?;
    require_key(accounts[13].key, seller)?;
    require_key(accounts[25].key, &PUMP_PROGRAM_ID)?;
    Ok(())
}

fn require_pump_buy_accounts_v2(
    accounts: &[AccountInfo],
    buyer: &Pubkey,
    mint: &Pubkey,
) -> ProgramResult {
    if accounts.len() != 27 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    require_key(accounts[1].key, mint)?;
    require_key(accounts[13].key, buyer)?;
    require_key(accounts[26].key, &PUMP_PROGRAM_ID)?;
    Ok(())
}

fn split_buy_and_tip_accounts<'a>(
    accounts: Vec<AccountInfo<'a>>,
    tip_lamports: u64,
) -> Result<(Vec<AccountInfo<'a>>, Option<AccountInfo<'a>>), ProgramError> {
    if accounts.len() < 27 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let tip_recipient = if tip_lamports > 0 {
        Some(
            accounts
                .get(27)
                .ok_or(ProgramError::NotEnoughAccountKeys)?
                .clone(),
        )
    } else {
        None
    };
    Ok((accounts[..27].to_vec(), tip_recipient))
}

fn pump_sell_v2_instruction(
    accounts: &[AccountInfo],
    amount: u64,
    min_sol_output: u64,
) -> Result<Instruction, ProgramError> {
    if accounts.len() < 26 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let metas = vec![
        AccountMeta::new_readonly(*accounts[0].key, false),
        AccountMeta::new_readonly(*accounts[1].key, false),
        AccountMeta::new_readonly(*accounts[2].key, false),
        AccountMeta::new_readonly(*accounts[3].key, false),
        AccountMeta::new_readonly(*accounts[4].key, false),
        AccountMeta::new_readonly(*accounts[5].key, false),
        AccountMeta::new(*accounts[6].key, false),
        AccountMeta::new(*accounts[7].key, false),
        AccountMeta::new(*accounts[8].key, false),
        AccountMeta::new(*accounts[9].key, false),
        AccountMeta::new(*accounts[10].key, false),
        AccountMeta::new(*accounts[11].key, false),
        AccountMeta::new(*accounts[12].key, false),
        AccountMeta::new(*accounts[13].key, true),
        AccountMeta::new(*accounts[14].key, false),
        AccountMeta::new(*accounts[15].key, false),
        AccountMeta::new(*accounts[16].key, false),
        AccountMeta::new(*accounts[17].key, false),
        AccountMeta::new_readonly(*accounts[18].key, false),
        AccountMeta::new(*accounts[19].key, false),
        AccountMeta::new(*accounts[20].key, false),
        AccountMeta::new_readonly(*accounts[21].key, false),
        AccountMeta::new_readonly(*accounts[22].key, false),
        AccountMeta::new_readonly(*accounts[23].key, false),
        AccountMeta::new_readonly(*accounts[24].key, false),
        AccountMeta::new_readonly(*accounts[25].key, false),
    ];
    Ok(Instruction {
        program_id: PUMP_PROGRAM_ID,
        accounts: metas,
        data: pump_instruction_data(PUMP_SELL_V2_DISCRIMINATOR, amount, min_sol_output),
    })
}

fn pump_buy_v2_instruction(
    accounts: &[AccountInfo],
    amount: u64,
    max_sol_cost: u64,
) -> Result<Instruction, ProgramError> {
    if accounts.len() < 27 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let metas = vec![
        AccountMeta::new_readonly(*accounts[0].key, false),
        AccountMeta::new_readonly(*accounts[1].key, false),
        AccountMeta::new_readonly(*accounts[2].key, false),
        AccountMeta::new_readonly(*accounts[3].key, false),
        AccountMeta::new_readonly(*accounts[4].key, false),
        AccountMeta::new_readonly(*accounts[5].key, false),
        AccountMeta::new(*accounts[6].key, false),
        AccountMeta::new(*accounts[7].key, false),
        AccountMeta::new(*accounts[8].key, false),
        AccountMeta::new(*accounts[9].key, false),
        AccountMeta::new(*accounts[10].key, false),
        AccountMeta::new(*accounts[11].key, false),
        AccountMeta::new(*accounts[12].key, false),
        AccountMeta::new(*accounts[13].key, true),
        AccountMeta::new(*accounts[14].key, false),
        AccountMeta::new(*accounts[15].key, false),
        AccountMeta::new(*accounts[16].key, false),
        AccountMeta::new(*accounts[17].key, false),
        AccountMeta::new_readonly(*accounts[18].key, false),
        AccountMeta::new(*accounts[19].key, false),
        AccountMeta::new(*accounts[20].key, false),
        AccountMeta::new(*accounts[21].key, false),
        AccountMeta::new_readonly(*accounts[22].key, false),
        AccountMeta::new_readonly(*accounts[23].key, false),
        AccountMeta::new_readonly(*accounts[24].key, false),
        AccountMeta::new_readonly(*accounts[25].key, false),
        AccountMeta::new_readonly(*accounts[26].key, false),
    ];
    Ok(Instruction {
        program_id: PUMP_PROGRAM_ID,
        accounts: metas,
        data: pump_instruction_data(PUMP_BUY_V2_DISCRIMINATOR, amount, max_sol_cost),
    })
}

fn pump_instruction_data(discriminator: [u8; 8], first: u64, second: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&first.to_le_bytes());
    data.extend_from_slice(&second.to_le_bytes());
    data
}

#[cfg(not(test))]
fn invoke_pump(ix: &Instruction, accounts: &[AccountInfo]) -> ProgramResult {
    invoke(ix, accounts)
}

#[cfg(test)]
fn invoke_pump(ix: &Instruction, accounts: &[AccountInfo]) -> ProgramResult {
    if ix.program_id != PUMP_PROGRAM_ID || ix.data.len() < 24 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut discriminator = [0_u8; 8];
    discriminator.copy_from_slice(&ix.data[..8]);
    if discriminator == PUMP_SELL_V2_DISCRIMINATOR {
        let bonding_curve = accounts.get(10).ok_or(ProgramError::NotEnoughAccountKeys)?;
        let user = accounts.get(13).ok_or(ProgramError::NotEnoughAccountKeys)?;
        transfer_from_program_owned(bonding_curve, user, MOCK_SELL_OUTPUT_LAMPORTS)?;
        return Ok(());
    }
    if discriminator == PUMP_BUY_V2_DISCRIMINATOR {
        let max_sol_cost = read_u64_at(&ix.data, 16)?;
        let bonding_curve = accounts.get(10).ok_or(ProgramError::NotEnoughAccountKeys)?;
        let user = accounts.get(13).ok_or(ProgramError::NotEnoughAccountKeys)?;
        transfer_from_program_owned(user, bonding_curve, max_sol_cost)?;
        return Ok(());
    }
    Err(SettlementError::InvalidInstruction.into())
}

#[cfg(not(test))]
fn transfer_from_signer_with_system<'a>(
    from: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    lamports: u64,
) -> ProgramResult {
    invoke(
        &system_instruction::transfer(from.key, to.key, lamports),
        &[from.clone(), to.clone(), system_program.clone()],
    )
}

#[cfg(test)]
fn transfer_from_signer_with_system<'a>(
    from: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    _system_program: &AccountInfo<'a>,
    lamports: u64,
) -> ProgramResult {
    require_signer(from)?;
    transfer_from_program_owned(from, to, lamports)
}

fn maybe_tip_from_buyer<'a>(
    buyer: &AccountInfo<'a>,
    tip_recipient: Option<&AccountInfo<'a>>,
    system_program: &AccountInfo<'a>,
    lamports: u64,
) -> ProgramResult {
    if lamports == 0 {
        return Ok(());
    }
    let recipient = tip_recipient.ok_or(ProgramError::NotEnoughAccountKeys)?;
    transfer_from_signer_with_system(buyer, recipient, system_program, lamports)
}

/// System.assign wrapper — non-test build uses real invoke, test build is
/// a no-op (process_close_burner is not unit-tested).
#[cfg(not(test))]
fn system_assign<'a>(
    account: &AccountInfo<'a>,
    owner: &Pubkey,
    system_program: &AccountInfo<'a>,
) -> ProgramResult {
    invoke(
        &system_instruction::assign(account.key, owner),
        &[account.clone(), system_program.clone()],
    )
}

#[cfg(test)]
fn system_assign<'a>(
    _account: &AccountInfo<'a>,
    _owner: &Pubkey,
    _system_program: &AccountInfo<'a>,
) -> ProgramResult {
    Ok(())
}

/// Test-only mock for `solana_program::program::invoke`. The real `invoke`
/// is `#[cfg(not(test))]`-gated because it requires BPF syscalls. For unit
/// tests of custody_token / release_from_custody we only need the call to
/// succeed without actually touching the runtime — the security assertions
/// (PDA derivation, recipient/owner binding) are checked before this point.
#[cfg(test)]
fn invoke(_ix: &Instruction, _accounts: &[AccountInfo]) -> ProgramResult {
    Ok(())
}

#[cfg(not(test))]
fn current_rent() -> Result<Rent, ProgramError> {
    Rent::get()
}

#[cfg(test)]
fn current_rent() -> Result<Rent, ProgramError> {
    Ok(Rent::default())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuyQuote {
    pub amount_raw: u64,
    pub total_fee_bps: u64,
}

// ──────────────────────────────────────────────────────────────────────────
// Section 7 — Bonding-curve math: dynamic buy quote + fee computation
// ──────────────────────────────────────────────────────────────────────────

fn dynamic_buy_quote(
    bonding_curve_data: &[u8],
    global_data: &[u8],
    fee_config_data: &[u8],
    spendable_quote: u64,
) -> Result<BuyQuote, ProgramError> {
    let bonding_curve = decode_bonding_curve(bonding_curve_data)?;
    let global = decode_global(global_data)?;
    let fees = compute_fees(&bonding_curve, &global, fee_config_data)?;
    let amount = tokens_out_for_spendable_quote(
        spendable_quote as u128,
        bonding_curve.virtual_token_reserves,
        bonding_curve.virtual_quote_reserves,
        fees.protocol_fee_bps as u128,
        fees.creator_fee_bps as u128,
    )?;
    let capped = amount.min(bonding_curve.real_token_reserves);
    if capped > u64::MAX as u128 {
        return Err(SettlementError::MathOverflow.into());
    }
    Ok(BuyQuote {
        amount_raw: capped as u64,
        total_fee_bps: fees.protocol_fee_bps.saturating_add(fees.creator_fee_bps),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BondingCurveState {
    virtual_token_reserves: u128,
    virtual_quote_reserves: u128,
    real_token_reserves: u128,
    token_total_supply: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GlobalFees {
    fee_basis_points: u64,
    creator_fee_basis_points: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Fees {
    protocol_fee_bps: u64,
    creator_fee_bps: u64,
}

fn decode_bonding_curve(data: &[u8]) -> Result<BondingCurveState, ProgramError> {
    if data.len() < 49 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    Ok(BondingCurveState {
        virtual_token_reserves: read_u64_le(data, 8)? as u128,
        virtual_quote_reserves: read_u64_le(data, 16)? as u128,
        real_token_reserves: read_u64_le(data, 24)? as u128,
        token_total_supply: read_u64_le(data, 40)? as u128,
    })
}

fn decode_global(data: &[u8]) -> Result<GlobalFees, ProgramError> {
    // Pump AMM global account layout (verified against live mainnet data):
    //   8 discriminator + 1 disabled(bool) + 32 disabled_reason +
    //   32 fee_recipient + 8 initial_virtual_token_reserves +
    //   8 initial_virtual_quote_reserves + 8 initial_real_token_reserves +
    //   8 token_total_supply + 8 fee_basis_points(@105) + ...
    //   creator_fee_basis_points(@154)
    if data.len() < 162 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    Ok(GlobalFees {
        fee_basis_points: read_u64_le(data, 105)?,
        creator_fee_basis_points: read_u64_le(data, 154)?,
    })
}

fn compute_fees(
    bonding_curve: &BondingCurveState,
    global: &GlobalFees,
    fee_config_data: &[u8],
) -> Result<Fees, ProgramError> {
    if let Some(fees) = fee_tier_fees(bonding_curve, fee_config_data)? {
        return Ok(fees);
    }
    Ok(Fees {
        protocol_fee_bps: global.fee_basis_points,
        creator_fee_bps: global.creator_fee_basis_points,
    })
}

fn fee_tier_fees(
    bonding_curve: &BondingCurveState,
    fee_config_data: &[u8],
) -> Result<Option<Fees>, ProgramError> {
    if fee_config_data.len() < 77 {
        return Ok(None);
    }
    let market_cap = checked_div(
        checked_mul(
            bonding_curve.virtual_quote_reserves,
            bonding_curve.token_total_supply,
        )?,
        bonding_curve.virtual_token_reserves,
    )?;
    let mut offset = 8 + 1 + 32 + 24;
    let count = read_u32_le(fee_config_data, offset)? as usize;
    offset += 4;
    if count == 0 {
        return Ok(None);
    }

    let mut first: Option<(u128, Fees)> = None;
    let mut selected: Option<(u128, Fees)> = None;
    for index in 0..count {
        if fee_config_data.len() < offset + 40 {
            return Err(SettlementError::AccountTooSmall.into());
        }
        let threshold = read_u128_le(fee_config_data, offset)?;
        let fees = Fees {
            protocol_fee_bps: read_u64_le(fee_config_data, offset + 24)?,
            creator_fee_bps: read_u64_le(fee_config_data, offset + 32)?,
        };
        if index == 0 {
            first = Some((threshold, fees));
        }
        if market_cap >= threshold {
            selected = Some((threshold, fees));
        }
        offset += 40;
    }
    if let Some((threshold, fees)) = first {
        if market_cap < threshold {
            return Ok(Some(fees));
        }
    }
    Ok(selected
        .map(|(_, fees)| fees)
        .or_else(|| first.map(|(_, fees)| fees)))
}

pub fn tokens_out_for_spendable_quote(
    spendable_quote: u128,
    virtual_token_reserves: u128,
    virtual_quote_reserves: u128,
    protocol_fee_bps: u128,
    creator_fee_bps: u128,
) -> Result<u128, ProgramError> {
    let total_fee_bps = checked_add(protocol_fee_bps, creator_fee_bps)?;
    let denom = checked_add(BASIS_POINTS_DENOMINATOR, total_fee_bps)?;
    let mut net_quote = checked_div(
        checked_mul(spendable_quote, BASIS_POINTS_DENOMINATOR)?,
        denom,
    )?;
    let protocol_fee = ceil_div(
        checked_mul(net_quote, protocol_fee_bps)?,
        BASIS_POINTS_DENOMINATOR,
    )?;
    let creator_fee = ceil_div(
        checked_mul(net_quote, creator_fee_bps)?,
        BASIS_POINTS_DENOMINATOR,
    )?;
    let fees = checked_add(protocol_fee, creator_fee)?;
    let gross = checked_add(net_quote, fees)?;
    if gross > spendable_quote {
        net_quote = net_quote
            .checked_sub(gross - spendable_quote)
            .ok_or(SettlementError::MathOverflow)?;
    }
    if net_quote <= 1 {
        return Ok(0);
    }
    checked_div(
        checked_mul(net_quote - 1, virtual_token_reserves)?,
        checked_add(virtual_quote_reserves, net_quote - 1)?,
    )
}

// ──────────────────────────────────────────────────────────────────────────
// Section 8 — SOL transfer primitives, guards, serialization helpers
// ──────────────────────────────────────────────────────────────────────────

fn vault_available_lamports(vault: &AccountInfo, rent: &Rent) -> Result<u64, ProgramError> {
    let rent_floor = rent.minimum_balance(vault.data_len());
    Ok(vault.lamports().saturating_sub(rent_floor))
}

fn transfer_from_program_owned(
    from: &AccountInfo,
    to: &AccountInfo,
    lamports: u64,
) -> ProgramResult {
    if lamports == 0 {
        return Ok(());
    }
    let from_balance = from.lamports();
    if from_balance < lamports {
        return Err(ProgramError::InsufficientFunds);
    }
    **from.try_borrow_mut_lamports()? = from_balance
        .checked_sub(lamports)
        .ok_or(SettlementError::MathOverflow)?;
    **to.try_borrow_mut_lamports()? = to
        .lamports()
        .checked_add(lamports)
        .ok_or(SettlementError::MathOverflow)?;
    Ok(())
}

fn require_signer(account: &AccountInfo) -> ProgramResult {
    if !account.is_signer {
        return Err(SettlementError::InvalidSigner.into());
    }
    Ok(())
}

fn require_key(actual: &Pubkey, expected: &Pubkey) -> ProgramResult {
    if actual != expected {
        return Err(SettlementError::AccountMismatch.into());
    }
    Ok(())
}

fn require_not_expired(current_slot: u64, expires_slot: u64) -> ProgramResult {
    if expires_slot > 0 && current_slot > expires_slot {
        return Err(SettlementError::RouteExpired.into());
    }
    Ok(())
}

fn checked_add_u64(left: u64, right: u64) -> Result<u64, ProgramError> {
    left.checked_add(right)
        .ok_or_else(|| SettlementError::MathOverflow.into())
}

fn checked_add(left: u128, right: u128) -> Result<u128, ProgramError> {
    left.checked_add(right)
        .ok_or_else(|| SettlementError::MathOverflow.into())
}

fn checked_mul(left: u128, right: u128) -> Result<u128, ProgramError> {
    left.checked_mul(right)
        .ok_or_else(|| SettlementError::MathOverflow.into())
}

fn checked_div(left: u128, right: u128) -> Result<u128, ProgramError> {
    if right == 0 {
        return Err(SettlementError::MathOverflow.into());
    }
    Ok(left / right)
}

fn ceil_div(left: u128, right: u128) -> Result<u128, ProgramError> {
    if right == 0 {
        return Err(SettlementError::MathOverflow.into());
    }
    Ok(left
        .checked_add(right - 1)
        .ok_or(SettlementError::MathOverflow)?
        / right)
}

fn read_settlement_id(data: &[u8], offset: usize) -> Result<([u8; 32], usize), ProgramError> {
    if data.len() < offset + 32 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut value = [0_u8; 32];
    value.copy_from_slice(&data[offset..offset + 32]);
    Ok((value, offset + 32))
}

fn read_pubkey_at(data: &[u8], offset: usize) -> Result<(Pubkey, usize), ProgramError> {
    if data.len() < offset + 32 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&data[offset..offset + 32]);
    Ok((Pubkey::new_from_array(bytes), offset + 32))
}

fn read_u64_at(data: &[u8], offset: usize) -> Result<u64, ProgramError> {
    if data.len() < offset + 8 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&data[offset..offset + 8]);
    Ok(u64::from_le_bytes(bytes))
}

fn read_optional_u64_at(data: &[u8], offset: usize) -> Result<u64, ProgramError> {
    if data.len() <= offset {
        return Ok(0);
    }
    read_u64_at(data, offset)
}

fn read_u64_le(data: &[u8], offset: usize) -> Result<u64, ProgramError> {
    if data.len() < offset + 8 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&data[offset..offset + 8]);
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32_le(data: &[u8], offset: usize) -> Result<u32, ProgramError> {
    if data.len() < offset + 4 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&data[offset..offset + 4]);
    Ok(u32::from_le_bytes(bytes))
}

fn read_u128_le(data: &[u8], offset: usize) -> Result<u128, ProgramError> {
    if data.len() < offset + 16 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&data[offset..offset + 16]);
    Ok(u128::from_le_bytes(bytes))
}

fn write_pubkey(data: &mut [u8], offset: &mut usize, value: &Pubkey) {
    data[*offset..*offset + 32].copy_from_slice(value.as_ref());
    *offset += 32;
}

fn read_pubkey(data: &[u8], offset: &mut usize) -> Result<Pubkey, ProgramError> {
    if data.len() < *offset + 32 {
        return Err(SettlementError::AccountTooSmall.into());
    }
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    Ok(Pubkey::new_from_array(bytes))
}

fn write_u64(data: &mut [u8], offset: &mut usize, value: u64) {
    data[*offset..*offset + 8].copy_from_slice(&value.to_le_bytes());
    *offset += 8;
}

fn read_u64(data: &[u8], offset: &mut usize) -> Result<u64, ProgramError> {
    let value = read_u64_le(data, *offset)?;
    *offset += 8;
    Ok(value)
}

fn settlement_id_hex(value: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in value {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

// ── Human-readable log helpers ────────────────────────────────────────────
// Solana program logs are the only on-chain readable surface for explorers
// (Solscan, Solana Explorer, Helius). These helpers convert raw u64 lamports
// and token raw amounts into human-readable SOL / token strings so anyone
// inspecting a transaction can immediately understand what the program did,
// without manually dividing by 1e9 or cross-referencing account indices.

/// Format lamports as a SOL string with 6 decimals, e.g. `format_sol(1_500_000_000)` → `"1.500000 SOL"`.
fn format_sol(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = lamports % 1_000_000_000;
    format!("{}.{} SOL", whole, frac)
}

/// Format a raw u64 token amount as a UI string assuming 6 decimals (Pump.fun standard), e.g. `format_token(5_000_000)` → `"5.00 token"`.
fn format_token(raw: u64) -> String {
    let whole = raw / 1_000_000;
    let frac = raw % 1_000_000;
    format!("{}.{} token", whole, frac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_system_interface::program as system_program;

    fn key(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    fn write_u64_at(data: &mut [u8], offset: usize, value: u64) {
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn bonding_curve_data() -> Vec<u8> {
        let mut data = vec![0_u8; 49];
        write_u64_at(&mut data, 8, 1_073_000_000_000_000);
        write_u64_at(&mut data, 16, 30_000_000_000);
        write_u64_at(&mut data, 24, 793_100_000_000_000);
        write_u64_at(&mut data, 40, 1_000_000_000_000_000);
        data
    }

    fn global_data() -> Vec<u8> {
        // Matches the live Pump AMM global layout used by `decode_global`:
        // fee_basis_points @ 105, creator_fee_basis_points @ 154, min len 162.
        let mut data = vec![0_u8; 162];
        write_u64_at(&mut data, 105, 0);
        write_u64_at(&mut data, 154, 0);
        data
    }

    fn sell_to_vault_data(
        settlement_id: [u8; 32],
        expires_slot: u64,
        sell_amount_raw: u64,
        min_sol_output_raw: u64,
        max_deposit_lamports: u64,
    ) -> Vec<u8> {
        let mut data = vec![1_u8];
        data.extend_from_slice(&settlement_id);
        data.extend_from_slice(&expires_slot.to_le_bytes());
        data.extend_from_slice(&sell_amount_raw.to_le_bytes());
        data.extend_from_slice(&min_sol_output_raw.to_le_bytes());
        data.extend_from_slice(&max_deposit_lamports.to_le_bytes());
        data
    }

    fn settle_vault_to_buyer_data(settlement_id: [u8; 32], expires_slot: u64) -> Vec<u8> {
        let mut data = vec![5_u8];
        data.extend_from_slice(&settlement_id);
        data.extend_from_slice(&expires_slot.to_le_bytes());
        data
    }

    fn buy_with_buyer_balance_data(
        settlement_id: [u8; 32],
        expires_slot: u64,
        max_buy_lamports: u64,
        min_buy_amount_raw: u64,
        jito_tip_lamports: u64,
    ) -> Vec<u8> {
        let mut data = vec![6_u8];
        data.extend_from_slice(&settlement_id);
        data.extend_from_slice(&expires_slot.to_le_bytes());
        data.extend_from_slice(&max_buy_lamports.to_le_bytes());
        data.extend_from_slice(&min_buy_amount_raw.to_le_bytes());
        data.extend_from_slice(&jito_tip_lamports.to_le_bytes());
        data
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

    fn dummy_account(seed: u8) -> AccountInfo<'static> {
        account_info(
            key(seed),
            system_program::id(),
            1_000_000,
            vec![],
            false,
            true,
        )
    }

    fn clock_account(slot: u64) -> AccountInfo<'static> {
        let mut account = account_info(
            solana_program::sysvar::clock::id(),
            system_program::id(),
            1,
            vec![0_u8; Clock::size_of()],
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

    fn sell_pump_accounts(
        seller: &AccountInfo<'static>,
        mint: &AccountInfo<'static>,
        global: &AccountInfo<'static>,
        bonding_curve: &AccountInfo<'static>,
        system_program_account: &AccountInfo<'static>,
        pump_program: &AccountInfo<'static>,
    ) -> Vec<AccountInfo<'static>> {
        vec![
            global.clone(),
            mint.clone(),
            dummy_account(30),
            dummy_account(31),
            dummy_account(32),
            dummy_account(33),
            dummy_account(34),
            dummy_account(35),
            dummy_account(36),
            dummy_account(37),
            bonding_curve.clone(),
            dummy_account(38),
            dummy_account(39),
            seller.clone(),
            dummy_account(40),
            dummy_account(41),
            dummy_account(42),
            dummy_account(43),
            dummy_account(44),
            dummy_account(45),
            dummy_account(46),
            dummy_account(47),
            dummy_account(48),
            system_program_account.clone(),
            dummy_account(49),
            pump_program.clone(),
        ]
    }

    fn buy_pump_accounts(
        buyer: &AccountInfo<'static>,
        mint: &AccountInfo<'static>,
        global: &AccountInfo<'static>,
        bonding_curve: &AccountInfo<'static>,
        fee_config: &AccountInfo<'static>,
        system_program_account: &AccountInfo<'static>,
        pump_program: &AccountInfo<'static>,
    ) -> Vec<AccountInfo<'static>> {
        vec![
            global.clone(),
            mint.clone(),
            dummy_account(40),
            dummy_account(41),
            dummy_account(42),
            dummy_account(43),
            dummy_account(44),
            dummy_account(45),
            dummy_account(46),
            dummy_account(47),
            bonding_curve.clone(),
            dummy_account(48),
            dummy_account(49),
            buyer.clone(),
            dummy_account(50),
            dummy_account(51),
            dummy_account(52),
            dummy_account(53),
            dummy_account(54),
            dummy_account(55),
            dummy_account(56),
            dummy_account(57),
            fee_config.clone(),
            dummy_account(58),
            system_program_account.clone(),
            dummy_account(59),
            pump_program.clone(),
        ]
    }

    fn instruction_accounts(
        prefix: Vec<AccountInfo<'static>>,
        pump_accounts: Vec<AccountInfo<'static>>,
    ) -> Vec<AccountInfo<'static>> {
        prefix.into_iter().chain(pump_accounts).collect()
    }

    #[test]
    fn route_state_round_trips() {
        let state = RouteState {
            bump: 250,
            vault_bump: 249,
            closed: false,
            authority: key(1),
            mint: key(2),
            seller: key(3),
            buyer: key(4),
            settlement_id: [9; 32],
            expires_slot: 99,
            created_slot: 10,
            last_deposit_lamports: 7,
            pending_buy_lamports: 3,
            total_deposited_lamports: 11,
            total_spent_lamports: 5,
        };
        let mut data = vec![0_u8; RouteState::LEN];
        state.pack(&mut data).unwrap();
        assert_eq!(RouteState::unpack(&data).unwrap(), state);
    }

    #[test]
    fn route_pda_is_keyed_without_settlement_id() {
        let program_id = id();
        let authority = key(1);
        let mint = key(2);
        let seller = key(3);
        let buyer = key(4);
        let (state_a, _) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        let (state_b, _) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        assert_eq!(state_a, state_b);
        assert_ne!(
            state_a,
            route_state_pda(&program_id, &authority, &mint, &buyer, &seller).0
        );
    }

    #[test]
    fn tokens_out_matches_ts_formula_shape() {
        let out = tokens_out_for_spendable_quote(
            1_000_000,
            1_073_000_000_000_000,
            30_000_000_000,
            95,
            30,
        )
        .unwrap();
        assert!(out > 0);
        assert!(out < 1_073_000_000_000_000);
    }

    #[test]
    fn tokens_out_returns_zero_for_dust_after_fees() {
        let out = tokens_out_for_spendable_quote(1, 100_000, 100_000, 95, 30).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn instruction_parser_rejects_short_data() {
        assert!(VaultInstruction::unpack(&[1, 2, 3]).is_err());
    }

    #[test]
    fn instruction_parser_accepts_sell_to_vault() {
        let mut data = vec![1_u8];
        data.extend_from_slice(&[7_u8; 32]);
        data.extend_from_slice(&123_u64.to_le_bytes());
        data.extend_from_slice(&456_u64.to_le_bytes());
        data.extend_from_slice(&789_u64.to_le_bytes());
        data.extend_from_slice(&999_u64.to_le_bytes());
        match VaultInstruction::unpack(&data).unwrap() {
            VaultInstruction::SellToVault(args) => {
                assert_eq!(args.settlement_id, [7_u8; 32]);
                assert_eq!(args.expires_slot, 123);
                assert_eq!(args.sell_amount_raw, 456);
                assert_eq!(args.min_sol_output_raw, 789);
                assert_eq!(args.max_deposit_lamports, 999);
            }
            other => panic!("unexpected instruction: {other:?}"),
        }
    }

    #[test]
    fn instruction_parser_accepts_buy_tip_extension() {
        let data = buy_with_buyer_balance_data([8_u8; 32], 123, 456, 789, 1_000);
        match VaultInstruction::unpack(&data).unwrap() {
            VaultInstruction::BuyWithBuyerBalance(args) => {
                assert_eq!(args.settlement_id, [8_u8; 32]);
                assert_eq!(args.expires_slot, 123);
                assert_eq!(args.max_buy_lamports, 456);
                assert_eq!(args.min_buy_amount_raw, 789);
                assert_eq!(args.jito_tip_lamports, 1_000);
            }
            other => panic!("unexpected instruction: {other:?}"),
        }
    }

    #[test]
    fn expired_slot_fails() {
        assert!(require_not_expired(20, 19).is_err());
        assert!(require_not_expired(20, 20).is_ok());
        assert!(require_not_expired(20, 0).is_ok());
    }

    #[test]
    fn sell_to_vault_and_settle_buy_with_mock_pump() {
        let program_id = id();
        let authority = key(1);
        let seller = key(2);
        let buyer = key(3);
        let mint = key(10);
        let settlement_id = [7_u8; 32];
        let expires_slot = 1_000;
        let (state, route_bump) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        let (vault, vault_bump) = route_vault_pda(&program_id, &authority, &mint, &seller, &buyer);

        let route = RouteState {
            bump: route_bump,
            vault_bump,
            closed: false,
            authority,
            mint,
            seller,
            buyer,
            settlement_id,
            expires_slot,
            created_slot: 1,
            last_deposit_lamports: 0,
            pending_buy_lamports: 0,
            total_deposited_lamports: 0,
            total_spent_lamports: 0,
        };
        let mut state_data = vec![0_u8; RouteState::LEN];
        route.pack(&mut state_data).unwrap();

        let authority_account = account_info(
            authority,
            system_program::id(),
            1_000_000,
            vec![],
            true,
            false,
        );
        let seller_account =
            account_info(seller, system_program::id(), 10_000_000, vec![], true, true);
        let buyer_account =
            account_info(buyer, system_program::id(), 10_000_000, vec![], true, true);
        let state_account = account_info(
            state,
            program_id,
            Rent::default().minimum_balance(RouteState::LEN),
            state_data,
            false,
            true,
        );
        let vault_account = account_info(
            vault,
            program_id,
            Rent::default().minimum_balance(0),
            vec![],
            false,
            true,
        );
        let clock_account = clock_account(10);
        let system_program_account = account_info(
            system_program::id(),
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        let pump_program_account = account_info(
            PUMP_PROGRAM_ID,
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        let mint_account =
            account_info(mint, system_program::id(), 1_000_000, vec![], false, false);
        let global_account = account_info(
            key(11),
            PUMP_PROGRAM_ID,
            1_000_000,
            global_data(),
            false,
            false,
        );
        let bonding_curve_account = account_info(
            key(12),
            PUMP_PROGRAM_ID,
            5_000_000_000,
            bonding_curve_data(),
            false,
            true,
        );
        let fee_config_account =
            account_info(key(13), PUMP_PROGRAM_ID, 1_000_000, vec![], false, false);
        let tip_account = account_info(key(14), system_program::id(), 0, vec![], false, true);

        let sell_accounts = instruction_accounts(
            vec![
                authority_account.clone(),
                seller_account.clone(),
                state_account.clone(),
                vault_account.clone(),
                clock_account.clone(),
            ],
            sell_pump_accounts(
                &seller_account,
                &mint_account,
                &global_account,
                &bonding_curve_account,
                &system_program_account,
                &pump_program_account,
            ),
        );
        process_instruction(
            &program_id,
            &sell_accounts,
            &sell_to_vault_data(
                settlement_id,
                expires_slot,
                1_000_000,
                MOCK_SELL_OUTPUT_LAMPORTS,
                MOCK_SELL_OUTPUT_LAMPORTS,
            ),
        )
        .unwrap();

        let rent_floor = Rent::default().minimum_balance(0);
        assert_eq!(
            vault_account.lamports(),
            rent_floor + MOCK_SELL_OUTPUT_LAMPORTS
        );
        let route_after_sell =
            RouteState::unpack(&state_account.try_borrow_data().unwrap()).unwrap();
        assert_eq!(
            route_after_sell.last_deposit_lamports,
            MOCK_SELL_OUTPUT_LAMPORTS
        );
        assert_eq!(
            route_after_sell.total_deposited_lamports,
            MOCK_SELL_OUTPUT_LAMPORTS
        );

        let settle_accounts = vec![
            authority_account.clone(),
            buyer_account.clone(),
            state_account.clone(),
            vault_account.clone(),
            clock_account.clone(),
        ];
        process_instruction(
            &program_id,
            &settle_accounts,
            &settle_vault_to_buyer_data(settlement_id, expires_slot),
        )
        .unwrap();

        assert_eq!(vault_account.lamports(), rent_floor);
        assert_eq!(
            buyer_account.lamports(),
            10_000_000 + MOCK_SELL_OUTPUT_LAMPORTS
        );
        let route_after_split_settle =
            RouteState::unpack(&state_account.try_borrow_data().unwrap()).unwrap();
        assert_eq!(
            route_after_split_settle.pending_buy_lamports,
            MOCK_SELL_OUTPUT_LAMPORTS
        );

        let buy_accounts = instruction_accounts(
            vec![
                authority_account,
                buyer_account.clone(),
                state_account.clone(),
                vault_account.clone(),
                clock_account.clone(),
            ],
            buy_pump_accounts(
                &buyer_account,
                &mint_account,
                &global_account,
                &bonding_curve_account,
                &fee_config_account,
                &system_program_account,
                &pump_program_account,
            ),
        );
        process_instruction(
            &program_id,
            &buy_accounts,
            &buy_with_buyer_balance_data(settlement_id, expires_slot, 10_000_000, 1, 0),
        )
        .unwrap();

        assert_eq!(vault_account.lamports(), rent_floor);
        assert_eq!(buyer_account.lamports(), 10_000_000);
        let route_after_settle =
            RouteState::unpack(&state_account.try_borrow_data().unwrap()).unwrap();
        assert_eq!(route_after_settle.pending_buy_lamports, 0);
        assert_eq!(
            route_after_settle.total_spent_lamports,
            MOCK_SELL_OUTPUT_LAMPORTS
        );

        let route = RouteState {
            bump: route_bump,
            vault_bump,
            closed: false,
            authority,
            mint,
            seller,
            buyer,
            settlement_id,
            expires_slot,
            created_slot: 1,
            last_deposit_lamports: MOCK_SELL_OUTPUT_LAMPORTS,
            pending_buy_lamports: MOCK_SELL_OUTPUT_LAMPORTS,
            total_deposited_lamports: MOCK_SELL_OUTPUT_LAMPORTS,
            total_spent_lamports: 0,
        };
        route
            .pack(&mut state_account.try_borrow_mut_data().unwrap())
            .unwrap();
        **buyer_account.try_borrow_mut_lamports().unwrap() = 10_000_000 + MOCK_SELL_OUTPUT_LAMPORTS;
        **tip_account.try_borrow_mut_lamports().unwrap() = 0;
        let buy_with_tip_accounts = instruction_accounts(
            vec![
                account_info(
                    authority,
                    system_program::id(),
                    1_000_000,
                    vec![],
                    true,
                    false,
                ),
                buyer_account.clone(),
                state_account.clone(),
                vault_account,
                clock_account,
            ],
            buy_pump_accounts(
                &buyer_account,
                &mint_account,
                &global_account,
                &bonding_curve_account,
                &fee_config_account,
                &system_program_account,
                &pump_program_account,
            )
            .into_iter()
            .chain([tip_account.clone()])
            .collect(),
        );
        process_instruction(
            &program_id,
            &buy_with_tip_accounts,
            &buy_with_buyer_balance_data(settlement_id, expires_slot, 10_000_000, 1, 1_000),
        )
        .unwrap();
        assert_eq!(buyer_account.lamports(), 10_000_000);
        assert_eq!(tip_account.lamports(), 1_000);
        let route_after_tip =
            RouteState::unpack(&state_account.try_borrow_data().unwrap()).unwrap();
        assert_eq!(
            route_after_tip.total_spent_lamports,
            MOCK_SELL_OUTPUT_LAMPORTS
        );
    }

    #[test]
    fn init_route_vault_preserves_counters_on_reinit() {
        // Regression for the footgun where re-invoking init_route_vault between
        // a sell and a settle would overwrite state and zero out the pending buy
        // budget plus lifetime totals. Re-init must keep the running counters
        // and only refresh the expiry window.
        let program_id = id();
        let authority = key(1);
        let seller = key(2);
        let buyer = key(3);
        let mint = key(10);
        let settlement_id = [7_u8; 32];
        let (state, route_bump) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        let (vault, vault_bump) = route_vault_pda(&program_id, &authority, &mint, &seller, &buyer);

        // Seed the route with an in-flight deposit that must survive re-init.
        let seeded = RouteState {
            bump: route_bump,
            vault_bump,
            closed: false,
            authority,
            mint,
            seller,
            buyer,
            settlement_id,
            expires_slot: 1_000,
            created_slot: 5,
            last_deposit_lamports: 4_000,
            pending_buy_lamports: 4_000,
            total_deposited_lamports: 12_000,
            total_spent_lamports: 8_000,
        };
        let mut state_data = vec![0_u8; RouteState::LEN];
        seeded.pack(&mut state_data).unwrap();

        let authority_account = account_info(
            authority,
            system_program::id(),
            1_000_000,
            vec![],
            true,
            false,
        );
        let payer_account =
            account_info(key(99), system_program::id(), 1_000_000, vec![], true, true);
        let state_account = account_info(
            state,
            program_id,
            Rent::default().minimum_balance(RouteState::LEN),
            state_data,
            false,
            true,
        );
        let vault_account = account_info(
            vault,
            program_id,
            Rent::default().minimum_balance(0),
            vec![],
            false,
            true,
        );
        let mint_account = account_info(mint, system_program::id(), 1, vec![], false, false);
        let seller_account = account_info(seller, system_program::id(), 1, vec![], false, false);
        let buyer_account = account_info(buyer, system_program::id(), 1, vec![], false, false);
        let system_program_account = account_info(
            system_program::id(),
            system_program::id(),
            1,
            vec![],
            false,
            false,
        );
        let rent_sysvar_account = rent_account();

        let init_accounts = vec![
            payer_account,
            authority_account,
            state_account.clone(),
            vault_account,
            mint_account,
            seller_account,
            buyer_account,
            system_program_account,
            rent_sysvar_account,
        ];
        // Re-init with a new settlement id and a longer expiry window.
        let new_settlement_id = [8_u8; 32];
        let new_expires_slot = 5_000_u64;
        let mut data = vec![0_u8];
        data.extend_from_slice(&new_settlement_id);
        data.extend_from_slice(&new_expires_slot.to_le_bytes());

        process_instruction(&program_id, &init_accounts, &data).unwrap();

        let after = RouteState::unpack(&state_account.try_borrow_data().unwrap()).unwrap();
        assert_eq!(
            after.settlement_id, new_settlement_id,
            "settlement id should refresh"
        );
        assert_eq!(
            after.expires_slot, new_expires_slot,
            "expiry window should refresh"
        );
        assert_eq!(
            after.pending_buy_lamports, 4_000,
            "pending buy must survive re-init"
        );
        assert_eq!(
            after.last_deposit_lamports, 4_000,
            "last deposit must survive re-init"
        );
        assert_eq!(
            after.total_deposited_lamports, 12_000,
            "lifetime deposit must survive"
        );
        assert_eq!(
            after.total_spent_lamports, 8_000,
            "lifetime spend must survive"
        );
        assert_eq!(after.created_slot, 5, "original created_slot must survive");
    }

    fn rent_account() -> AccountInfo<'static> {
        let mut account = account_info(
            solana_program::sysvar::rent::id(),
            system_program::id(),
            1,
            vec![0_u8; Rent::size_of()],
            false,
            false,
        );
        Rent::default().to_account_info(&mut account).unwrap();
        account
    }

    #[test]
    fn fee_tier_fees_fallback_uses_global_when_fee_config_empty() {
        // When fee_config has no tiers (< 77 bytes) the program must fall back
        // to global fees. With fees encoded at the corrected offsets, the
        // resulting buy quote must reflect those fees (net_quote < spendable),
        // whereas the buggy offsets would have produced garbage and a broken
        // quote for any curve whose fee_config is empty/short.
        let mut global = vec![0_u8; 162];
        write_u64_at(&mut global, 105, 95);
        write_u64_at(&mut global, 154, 5);
        let global_decoded = decode_global(&global).unwrap();
        let bonding_curve = decode_bonding_curve(&bonding_curve_data()).unwrap();
        let fees = compute_fees(&bonding_curve, &global_decoded, &[]).unwrap();
        assert_eq!(fees.protocol_fee_bps, 95);
        assert_eq!(fees.creator_fee_bps, 5);

        let no_fee = tokens_out_for_spendable_quote(
            1_000_000,
            bonding_curve.virtual_token_reserves,
            bonding_curve.virtual_quote_reserves,
            95,
            5,
        )
        .unwrap();
        let zero_fee = tokens_out_for_spendable_quote(
            1_000_000,
            bonding_curve.virtual_token_reserves,
            bonding_curve.virtual_quote_reserves,
            0,
            0,
        )
        .unwrap();
        assert!(no_fee < zero_fee, "fees must reduce tokens out");
        assert!(no_fee > 0);
    }

    #[test]
    fn close_route_vault_rejects_non_seller_recipient() {
        // Security regression: close_recipient MUST equal state.seller, so a
        // compromised authority cannot sweep residual vault lamports or rent
        // to an arbitrary wallet. Any other recipient is rejected.
        let program_id = id();
        let authority = key(1);
        let seller = key(2);
        let buyer = key(3);
        let mint = key(10);
        let attacker = key(99);
        let settlement_id = [7_u8; 32];
        let (state, route_bump) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        let (vault, vault_bump) = route_vault_pda(&program_id, &authority, &mint, &seller, &buyer);

        let seeded = RouteState {
            bump: route_bump,
            vault_bump,
            closed: false,
            authority,
            mint,
            seller,
            buyer,
            settlement_id,
            expires_slot: 1_000,
            created_slot: 5,
            last_deposit_lamports: 0,
            pending_buy_lamports: 0,
            total_deposited_lamports: 0,
            total_spent_lamports: 0,
        };
        let mut state_data = vec![0_u8; RouteState::LEN];
        seeded.pack(&mut state_data).unwrap();

        let authority_account = account_info(
            authority,
            system_program::id(),
            1_000_000,
            vec![],
            true,
            false,
        );
        let state_account = account_info(
            state,
            program_id,
            Rent::default().minimum_balance(RouteState::LEN),
            state_data,
            false,
            true,
        );
        // Pre-fund the vault so a wrong recipient would actually steal lamports.
        let vault_account = account_info(
            vault,
            program_id,
            Rent::default().minimum_balance(0) + 500_000,
            vec![],
            false,
            true,
        );
        let attacker_account = account_info(attacker, system_program::id(), 0, vec![], false, true);
        let close_accounts = vec![
            authority_account,
            state_account,
            vault_account.clone(),
            attacker_account,
        ];
        let mut data = vec![4_u8]; // CloseRouteVault tag
        data.extend_from_slice(&settlement_id);
        let err = process_instruction(&program_id, &close_accounts, &data).unwrap_err();
        assert_eq!(
            err,
            SettlementError::AccountMismatch.into(),
            "close to a non-seller must be rejected"
        );
        // Vault must still hold its lamports (no partial drain).
        assert_eq!(
            vault_account.lamports(),
            Rent::default().minimum_balance(0) + 500_000
        );
    }

    #[test]
    fn refund_vault_rejects_non_seller_recipient() {
        // Security regression: refund_recipient MUST equal state.seller, so the
        // authority cannot refund vault SOL to an attacker wallet mid-settlement.
        let program_id = id();
        let authority = key(1);
        let seller = key(2);
        let buyer = key(3);
        let mint = key(10);
        let attacker = key(99);
        let settlement_id = [7_u8; 32];
        let (state, route_bump) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        let (vault, vault_bump) = route_vault_pda(&program_id, &authority, &mint, &seller, &buyer);

        let seeded = RouteState {
            bump: route_bump,
            vault_bump,
            closed: false,
            authority,
            mint,
            seller,
            buyer,
            settlement_id,
            expires_slot: 1_000,
            created_slot: 5,
            last_deposit_lamports: 0,
            pending_buy_lamports: 0,
            total_deposited_lamports: 0,
            total_spent_lamports: 0,
        };
        let mut state_data = vec![0_u8; RouteState::LEN];
        seeded.pack(&mut state_data).unwrap();

        let authority_account = account_info(
            authority,
            system_program::id(),
            1_000_000,
            vec![],
            true,
            false,
        );
        let state_account = account_info(
            state,
            program_id,
            Rent::default().minimum_balance(RouteState::LEN),
            state_data,
            false,
            true,
        );
        let vault_account = account_info(
            vault,
            program_id,
            Rent::default().minimum_balance(0) + 1_000_000,
            vec![],
            false,
            true,
        );
        let attacker_account = account_info(attacker, system_program::id(), 0, vec![], false, true);
        let refund_accounts = vec![
            authority_account,
            state_account,
            vault_account.clone(),
            attacker_account,
        ];
        let mut data = vec![3_u8]; // RefundVault tag
        data.extend_from_slice(&settlement_id);
        let err = process_instruction(&program_id, &refund_accounts, &data).unwrap_err();
        assert_eq!(
            err,
            SettlementError::AccountMismatch.into(),
            "refund to a non-seller must be rejected"
        );
        assert_eq!(
            vault_account.lamports(),
            Rent::default().minimum_balance(0) + 1_000_000
        );
    }

    #[test]
    fn stealth_pda_is_unique_per_round_nonce() {
        // Each (authority, mint, round_nonce) tuple yields a distinct PDA so
        // stealth rounds never collide on chain.
        let program_id = id();
        let authority = key(1);
        let mint = key(2);
        let (pda_a, _) = stealth_pda(&program_id, &authority, &mint, 0);
        let (pda_b, _) = stealth_pda(&program_id, &authority, &mint, 1);
        let (pda_c, _) = stealth_pda(&program_id, &authority, &mint, 2);
        assert_ne!(pda_a, pda_b);
        assert_ne!(pda_b, pda_c);
        assert_ne!(pda_a, pda_c);
        // Same nonce → same PDA (deterministic).
        let (pda_a2, _) = stealth_pda(&program_id, &authority, &mint, 0);
        assert_eq!(pda_a, pda_a2);
    }

    #[test]
    fn stealth_pda_distinct_from_other_pdas() {
        // STEALTH seed must never collide with route/vault PDAs.
        let program_id = id();
        let authority = key(1);
        let mint = key(2);
        let buyer = key(3);
        let seller = key(4);
        let (stealth, _) = stealth_pda(&program_id, &authority, &mint, 0);
        let (route, _) = route_state_pda(&program_id, &authority, &mint, &seller, &buyer);
        let (vault, _) = route_vault_pda(&program_id, &authority, &mint, &seller, &buyer);
        for other in [route, vault] {
            assert_ne!(
                stealth, other,
                "stealth PDA must not collide with existing PDA seeds"
            );
        }
    }

    #[test]
    fn instruction_parser_accepts_stealth_sell_pda() {
        // Tag 13 unpack: [nonce:u64, sell_amount:u64, min_sol:u64, recipient:Pubkey]
        let nonce: u64 = 42;
        let sell_amount: u64 = 1_000_000;
        let min_sol: u64 = 5_000;
        let recipient = key(99);
        let mut data = vec![13_u8];
        data.extend_from_slice(&nonce.to_le_bytes());
        data.extend_from_slice(&sell_amount.to_le_bytes());
        data.extend_from_slice(&min_sol.to_le_bytes());
        data.extend_from_slice(&recipient.to_bytes());
        match VaultInstruction::unpack(&data).unwrap() {
            VaultInstruction::StealthSellPda(args) => {
                assert_eq!(args.round_nonce, nonce);
                assert_eq!(args.sell_amount_raw, sell_amount);
                assert_eq!(args.min_sol_output_raw, min_sol);
                assert_eq!(args.recipient, recipient);
            }
            other => panic!("expected StealthSellPda, got {:?}", other),
        }
    }

    #[test]
    fn stealth_sell_pda_rejects_wrong_pda_address() {
        // Authority must pass the exact PDA derived from [STEALTH_SEED,
        // authority, mint, round_nonce]. A random pubkey must be rejected
        // with InvalidPda (error 2) before any CPI is attempted.
        let program_id = id();
        let authority = key(1);
        let mint = key(2);
        let recipient = key(3);
        let fake_pda = key(99); // not derived from any seed
        let authority_account = account_info(
            authority,
            system_program::id(),
            1_000_000,
            vec![],
            true,
            false,
        );
        let mint_account = account_info(mint, system_program::id(), 0, vec![], false, false);
        let fake_pda_account = account_info(fake_pda, system_program::id(), 0, vec![], false, true);
        let recipient_account =
            account_info(recipient, system_program::id(), 0, vec![], false, true);
        // Pad with enough dummy accounts so account_iter doesn't run dry
        // before pump account validation. We only need to reach the PDA check.
        let mut accounts = vec![
            authority_account,
            mint_account,
            fake_pda_account,
            recipient_account,
        ];
        for i in 0_u8..30 {
            accounts.push(dummy_account(i));
        }
        let mut data = vec![13_u8];
        data.extend_from_slice(&0u64.to_le_bytes()); // round_nonce
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&recipient.to_bytes());
        let err = process_instruction(&program_id, &accounts, &data).unwrap_err();
        assert_eq!(
            err,
            SettlementError::AccountMismatch.into(),
            "non-derived PDA must be rejected"
        );
    }

    #[test]
    fn stealth_sell_pda_rejects_wrong_recipient() {
        // recipient account must equal args.recipient — a leaked authority
        // cannot redirect the close + drain to an arbitrary wallet.
        let program_id = id();
        let authority = key(1);
        let mint = key(2);
        let real_recipient = key(3);
        let attacker = key(99);
        let (proxy_pda, _) = stealth_pda(&program_id, &authority, &mint, 0);
        let authority_account = account_info(
            authority,
            system_program::id(),
            1_000_000,
            vec![],
            true,
            false,
        );
        let mint_account = account_info(mint, system_program::id(), 0, vec![], false, false);
        let proxy_pda_account =
            account_info(proxy_pda, system_program::id(), 0, vec![], false, true);
        let attacker_account = account_info(attacker, system_program::id(), 0, vec![], false, true);
        let mut accounts = vec![
            authority_account,
            mint_account,
            proxy_pda_account,
            attacker_account,
        ];
        for i in 0_u8..30 {
            accounts.push(dummy_account(i));
        }
        let mut data = vec![13_u8];
        data.extend_from_slice(&0u64.to_le_bytes()); // round_nonce
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&real_recipient.to_bytes()); // args.recipient = real, but account = attacker
        let err = process_instruction(&program_id, &accounts, &data).unwrap_err();
        assert_eq!(
            err,
            SettlementError::AccountMismatch.into(),
            "recipient account != args.recipient must be rejected"
        );
    }
}
