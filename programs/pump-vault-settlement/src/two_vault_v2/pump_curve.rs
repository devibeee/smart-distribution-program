//! Pump curve exact-input adapter. Official Pump IDL 81091419e4457566469d4e2a27f64ed84d42419c.
//! Newly created volume-owned quote accounts must close, including those created
//! by ordinary token-quote routes. An uncloseable account rolls back the use.
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
const PUMP: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const FEE: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const BUY: [u8; 8] = [194, 171, 28, 70, 104, 77, 91, 47];
const SELL: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
const UNSUPPORTED_TEMPORARY_CASHBACK_ACCOUNT: u32 = 2401;
const UNSUPPORTED_VENUE_VARIANT: u32 = 2402;
fn same(a: &Pubkey, b: &Pubkey) -> ProgramResult {
    if a != b {
        Err(SettlementError::AccountMismatch.into())
    } else {
        Ok(())
    }
}
fn absent(a: &AccountInfo) -> ProgramResult {
    if a.lamports() != 0 || !a.data_is_empty() || a.owner != &solana_system_interface::program::id()
    {
        Err(SettlementError::InvalidStatus.into())
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
fn indices(a: &[AccountInfo], buy: bool) -> Result<(usize, usize, usize, usize), ProgramError> {
    if a.len() != if buy { 27 } else { 26 } {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let vi = if buy { 20 } else { 19 };
    Ok((vi, vi + 4, vi + 5, vi + 6))
}
fn curve_variant(d: &[u8], quote: &Pubkey) -> ProgramResult {
    // Pump's SDK decodes the longest complete known prefix and ignores extended
    // bytes; official deployed code currently creates 174-byte curve accounts.
    if d.len() < 83
        || d[..8] != [23, 183, 248, 55, 96, 216, 172, 96]
        || d[48] != 0
        || d[82] > 1
        || d.get(123).is_some_and(|value| *value > 1)
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    if d[81] != 0 || d.get(124).is_some_and(|v| *v != 0) {
        return Err(ProgramError::Custom(UNSUPPORTED_VENUE_VARIANT));
    }
    let stored = if d.len() >= 115 {
        Pubkey::new_from_array(
            d[83..115]
                .try_into()
                .map_err(|_| ProgramError::InvalidAccountData)?,
        )
    } else {
        Pubkey::default()
    };
    let effective = if stored == Pubkey::default() {
        WSOL
    } else {
        stored
    };
    same(quote, &effective)?;
    if quote != &WSOL && d[82] != 0 {
        return Err(ProgramError::Custom(UNSUPPORTED_TEMPORARY_CASHBACK_ACCOUNT));
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
    let (vi, si, ei, pi) = indices(a, buy)?;
    if base == quote
        || ![TOKEN, TOKEN22].contains(base_tp)
        || ![TOKEN, TOKEN22].contains(quote_tp)
        || (quote == &WSOL && quote_tp != &TOKEN)
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    for (i, key) in [
        (1, base),
        (2, quote),
        (3, base_tp),
        (4, quote_tp),
        (5, &ATA),
        (13, user),
        (vi + 3, &FEE),
        (si, &solana_system_interface::program::id()),
        (pi, &PUMP),
    ] {
        same(a[i].key, key)?;
    }
    for i in [3, 4, 5, vi + 3, si, pi] {
        if !a[i].executable {
            return Err(ProgramError::IncorrectProgramId);
        }
    }
    same(a[1].owner, base_tp)?;
    same(a[2].owner, quote_tp)?;
    same(a[0].key, &pda(&[b"global"], &PUMP))?;
    same(a[0].owner, &PUMP)?;
    let global = a[0].try_borrow_data()?;
    if global.get(..8) != Some(&[167, 232, 232, 177, 200, 108, 114, 127]) || global.len() < 997 {
        return Err(ProgramError::InvalidAccountData);
    }
    if (global[41..73] != a[6].key.to_bytes()
        && !global[162..386]
            .chunks_exact(32)
            .any(|key| key == a[6].key.as_ref()))
        || !global[741..997]
            .chunks_exact(32)
            .any(|key| key == a[8].key.as_ref())
    {
        return Err(SettlementError::AccountMismatch.into());
    }
    drop(global);
    same(a[10].key, &pda(&[b"bonding-curve", base.as_ref()], &PUMP))?;
    same(a[10].owner, &PUMP)?;
    let curve = a[10].try_borrow_data()?;
    curve_variant(&curve, quote)?;
    same(a[16].key, &pda(&[b"creator-vault", &curve[49..81]], &PUMP))?;
    for (idx, owner, mint, tp) in [
        (7, a[6].key, quote, quote_tp),
        (9, a[8].key, quote, quote_tp),
        (11, a[10].key, base, base_tp),
        (12, a[10].key, quote, quote_tp),
        (14, user, base, base_tp),
        (15, user, quote, quote_tp),
        (17, a[16].key, quote, quote_tp),
    ] {
        same(a[idx].key, &ata(owner, mint, tp))?;
    }
    same(a[18].key, &pda(&[b"sharing-config", base.as_ref()], &FEE))?;
    same(
        a[vi].key,
        &pda(&[b"user_volume_accumulator", user.as_ref()], &PUMP),
    )?;
    same(a[vi + 1].key, &ata(a[vi].key, quote, quote_tp))?;
    // Existing associated cashback ATAs are deliberately unsupported: this API
    // preserves a buyer UVA but cannot record independent token-account custody.
    absent(&a[vi + 1])?;
    same(a[vi + 2].key, &pda(&[b"fee_config", PUMP.as_ref()], &FEE))?;
    if !a[vi + 2].data_is_empty() {
        same(a[vi + 2].owner, &FEE)?;
    }
    same(a[ei].key, &pda(&[b"__event_authority"], &PUMP))?;
    if buy {
        same(a[19].key, &pda(&[b"global_volume_accumulator"], &PUMP))?;
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
    let (vi, _, _, _) = indices(a, buy)?;
    if amount == 0 || min_out == 0 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut data = if buy { BUY.to_vec() } else { SELL.to_vec() };
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    let accounts = a
        .iter()
        .enumerate()
        .map(|(i, a)| AccountMeta {
            pubkey: *a.key,
            is_signer: i == 13,
            is_writable: (6..18).contains(&i) || i == vi || i == vi + 1,
        })
        .collect();
    Ok(Instruction {
        program_id: PUMP,
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
    let (vi, _, _, _) = indices(a, buy)?;
    if a[vi].lamports() == 0 {
        absent(&a[vi])?;
        Ok(false)
    } else {
        same(a[vi].owner, &PUMP)?;
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
    let (vi, si, ei, pi) = indices(a, buy)?;
    // No public Pump close accepts the associated UVA token account. Never orphan it.
    if absent(&a[vi + 1]).is_err() {
        return Err(ProgramError::Custom(UNSUPPORTED_TEMPORARY_CASHBACK_ACCOUNT));
    }
    if was_existing {
        return Ok(());
    }
    if !volume_exists(a, buy)? {
        return Ok(());
    }
    if native {
        same(a[2].key, &WSOL)?;
        let infos = [
            a[13].clone(),
            a[vi].clone(),
            a[si].clone(),
            a[ei].clone(),
            a[pi].clone(),
        ];
        let metas = vec![
            AccountMeta::new(*a[13].key, false),
            AccountMeta::new(*a[vi].key, false),
            AccountMeta::new_readonly(*a[si].key, false),
            AccountMeta::new_readonly(*a[ei].key, false),
            AccountMeta::new_readonly(PUMP, false),
        ];
        call(
            &Instruction {
                program_id: PUMP,
                accounts: metas,
                data: vec![37, 58, 35, 126, 190, 53, 228, 197],
            },
            &infos,
            seeds,
        )?;
    }
    let infos = [a[13].clone(), a[vi].clone(), a[ei].clone(), a[pi].clone()];
    call(
        &Instruction {
            program_id: PUMP,
            accounts: vec![
                AccountMeta::new(*a[13].key, true),
                AccountMeta::new(*a[vi].key, false),
                AccountMeta::new_readonly(*a[ei].key, false),
                AccountMeta::new_readonly(PUMP, false),
            ],
            data: vec![249, 69, 164, 218, 150, 103, 84, 138],
        },
        &infos,
        seeds,
    )?;
    if a[vi].lamports() != 0 {
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
        let mut a = accounts(if buy { 27 } else { 26 });
        let base = *a[1].key;
        let user = *a[13].key;
        let vi = if buy { 20 } else { 19 };
        let sys = solana_system_interface::program::id();
        for (i, key) in [
            (1, base),
            (2, quote),
            (3, TOKEN),
            (4, TOKEN),
            (5, ATA),
            (vi + 3, FEE),
            (vi + 4, sys),
            (vi + 6, PUMP),
        ] {
            a[i] = fixture_info(key, if i < 3 { TOKEN } else { sys }, vec![], i >= 3);
        }
        let mut global = vec![0; 997];
        global[..8].copy_from_slice(&[167, 232, 232, 177, 200, 108, 114, 127]);
        global[41..73].copy_from_slice(a[6].key.as_ref());
        global[741..773].copy_from_slice(a[8].key.as_ref());
        a[0] = fixture_info(pda(&[b"global"], &PUMP), PUMP, global, false);
        let mut curve = vec![0; 125];
        curve[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
        curve[83..115].copy_from_slice(quote.as_ref());
        a[10] = fixture_info(
            pda(&[b"bonding-curve", base.as_ref()], &PUMP),
            PUMP,
            curve,
            false,
        );
        a[16] = fixture_info(
            pda(&[b"creator-vault", &[0; 32]], &PUMP),
            sys,
            vec![],
            false,
        );
        for (i, owner, mint) in [
            (7, *a[6].key, quote),
            (9, *a[8].key, quote),
            (11, *a[10].key, base),
            (12, *a[10].key, quote),
            (14, user, base),
            (15, user, quote),
            (17, *a[16].key, quote),
        ] {
            a[i] = fixture_info(ata(&owner, &mint, &TOKEN), sys, vec![], false);
        }
        for (i, key) in [
            (18, pda(&[b"sharing-config", base.as_ref()], &FEE)),
            (vi, pda(&[b"user_volume_accumulator", user.as_ref()], &PUMP)),
            (vi + 2, pda(&[b"fee_config", PUMP.as_ref()], &FEE)),
            (vi + 5, pda(&[b"__event_authority"], &PUMP)),
        ] {
            a[i] = fixture_info(key, sys, vec![], false);
        }
        a[vi + 1] = fixture_info(ata(a[vi].key, &quote, &TOKEN), sys, vec![], false);
        if buy {
            a[19] = fixture_info(
                pda(&[b"global_volume_accumulator"], &PUMP),
                PUMP,
                vec![],
                false,
            );
        }
        a
    }
    #[test]
    fn canonical_native_and_custom_fixtures_validate_and_substitutions_fail() {
        for quote in [WSOL, Pubkey::new_unique()] {
            for buy in [false, true] {
                let mut a = fixture(quote, buy);
                let user = *a[13].key;
                let base = *a[1].key;
                assert!(validate(&a, &user, &base, &quote, &TOKEN, &TOKEN, buy).is_ok());
                let original = a[18].key;
                a[18].key = Box::leak(Box::new(Pubkey::new_unique()));
                assert!(validate(&a, &user, &base, &quote, &TOKEN, &TOKEN, buy).is_err());
                a[18].key = original;
                a[6].key = Box::leak(Box::new(Pubkey::new_unique()));
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
    fn exact_input_wire_and_fixed_privileges_do_not_trust_input_flags() {
        for buy in [false, true] {
            let a = accounts(if buy { 27 } else { 26 });
            let instruction = ix(&a, buy, 123, 45).unwrap();
            assert_eq!(&instruction.data[..8], if buy { &BUY } else { &SELL });
            assert_eq!(&instruction.data[8..16], &123u64.to_le_bytes());
            assert_eq!(&instruction.data[16..], &45u64.to_le_bytes());
            assert!(instruction.accounts[13].is_signer);
            assert_eq!(
                instruction.accounts.iter().filter(|a| a.is_signer).count(),
                1
            );
            let vi = if buy { 20 } else { 19 };
            for (i, meta) in instruction.accounts.iter().enumerate() {
                assert_eq!(
                    meta.is_writable,
                    (6..18).contains(&i) || i == vi || i == vi + 1
                );
            }
            assert!(ix(&a, buy, 0, 45).is_err());
            assert!(ix(&a, buy, 123, 0).is_err());
            assert!(ix(&a[..a.len() - 1], buy, 123, 45).is_err());
        }
    }
    #[test]
    fn token_quote_is_bound_not_globally_denied() {
        let mut d = vec![0; 125];
        d[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
        let q = Pubkey::new_unique();
        d[83..115].copy_from_slice(q.as_ref());
        assert!(curve_variant(&d, &q).is_ok());
        assert!(curve_variant(&d, &WSOL).is_err());
        d[82] = 1;
        assert_eq!(curve_variant(&d, &q), Err(ProgramError::Custom(2401)));
        d[83..115].fill(0);
        assert!(curve_variant(&d, &WSOL).is_ok());
    }
    #[test]
    fn rejects_unknown_curve_variants_and_complete() {
        let mut d = vec![0; 125];
        d[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
        for idx in [48, 81, 124] {
            d[idx] = 1;
            assert!(curve_variant(&d, &WSOL).is_err());
            d[idx] = 0;
        }
        assert!(curve_variant(&d[..82], &WSOL).is_err());
    }

    #[test]
    fn rejects_noncanonical_known_booleans() {
        for length in [83, 115, 124, 125, 151, 174] {
            let mut data = vec![0; length];
            data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
            for index in [48, 81, 82, 123, 124]
                .into_iter()
                .filter(|index| *index < length)
            {
                data[index] = 2;
                assert!(
                    curve_variant(&data, &WSOL).is_err(),
                    "length {length}, offset {index}"
                );
                data[index] = 0;
            }
        }
    }

    #[test]
    fn preserves_prefix_decoding_and_opaque_extended_accounts() {
        for length in [83, 84, 114, 115, 116, 123, 124, 125, 126, 151, 174, 200] {
            let mut data = vec![0; length];
            data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
            assert!(curve_variant(&data, &WSOL).is_ok(), "length {length}");
            if length >= 124 {
                data[123] = 1;
                assert!(curve_variant(&data, &WSOL).is_ok());
            }
            if length >= 115 {
                let quote = Pubkey::new_unique();
                data[83..115].copy_from_slice(quote.as_ref());
                assert!(curve_variant(&data, &quote).is_ok());
                assert!(curve_variant(&data, &WSOL).is_err());
            }
            if length > 125 {
                data[125..].fill(255);
                let quote = Pubkey::new_from_array(data[83..115].try_into().unwrap());
                assert!(curve_variant(&data, &quote).is_ok());
            }
        }
    }
}
