//! Native-SOL PumpSwap settlement. Public IDL 81091419e4457566469d4e2a27f64ed84d42419c.
//! User WSOL accounts remain intact: only this route's token delta is unwrapped.
use super::*;
use solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke,
};
const AMM: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
const PUMP: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const TEMP_SEED: &[u8] = b"distribution-wsol";
const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
const BUY: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
fn token_amount(
    account: &AccountInfo,
    mint: &Pubkey,
    user: &Pubkey,
    program: &Pubkey,
) -> Result<u64, ProgramError> {
    require_key(account.owner, program)?;
    let data = account.try_borrow_data()?;
    if data.len() < 165
        || data[0..32] != mint.to_bytes()
        || data[32..64] != user.to_bytes()
        || data[108] != 1
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    read_u64_at(&data, 64)
}
fn quote_amount(account: &AccountInfo, user: &Pubkey) -> Result<u64, ProgramError> {
    let amount = token_amount(account, &WSOL_MINT, user, &TOKEN_PROGRAM_ID)?;
    let data = account.try_borrow_data()?;
    if data.len() != 165
        || data[72..76] != [0; 4]
        || data[109..113] != [1, 0, 0, 0]
        || data[129..133] != [0; 4]
        || checked_add_u64(amount, read_u64_at(&data, 113)?)? != account.lamports()
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    Ok(amount)
}
// Passive metadata extensions cannot alter token delivery. Unknown TLVs fail closed.
fn ordinary_mint(data: &[u8], program: &Pubkey) -> ProgramResult {
    if data.len() < 82 || data.len() > 16384 || data[45] != 1 {
        return Err(SettlementError::InvalidRoute.into());
    }
    if data.len() == 82 {
        return Ok(());
    }
    if program != &TOKEN_2022
        || data.len() < 166
        || data.len() == 355
        || data[165] != 1
        || data[82..165].iter().any(|b| *b != 0)
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    let mut offset = 166;
    let mut seen = Vec::new();
    while offset < data.len() {
        if data[offset..].iter().all(|b| *b == 0) {
            break;
        }
        if offset + 4 > data.len() {
            return Err(SettlementError::InvalidRoute.into());
        }
        let kind = u16::from_le_bytes([data[offset], data[offset + 1]]);
        let size = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
        offset += 4;
        if offset + size > data.len()
            || seen.contains(&kind)
            || !match kind {
                3 => size == 32,
                18 => size == 64,
                19 => size >= 80,
                _ => false,
            }
        {
            return Err(SettlementError::InvalidRoute.into());
        }
        seen.push(kind);
        offset += size;
    }
    Ok(())
}
fn ordinary_pool(data: &[u8]) -> ProgramResult {
    if ![261, 271, 301].contains(&data.len())
        || data[..8] != [241, 154, 109, 4, 17, 177, 109, 188]
        || data[243] != 0
        || data[244] != 0
        || data[261..].iter().any(|b| *b != 0)
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    Ok(())
}
fn validate(accounts: &[AccountInfo], user: &Pubkey, mint: &Pubkey, buy: bool) -> ProgramResult {
    let core = if buy { 23 } else { 21 };
    // Ordinary routes have at most pool-v2 plus the buyback recipient pair.
    if accounts.len() < core || accounts.len() > core + 3 {
        return Err(SettlementError::InvalidRoute.into());
    }
    require_key(accounts[0].owner, &AMM)?;
    require_key(accounts[1].key, user)?;
    require_key(accounts[3].key, mint)?;
    require_key(accounts[4].key, &WSOL_MINT)?;
    require_key(accounts[12].key, &TOKEN_PROGRAM_ID)?;
    require_key(accounts[13].key, &solana_system_interface::program::id())?;
    require_key(accounts[14].key, &ASSOCIATED_TOKEN_PROGRAM_ID)?;
    require_key(accounts[16].key, &AMM)?;
    if !accounts[16].executable || ![TOKEN_PROGRAM_ID, TOKEN_2022].contains(accounts[11].key) {
        return Err(SettlementError::InvalidRoute.into());
    }
    require_key(accounts[3].owner, accounts[11].key)?;
    ordinary_mint(&accounts[3].try_borrow_data()?, accounts[11].key)?;
    require_key(
        accounts[5].key,
        &associated_token_address(user, accounts[11].key, mint),
    )?;
    require_key(
        accounts[6].key,
        &associated_token_address(user, &TOKEN_PROGRAM_ID, &WSOL_MINT),
    )?;
    let authority = Pubkey::find_program_address(&[b"pool-authority", mint.as_ref()], &PUMP).0;
    let pool = Pubkey::find_program_address(
        &[
            b"pool",
            &[0, 0],
            authority.as_ref(),
            mint.as_ref(),
            WSOL_MINT.as_ref(),
        ],
        &AMM,
    )
    .0;
    require_key(accounts[0].key, &pool)?;
    let data = accounts[0].try_borrow_data()?;
    ordinary_pool(&data)?;
    if data.len() < 261
        || data[9..11] != [0, 0]
        || data[11..43] != authority.to_bytes()
        || data[43..75] != mint.to_bytes()
        || data[75..107] != WSOL_MINT.to_bytes()
        || data[139..171] != accounts[7].key.to_bytes()
        || data[171..203] != accounts[8].key.to_bytes()
        || data[243] != 0
        || data[244] != 0
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    token_amount(&accounts[5], mint, user, accounts[11].key)?;
    quote_amount(&accounts[6], user)?;
    Ok(())
}
fn swap_instruction(accounts: &[AccountInfo], buy: bool, amount: u64, minimum: u64) -> Instruction {
    let mut data = if buy { BUY.to_vec() } else { SELL.to_vec() };
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&minimum.to_le_bytes());
    if buy {
        data.push(1);
    }
    let metas = accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: *a.key,
            is_signer: a.is_signer,
            is_writable: a.is_writable,
        })
        .collect();
    Instruction {
        program_id: AMM,
        accounts: metas,
        data,
    }
}
fn swap(accounts: &[AccountInfo], buy: bool, amount: u64, minimum: u64) -> ProgramResult {
    invoke(&swap_instruction(accounts, buy, amount, minimum), accounts)
}
fn token_ix<'a>(
    program: &AccountInfo<'a>,
    accounts: &[AccountInfo<'a>],
    metas: Vec<AccountMeta>,
    data: Vec<u8>,
) -> ProgramResult {
    let mut infos = accounts.to_vec();
    infos.push(program.clone());
    invoke(
        &Instruction {
            program_id: TOKEN_PROGRAM_ID,
            accounts: metas,
            data,
        },
        &infos,
    )
}
fn unwrap<'a>(
    program_id: &Pubkey,
    state: &AccountInfo<'a>,
    user: &AccountInfo<'a>,
    temp: &AccountInfo<'a>,
    amm: &[AccountInfo<'a>],
    amount: u64,
) -> ProgramResult {
    let (expected, bump) = Pubkey::find_program_address(
        &[TEMP_SEED, state.key.as_ref(), user.key.as_ref()],
        program_id,
    );
    require_key(temp.key, &expected)?;
    if temp.lamports() != 0
        || temp.data_len() != 0
        || temp.owner != &solana_system_interface::program::id()
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    if amount == 0 {
        return Ok(());
    }
    let bump_seed = [bump];
    let seeds: &[&[u8]] = &[TEMP_SEED, state.key.as_ref(), user.key.as_ref(), &bump_seed];
    invoke_signed(
        &system_instruction::create_account(
            user.key,
            temp.key,
            Rent::get()?.minimum_balance(165),
            165,
            &TOKEN_PROGRAM_ID,
        ),
        &[user.clone(), temp.clone(), amm[13].clone()],
        &[seeds],
    )?;
    let mut init = vec![18];
    init.extend_from_slice(user.key.as_ref());
    token_ix(
        &amm[12],
        &[temp.clone(), amm[4].clone()],
        vec![
            AccountMeta::new(*temp.key, false),
            AccountMeta::new_readonly(WSOL_MINT, false),
        ],
        init,
    )?;
    let mut transfer = vec![3];
    transfer.extend_from_slice(&amount.to_le_bytes());
    token_ix(
        &amm[12],
        &[amm[6].clone(), temp.clone(), user.clone()],
        vec![
            AccountMeta::new(*amm[6].key, false),
            AccountMeta::new(*temp.key, false),
            AccountMeta::new_readonly(*user.key, true),
        ],
        transfer,
    )?;
    // Use real SPL close even in tests: no synthetic native accounting.
    token_ix(
        &amm[12],
        &[temp.clone(), user.clone()],
        vec![
            AccountMeta::new(*temp.key, false),
            AccountMeta::new(*user.key, false),
            AccountMeta::new_readonly(*user.key, true),
        ],
        vec![9],
    )
}
pub(super) fn process_sell(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    args: SellV2Args,
) -> ProgramResult {
    if accounts.len() < 29 {
        return Err(SettlementError::InvalidRoute.into());
    }
    let (identity, tail) = accounts.split_at(7);
    let (temp, amm) = tail.split_last().ok_or(SettlementError::InvalidRoute)?;
    let (seller, buyer, mint, state, vault) = (
        &identity[0],
        &identity[1],
        &identity[2],
        &identity[3],
        &identity[4],
    );
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
        Clock::from_account_info(&identity[5])?.slot,
        route.expires_slot,
    )?;
    require_atomic_sequence_for_state(
        program_id,
        &identity[6],
        SELL_AMM_V3_TAG,
        state,
        vault,
        &route,
    )?;
    require_sell_limits(route.sell_amount_raw, &args)?;
    if seller.lamports() != route.seller_pre_sell_lamports {
        return Err(SettlementError::BaselineMismatch.into());
    }
    validate(amm, seller.key, mint.key, false)?;
    let before = quote_amount(&amm[6], seller.key)?;
    swap(amm, false, args.sell_amount_raw, args.min_sol_output_raw)?;
    let delta = quote_amount(&amm[6], seller.key)?
        .checked_sub(before)
        .ok_or(SettlementError::BaselineMismatch)?;
    unwrap(program_id, state, seller, temp, amm, delta)?;
    if quote_amount(&amm[6], seller.key)? != before {
        return Err(SettlementError::CleanupMismatch.into());
    }
    let proceeds = validated_sell_proceeds(
        route.seller_pre_sell_lamports,
        seller.lamports(),
        args.min_sol_output_raw,
        args.max_deposit_lamports,
    )?;
    transfer_from_signer_with_system(seller, vault, &amm[13], proceeds)?;
    route.status = STATUS_SOLD;
    route.vault_funded_lamports = proceeds;
    route.pack(&mut state.try_borrow_mut_data()?)
}
pub(super) fn process_buy(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    id: [u8; 32],
    minimum: u64,
    expiry: u64,
    budget: u64,
) -> ProgramResult {
    if accounts.len() < 31 {
        return Err(SettlementError::InvalidRoute.into());
    }
    let (identity, tail) = accounts.split_at(7);
    let (temp, amm) = tail.split_last().ok_or(SettlementError::InvalidRoute)?;
    let (seller, buyer, mint, state, vault) = (
        &identity[0],
        &identity[1],
        &identity[2],
        &identity[3],
        &identity[4],
    );
    require_signer(seller)?;
    require_signer(buyer)?;
    let mut route = load_route_v2(
        program_id, state, vault, seller.key, buyer.key, mint.key, &id,
    )?;
    require_status(route.status, STATUS_FUNDED)?;
    require_buyback_guard(
        minimum,
        expiry,
        route.expires_slot,
        Clock::from_account_info(&identity[5])?.slot,
    )?;
    require_atomic_sequence_for_state(
        program_id,
        &identity[6],
        BUY_AMM_V3_TAG,
        state,
        vault,
        &route,
    )?;
    if budget == 0
        || checked_add_u64(budget, Rent::get()?.minimum_balance(165))? > route.buyer_funded_lamports
        || buyer.lamports()
            != checked_add_u64(route.buyer_baseline_lamports, route.buyer_funded_lamports)?
        || vault.lamports() != 0
    {
        return Err(SettlementError::BaselineMismatch.into());
    }
    validate(amm, buyer.key, mint.key, true)?;
    let quote_before = quote_amount(&amm[6], buyer.key)?;
    let base_before = token_amount(&amm[5], mint.key, buyer.key, amm[11].key)?;
    let native_before = buyer.lamports();
    transfer_from_signer_with_system(buyer, &amm[6], &amm[13], budget)?;
    token_ix(
        &amm[12],
        &[amm[6].clone()],
        vec![AccountMeta::new(*amm[6].key, false)],
        vec![17],
    )?;
    swap(amm, true, budget, minimum)?;
    let output = token_amount(&amm[5], mint.key, buyer.key, amm[11].key)?
        .checked_sub(base_before)
        .ok_or(SettlementError::SlippageExceeded)?;
    if output < minimum {
        return Err(SettlementError::SlippageExceeded.into());
    }
    let excess = quote_amount(&amm[6], buyer.key)?
        .checked_sub(quote_before)
        .ok_or(SettlementError::BaselineMismatch)?;
    unwrap(program_id, state, buyer, temp, amm, excess)?;
    if quote_amount(&amm[6], buyer.key)? != quote_before
        || buyer.lamports() < route.buyer_baseline_lamports
    {
        return Err(SettlementError::CleanupMismatch.into());
    }
    route.status = STATUS_BOUGHT;
    route.quoted_buy_amount_raw = output;
    route.actual_spent_lamports = native_before
        .checked_sub(buyer.lamports())
        .ok_or(SettlementError::BaselineMismatch)?;
    route.residual_lamports = buyer.lamports() - route.buyer_baseline_lamports;
    route.pack(&mut state.try_borrow_mut_data()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::sysvar::instructions::{
        construct_instructions_data, BorrowedAccountMeta, BorrowedInstruction,
    };
    fn account(key: Pubkey, owner: Pubkey, lamports: u64, data: Vec<u8>) -> AccountInfo<'static> {
        AccountInfo::new(
            Box::leak(Box::new(key)),
            false,
            true,
            Box::leak(Box::new(lamports)),
            Box::leak(data.into_boxed_slice()),
            Box::leak(Box::new(owner)),
            false,
        )
    }
    #[test]
    fn amm_sequence_is_exact_and_refuses_curve_downgrade_or_mixing() {
        let keys = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        for sequence in [
            [7, 21, 9, 22, 11],
            [7, 21, 9, 20, 11],
            [7, 8, 9, 22, 11],
            [7, 21, 9, 10, 11],
            [7, 22, 9, 21, 11],
        ] {
            for current in 0..5 {
                let data = sequence.map(|tag| {
                    let mut d = vec![tag];
                    d.extend_from_slice(&[9; 32]);
                    d
                });
                let borrowed = data
                    .iter()
                    .map(|d| BorrowedInstruction {
                        program_id: &crate::ID,
                        accounts: keys
                            .iter()
                            .enumerate()
                            .map(|(i, k)| BorrowedAccountMeta {
                                pubkey: k,
                                is_signer: i < 2,
                                is_writable: i != 2,
                            })
                            .collect(),
                        data: d,
                    })
                    .collect::<Vec<_>>();
                let mut encoded = construct_instructions_data(&borrowed);
                let n = encoded.len();
                encoded[n - 2..].copy_from_slice(&(current as u16).to_le_bytes());
                let sysvar = account(
                    instructions_sysvar_id(),
                    solana_program::sysvar::id(),
                    1,
                    encoded,
                );
                let result = require_atomic_sequence(
                    &crate::ID,
                    &sysvar,
                    sequence[current],
                    &[9; 32],
                    &keys[0],
                    &keys[1],
                    &keys[2],
                    &keys[3],
                    &keys[4],
                    false,
                );
                assert_eq!(result.is_ok(), sequence == [7, 21, 9, 22, 11]);
                if sequence == [7, 21, 9, 22, 11] {
                    assert!(require_atomic_sequence(
                        &crate::ID,
                        &sysvar,
                        sequence[current],
                        &[9; 32],
                        &keys[0],
                        &keys[1],
                        &keys[2],
                        &keys[3],
                        &keys[4],
                        true
                    )
                    .is_err());
                }
            }
        }
    }
    #[test]
    fn quote_account_preserves_nonzero_baseline_and_rejects_unsynced_or_delegated_native() {
        let user = Pubkey::new_unique();
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(WSOL_MINT.as_ref());
        data[32..64].copy_from_slice(user.as_ref());
        data[64..72].copy_from_slice(&77u64.to_le_bytes());
        data[108] = 1;
        data[109] = 1;
        data[113..121].copy_from_slice(&200u64.to_le_bytes());
        let valid = account(Pubkey::new_unique(), TOKEN_PROGRAM_ID, 277, data.clone());
        assert_eq!(quote_amount(&valid, &user).unwrap(), 77);
        let unsynced = account(Pubkey::new_unique(), TOKEN_PROGRAM_ID, 278, data.clone());
        assert!(quote_amount(&unsynced, &user).is_err());
        data[72] = 1;
        let delegated = account(Pubkey::new_unique(), TOKEN_PROGRAM_ID, 277, data);
        assert!(quote_amount(&delegated, &user).is_err());
        assert!(quote_amount(&valid, &Pubkey::new_unique()).is_err());
    }
    #[test]
    fn ordinary_variant_guard_rejects_future_pool_fields_and_active_token_extensions() {
        for size in [261, 271, 301] {
            let mut pool = vec![0; size];
            pool[..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
            assert!(ordinary_pool(&pool).is_ok());
            if size > 261 {
                pool[261] = 1;
                assert!(ordinary_pool(&pool).is_err());
            }
        }
        let mut mint = vec![0; 202];
        mint[45] = 1;
        mint[165] = 1;
        mint[166..170].copy_from_slice(&[3, 0, 32, 0]);
        assert!(ordinary_mint(&mint, &TOKEN_2022).is_ok());
        mint[166] = 1;
        assert!(ordinary_mint(&mint, &TOKEN_2022).is_err());
        mint[166] = 3;
        mint[165] = 2;
        assert!(ordinary_mint(&mint, &TOKEN_2022).is_err());
    }
    #[test]
    fn cpi_instruction_enforces_signed_floor_in_both_protocol_instructions() {
        for buy in [false, true] {
            let ix = swap_instruction(&[], buy, 12345, 9876);
            assert_eq!(ix.program_id, AMM);
            assert_eq!(&ix.data[..8], if buy { &BUY } else { &SELL });
            assert_eq!(read_u64_at(&ix.data, 8).unwrap(), 12345);
            assert_eq!(read_u64_at(&ix.data, 16).unwrap(), 9876);
            assert_eq!(ix.data.len(), if buy { 25 } else { 24 });
            if buy {
                assert_eq!(ix.data[24], 1);
            }
        }
    }
    #[test]
    fn additive_wire_parsers_refuse_truncation_before_account_access() {
        for tag in [SELL_AMM_V3_TAG, BUY_AMM_V3_TAG] {
            for size in [1, 33, 49, 56, 58, 64] {
                let mut data = vec![0; size];
                data[0] = tag;
                assert!(crate::process_instruction(&crate::ID, &[], &data).is_err());
            }
        }
    }
}
