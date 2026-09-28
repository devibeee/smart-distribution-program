//! PumpSwap exact-input adapter; all quotes use token custody, including WSOL.
//! Cashback variants fail closed: no public instruction closes their UVA ATA.
use crate::SettlementError;
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
};
const AMM: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
const FEE: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const BUY: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
fn same(a: &Pubkey, b: &Pubkey) -> ProgramResult {
    if a != b {
        Err(SettlementError::AccountMismatch.into())
    } else {
        Ok(())
    }
}
fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}
fn ata(user: &Pubkey, mint: &Pubkey, tp: &Pubkey) -> Pubkey {
    pda(&[user.as_ref(), tp.as_ref(), mint.as_ref()], &ATA)
}
fn absent(a: &AccountInfo) -> ProgramResult {
    if a.lamports() != 0 || !a.data_is_empty() || a.owner != &solana_system_interface::program::id()
    {
        Err(SettlementError::InvalidStatus.into())
    } else {
        Ok(())
    }
}
fn pool_variant(d: &[u8]) -> ProgramResult {
    if !(d.len() == 261 || (271..=301).contains(&d.len()))
        || d[..8] != [241, 154, 109, 4, 17, 177, 109, 188]
    {
        return Err(ProgramError::InvalidAccountData);
    }
    if d[244] != 0 {
        return Err(ProgramError::Custom(2401));
    }
    if d[243] != 0
        || d.get(270).is_some_and(|b| *b != 0)
        || d.get(271..)
            .is_some_and(|tail| tail.iter().any(|b| *b != 0))
    {
        return Err(ProgramError::Custom(2402));
    }
    Ok(())
}
fn count(a: &[AccountInfo], buy: bool) -> Result<usize, ProgramError> {
    let core = if buy { 23 } else { 21 };
    if !(core + 2..=core + 3).contains(&a.len()) {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    Ok(core)
}
fn pool_token(a: &AccountInfo, mint: &Pubkey, owner: &Pubkey, tp: &Pubkey) -> ProgramResult {
    same(a.owner, tp)?;
    let d = a.try_borrow_data()?;
    if d.len() < 165 || d[..32] != mint.to_bytes() || d[32..64] != owner.to_bytes() || d[108] != 1 {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}
pub fn validate(
    a: &[AccountInfo],
    user: &Pubkey,
    base: &Pubkey,
    quote: &Pubkey,
    base_tp: &Pubkey,
    quote_tp: &Pubkey,
    buy: bool,
) -> ProgramResult {
    let core = count(a, buy)?;
    let fi = if buy { 21 } else { 19 };
    if base == quote || ![TOKEN, TOKEN22].contains(base_tp) || ![TOKEN, TOKEN22].contains(quote_tp)
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    for (i, key) in [
        (1, user),
        (3, base),
        (4, quote),
        (11, base_tp),
        (12, quote_tp),
        (13, &solana_system_interface::program::id()),
        (14, &ATA),
        (16, &AMM),
        (fi + 1, &FEE),
    ] {
        same(a[i].key, key)?;
    }
    for i in [11, 12, 13, 14, 16, fi + 1] {
        if !a[i].executable {
            return Err(ProgramError::IncorrectProgramId);
        }
    }
    same(a[3].owner, base_tp)?;
    same(a[4].owner, quote_tp)?;
    same(a[2].key, &pda(&[b"global_config"], &AMM))?;
    same(a[2].owner, &AMM)?;
    let global = a[2].try_borrow_data()?;
    if global.len() < 899
        || global[..8] != [149, 8, 156, 202, 160, 252, 176, 217]
        || !global[57..313]
            .chunks_exact(32)
            .any(|key| key == a[9].key.as_ref())
        || !global[643..899]
            .chunks_exact(32)
            .any(|key| key == a[a.len() - 2].key.as_ref())
    {
        return Err(SettlementError::AccountMismatch.into());
    }
    drop(global);
    same(a[0].owner, &AMM)?;
    let d = a[0].try_borrow_data()?;
    pool_variant(&d)?;
    same(
        a[0].key,
        &pda(
            &[
                b"pool",
                &d[9..11],
                &d[11..43],
                base.as_ref(),
                quote.as_ref(),
            ],
            &AMM,
        ),
    )?;
    if d[43..75] != base.to_bytes()
        || d[75..107] != quote.to_bytes()
        || d[139..171] != a[7].key.to_bytes()
        || d[171..203] != a[8].key.to_bytes()
    {
        return Err(SettlementError::AccountMismatch.into());
    }
    same(a[5].key, &ata(user, base, base_tp))?;
    same(a[6].key, &ata(user, quote, quote_tp))?;
    same(a[10].key, &ata(a[9].key, quote, quote_tp))?;
    same(a[18].key, &pda(&[b"creator_vault", &d[211..243]], &AMM))?;
    same(a[17].key, &ata(a[18].key, quote, quote_tp))?;
    same(a[15].key, &pda(&[b"__event_authority"], &AMM))?;
    same(a[fi].key, &pda(&[b"fee_config", AMM.as_ref()], &FEE))?;
    if !a[fi].data_is_empty() {
        same(a[fi].owner, &FEE)?;
    }
    let has_v2 = d[211..243] != [0; 32];
    let extra = core + usize::from(has_v2);
    if a.len() != extra + 2 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    if has_v2 {
        same(a[core].key, &pda(&[b"pool-v2", base.as_ref()], &AMM))?;
    }
    same(a[extra + 1].key, &ata(a[extra].key, quote, quote_tp))?;
    pool_token(&a[7], base, a[0].key, base_tp)?;
    pool_token(&a[8], quote, a[0].key, quote_tp)?;
    if buy {
        same(a[19].key, &pda(&[b"global_volume_accumulator"], &AMM))?;
        same(
            a[20].key,
            &pda(&[b"user_volume_accumulator", user.as_ref()], &AMM),
        )?;
    }
    volume_exists(a, buy)?;
    Ok(())
}
fn ix(
    a: &[AccountInfo],
    buy: bool,
    amount: u64,
    min_out: u64,
) -> Result<Instruction, ProgramError> {
    let core = count(a, buy)?;
    if amount == 0 || min_out == 0 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut data = if buy { BUY.to_vec() } else { SELL.to_vec() };
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    if buy {
        data.push(0);
    } // OptionBool(false): do not accrue optional volume.
    let last = a.len() - 1;
    let accounts = a
        .iter()
        .enumerate()
        .map(|(i, a)| AccountMeta {
            pubkey: *a.key,
            is_signer: i == 1,
            is_writable: [0, 1, 5, 6, 7, 8, 10, 17].contains(&i)
                || (buy && i == 20)
                || (i >= core && i == last),
        })
        .collect();
    Ok(Instruction {
        program_id: AMM,
        accounts,
        data,
    })
}
fn call(ix: &Instruction, a: &[AccountInfo], seeds: &[&[u8]]) -> ProgramResult {
    if seeds.is_empty() {
        invoke(ix, a)
    } else {
        invoke_signed(ix, a, &[seeds])
    }
}
pub fn swap<'a>(
    a: &[AccountInfo<'a>],
    buy: bool,
    amount: u64,
    min_out: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    call(&ix(a, buy, amount, min_out)?, a, seeds)
}
pub fn volume_exists(a: &[AccountInfo], buy: bool) -> Result<bool, ProgramError> {
    count(a, buy)?;
    if !buy {
        return Ok(false);
    }
    if a[20].lamports() == 0 {
        absent(&a[20])?;
        Ok(false)
    } else {
        same(a[20].owner, &AMM)?;
        Ok(true)
    }
}
pub fn cleanup<'a>(
    a: &[AccountInfo<'a>],
    buy: bool,
    was_existing: bool,
    native: bool,
    seeds: &[&[u8]],
) -> ProgramResult {
    count(a, buy)?;
    if native {
        return Err(SettlementError::InvalidRoute.into());
    }
    if !buy || was_existing || !volume_exists(a, buy)? {
        return Ok(());
    }
    // Noncashback pools do not receive any protocol-owned quote ATA. Closing
    // the fresh accumulator must succeed; otherwise the transaction rolls back.
    let infos = [a[1].clone(), a[20].clone(), a[15].clone(), a[16].clone()];
    call(
        &Instruction {
            program_id: AMM,
            accounts: vec![
                AccountMeta::new(*a[1].key, true),
                AccountMeta::new(*a[20].key, false),
                AccountMeta::new_readonly(*a[15].key, false),
                AccountMeta::new_readonly(AMM, false),
            ],
            data: vec![249, 69, 164, 218, 150, 103, 84, 138],
        },
        &infos,
        seeds,
    )?;
    if a[20].lamports() != 0 {
        return Err(SettlementError::CleanupMismatch.into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_info(
        key: Pubkey,
        owner: Pubkey,
        data: Vec<u8>,
        executable: bool,
    ) -> AccountInfo<'static> {
        AccountInfo::new(
            Box::leak(Box::new(key)),
            false,
            true,
            Box::leak(Box::new(0)),
            Box::leak(data.into_boxed_slice()),
            Box::leak(Box::new(owner)),
            executable,
        )
    }
    fn fixture(quote: Pubkey, buy: bool) -> Vec<AccountInfo<'static>> {
        let core = if buy { 23 } else { 21 };
        let fi = core - 2;
        let mut a = accounts(core + 2);
        let base = *a[3].key;
        let user = *a[1].key;
        let sys = solana_system_interface::program::id();
        let pool = pda(
            &[b"pool", &[0; 2], &[0; 32], base.as_ref(), quote.as_ref()],
            &AMM,
        );
        for (i, key) in [
            (3, base),
            (4, quote),
            (11, TOKEN),
            (12, TOKEN),
            (13, sys),
            (14, ATA),
            (16, AMM),
            (fi + 1, FEE),
        ] {
            a[i] = fixture_info(key, if i < 5 { TOKEN } else { sys }, vec![], i >= 11);
        }
        let mut global = vec![0; 899];
        global[..8].copy_from_slice(&[149, 8, 156, 202, 160, 252, 176, 217]);
        global[57..89].copy_from_slice(a[9].key.as_ref());
        global[643..675].copy_from_slice(a[core].key.as_ref());
        a[2] = fixture_info(pda(&[b"global_config"], &AMM), AMM, global, false);
        let mut d = vec![0; 271];
        d[..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
        d[43..75].copy_from_slice(base.as_ref());
        d[75..107].copy_from_slice(quote.as_ref());
        d[139..171].copy_from_slice(a[7].key.as_ref());
        d[171..203].copy_from_slice(a[8].key.as_ref());
        a[0] = fixture_info(pool, AMM, d, false);
        for (i, mint) in [(7, base), (8, quote)] {
            let mut t = vec![0; 165];
            t[..32].copy_from_slice(mint.as_ref());
            t[32..64].copy_from_slice(pool.as_ref());
            t[108] = 1;
            a[i] = fixture_info(*a[i].key, TOKEN, t, false);
        }
        a[18] = fixture_info(pda(&[b"creator_vault", &[0; 32]], &AMM), sys, vec![], false);
        for (i, owner, mint) in [
            (5, user, base),
            (6, user, quote),
            (10, *a[9].key, quote),
            (17, *a[18].key, quote),
            (core + 1, *a[core].key, quote),
        ] {
            a[i] = fixture_info(ata(&owner, &mint, &TOKEN), sys, vec![], false);
        }
        a[15] = fixture_info(pda(&[b"__event_authority"], &AMM), sys, vec![], false);
        a[fi] = fixture_info(
            pda(&[b"fee_config", AMM.as_ref()], &FEE),
            sys,
            vec![],
            false,
        );
        if buy {
            a[19] = fixture_info(
                pda(&[b"global_volume_accumulator"], &AMM),
                AMM,
                vec![],
                false,
            );
            a[20] = fixture_info(
                pda(&[b"user_volume_accumulator", user.as_ref()], &AMM),
                sys,
                vec![],
                false,
            );
        }
        a
    }
    #[test]
    fn canonical_custom_quotes_validate_and_pool_substitution_fails() {
        for quote in [
            pubkey!("So11111111111111111111111111111111111111112"),
            Pubkey::new_unique(),
        ] {
            for buy in [false, true] {
                let mut a = fixture(quote, buy);
                let user = *a[1].key;
                let base = *a[3].key;
                assert!(validate(&a, &user, &base, &quote, &TOKEN, &TOKEN, buy).is_ok());
                a[0].key = Box::leak(Box::new(Pubkey::new_unique()));
                assert!(validate(&a, &user, &base, &quote, &TOKEN, &TOKEN, buy).is_err());
            }
        }
    }
    fn accounts(count: usize) -> Vec<AccountInfo<'static>> {
        (0..count)
            .map(|_| {
                AccountInfo::new(
                    Box::leak(Box::new(Pubkey::new_unique())),
                    false,
                    false,
                    Box::leak(Box::new(0)),
                    Box::leak(Vec::new().into_boxed_slice()),
                    Box::leak(Box::new(Pubkey::default())),
                    false,
                )
            })
            .collect()
    }
    #[test]
    fn exact_input_wire_and_signer_are_fixed_for_both_core_layouts() {
        for buy in [false, true] {
            for extra in [2, 3] {
                let core = if buy { 23 } else { 21 };
                let a = accounts(core + extra);
                let instruction = ix(&a, buy, 123, 45).unwrap();
                assert_eq!(&instruction.data[..8], if buy { &BUY } else { &SELL });
                assert_eq!(&instruction.data[8..16], &123u64.to_le_bytes());
                assert_eq!(&instruction.data[16..24], &45u64.to_le_bytes());
                assert_eq!(instruction.data.len(), if buy { 25 } else { 24 });
                if buy {
                    assert_eq!(instruction.data[24], 0);
                }
                assert!(instruction.accounts[1].is_signer);
                assert_eq!(
                    instruction.accounts.iter().filter(|a| a.is_signer).count(),
                    1
                );
                assert!(instruction.accounts.last().unwrap().is_writable);
                assert!(!instruction.accounts[a.len() - 2].is_writable);
                assert!(ix(&a, buy, 0, 45).is_err());
                assert!(ix(&a, buy, 123, 0).is_err());
            }
        }
    }
    #[test]
    fn variants_reject_uncloseable_cashback_but_accept_virtual_quote() {
        let mut d = vec![0; 271];
        d[..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
        d[245..261].copy_from_slice(&17i128.to_le_bytes());
        assert!(pool_variant(&d).is_ok());
        d[244] = 1;
        assert_eq!(pool_variant(&d), Err(ProgramError::Custom(2401)));
        d[244] = 0;
        d[270] = 1;
        assert_eq!(pool_variant(&d), Err(ProgramError::Custom(2402)));
    }
    #[test]
    fn pool_unknown_tail_and_truncation_fail_closed() {
        let mut d = vec![0; 301];
        d[..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
        assert!(pool_variant(&d).is_ok());
        d[300] = 1;
        assert!(pool_variant(&d).is_err());
        assert!(pool_variant(&d[..244]).is_err());
    }
}
