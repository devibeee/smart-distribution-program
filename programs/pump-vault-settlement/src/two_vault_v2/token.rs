//! Strict raw-atom custody. Wire layouts verified against solana-program/token-2022
//! interface/src/{instruction.rs,extension/mod.rs,extension/transfer_fee}.
//! Scaled UI multipliers are validated but never applied to raw token amounts.
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
};

const SPL: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSTEM: Pubkey = Pubkey::new_from_array([0; 32]);

#[repr(u32)]
enum CustodyError {
    UnsupportedExtension = 700,
    UnsafeExtension,
    NotEmpty,
    CloseFailed,
}
fn err(e: CustodyError) -> ProgramError {
    ProgramError::Custom(e as u32)
}
fn valid_program(key: &Pubkey) -> ProgramResult {
    if *key == SPL || *key == TOKEN22 {
        Ok(())
    } else {
        Err(ProgramError::IncorrectProgramId)
    }
}
fn executable(program: &AccountInfo) -> ProgramResult {
    valid_program(program.key)?;
    if program.executable {
        Ok(())
    } else {
        Err(ProgramError::IncorrectProgramId)
    }
}
fn size(b: &[u8], n: usize) -> ProgramResult {
    if b.len() == n {
        Ok(())
    } else {
        Err(ProgramError::InvalidAccountData)
    }
}
fn u32_at(b: &[u8], at: usize) -> Result<u32, ProgramError> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?,
    ))
}
fn u64_at(b: &[u8], at: usize) -> Result<u64, ProgramError> {
    Ok(u64::from_le_bytes(
        b.get(at..at + 8)
            .ok_or(ProgramError::InvalidAccountData)?
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?,
    ))
}
fn coption(b: &[u8], at: usize) -> Result<u32, ProgramError> {
    let tag = u32_at(b, at)?;
    if tag > 1 {
        Err(ProgramError::InvalidAccountData)
    } else {
        Ok(tag)
    }
}
fn extensions<'a>(
    b: &'a [u8],
    base: usize,
    kind: u8,
    program: &Pubkey,
) -> Result<Vec<(u16, &'a [u8])>, ProgramError> {
    valid_program(program)?;
    if b.len() < base || b.len() == 355 {
        return Err(ProgramError::InvalidAccountData);
    }
    if b.len() == base {
        return Ok(vec![]);
    }
    if *program != TOKEN22
        || b.len() < 166
        || b[165] != kind
        || b[base..165].iter().any(|v| *v != 0)
    {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut out = Vec::new();
    let mut at = 166;
    while at < b.len() {
        if b[at..].iter().all(|v| *v == 0) {
            break;
        }
        let header = b.get(at..at + 4).ok_or(ProgramError::InvalidAccountData)?;
        let tag = u16::from_le_bytes([header[0], header[1]]);
        let len = u16::from_le_bytes([header[2], header[3]]) as usize;
        at += 4;
        let value = b
            .get(at..at + len)
            .ok_or(ProgramError::InvalidAccountData)?;
        if tag == 0 || out.iter().any(|(old, _)| *old == tag) {
            return Err(ProgramError::InvalidAccountData);
        }
        out.push((tag, value));
        at += len;
    }
    Ok(out)
}
fn metadata(b: &[u8], mint: &Pubkey) -> ProgramResult {
    if b.len() < 80 || &b[32..64] != mint.as_ref() {
        return Err(ProgramError::InvalidAccountData);
    }
    fn string(b: &[u8], at: &mut usize) -> ProgramResult {
        let len = u32_at(b, *at)? as usize;
        *at += 4;
        let end = at
            .checked_add(len)
            .ok_or(ProgramError::InvalidAccountData)?;
        core::str::from_utf8(b.get(*at..end).ok_or(ProgramError::InvalidAccountData)?)
            .map_err(|_| ProgramError::InvalidAccountData)?;
        *at = end;
        Ok(())
    }
    let mut at = 64;
    for _ in 0..3 {
        string(b, &mut at)?;
    }
    let count = u32_at(b, at)? as usize;
    at += 4;
    if count > (b.len() - at) / 8 {
        return Err(ProgramError::InvalidAccountData);
    }
    for _ in 0..count {
        string(b, &mut at)?;
        string(b, &mut at)?;
    }
    if at != b.len() {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}
fn mint_data(b: &[u8], mint: &Pubkey, program: &Pubkey) -> Result<u8, ProgramError> {
    let exts = extensions(b, 82, 1, program)?;
    coption(b, 0)?;
    coption(b, 46)?;
    if b[45] != 1 {
        return Err(ProgramError::UninitializedAccount);
    }
    for (tag, v) in exts {
        match tag {
            1 => {
                size(v, 108)?;
                for at in [88, 106] {
                    if u16::from_le_bytes([v[at], v[at + 1]]) > 10_000 {
                        return Err(ProgramError::InvalidAccountData);
                    }
                }
            }
            3 | 12 => size(v, 32)?, // mint close authority / permanent delegate
            6 => {
                size(v, 1)?;
                if v[0] != 1 {
                    return Err(err(CustodyError::UnsafeExtension));
                }
            }
            14 => {
                size(v, 64)?;
                if v[32..].iter().any(|x| *x != 0) {
                    return Err(err(CustodyError::UnsafeExtension));
                }
            }
            18 => size(v, 64)?,
            19 => metadata(v, mint)?,
            25 => {
                size(v, 56)?;
                for at in [32, 48] {
                    // IEEE-754 positive finite, nonzero. Avoid floating point in SBF.
                    let bits = u64_at(v, at)?;
                    if bits >> 63 != 0
                        || bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000
                        || bits == 0
                    {
                        return Err(ProgramError::InvalidAccountData);
                    }
                }
            }
            26 => {
                size(v, 33)?;
                if v[32] != 0 {
                    return Err(err(CustodyError::UnsafeExtension));
                }
            }
            _ => return Err(err(CustodyError::UnsupportedExtension)),
        }
    }
    Ok(b[44])
}
pub fn validate_mint(mint: &AccountInfo, program: &AccountInfo) -> Result<u8, ProgramError> {
    executable(program)?;
    if mint.owner != program.key {
        return Err(ProgramError::IllegalOwner);
    }
    mint_data(&mint.try_borrow_data()?, mint.key, program.key)
}
fn account_data(
    b: &[u8],
    mint: &Pubkey,
    user: &Pubkey,
    program: &Pubkey,
) -> Result<(u64, u64), ProgramError> {
    let exts = extensions(b, 165, 2, program)?;
    if &b[..32] != mint.as_ref() || &b[32..64] != user.as_ref() {
        return Err(ProgramError::InvalidAccountData);
    }
    if b[108] != 1 {
        return Err(ProgramError::UninitializedAccount);
    }
    if coption(b, 72)? != 0 || u64_at(b, 121)? != 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    coption(b, 109)?;
    if coption(b, 129)? == 1 && &b[133..165] != user.as_ref() {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut withheld = 0;
    for (tag, v) in exts {
        match tag {
            2 => {
                size(v, 8)?;
                withheld = u64_at(v, 0)?;
            }
            7 | 27 => size(v, 0)?,
            8 | 11 | 15 => {
                size(v, 1)?;
                if v[0] != 0 {
                    return Err(err(CustodyError::UnsafeExtension));
                }
            }
            _ => return Err(err(CustodyError::UnsupportedExtension)),
        }
    }
    Ok((u64_at(b, 64)?, withheld))
}
pub fn balance(
    account: &AccountInfo,
    mint: &Pubkey,
    user: &Pubkey,
    program: &Pubkey,
) -> Result<u64, ProgramError> {
    if account.owner != program {
        return Err(ProgramError::IllegalOwner);
    }
    Ok(account_data(&account.try_borrow_data()?, mint, user, program)?.0)
}
pub fn associated(user: &Pubkey, mint: &Pubkey, token: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[user.as_ref(), token.as_ref(), mint.as_ref()], &ATA).0
}
pub fn absent(account: &AccountInfo) -> ProgramResult {
    if account.lamports() != 0
        || !account.data_is_empty()
        || *account.owner != SYSTEM
        || account.executable
    {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    Ok(())
}
pub fn create_ata<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    user: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    ata_program: &AccountInfo<'a>,
) -> ProgramResult {
    validate_mint(mint, token_program)?;
    if *system.key != SYSTEM
        || !system.executable
        || *ata_program.key != ATA
        || !ata_program.executable
        || *account.key != associated(user.key, mint.key, token_program.key)
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    absent(account)?;
    let ix = Instruction {
        program_id: ATA,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*account.key, false),
            AccountMeta::new_readonly(*user.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new_readonly(*system.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: vec![0],
    };
    invoke(
        &ix,
        &[
            payer.clone(),
            account.clone(),
            user.clone(),
            mint.clone(),
            system.clone(),
            token_program.clone(),
            ata_program.clone(),
        ],
    )?;
    balance(account, mint.key, user.key, token_program.key)?;
    Ok(())
}
fn signed(ix: &Instruction, accounts: &[AccountInfo], seeds: &[&[u8]]) -> ProgramResult {
    if seeds.is_empty() {
        invoke(ix, accounts)
    } else {
        invoke_signed(ix, accounts, &[seeds])
    }
}
pub fn transfer<'a>(
    from: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    owner: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    amount: u64,
    decimals: u8,
    seeds: &[&[u8]],
) -> ProgramResult {
    if validate_mint(mint, token_program)? != decimals || from.key == to.key {
        return Err(ProgramError::InvalidArgument);
    }
    if balance(from, mint.key, owner.key, token_program.key)? < amount {
        return Err(ProgramError::InsufficientFunds);
    }
    // The recipient authority is bound by the caller's route. Validate its state here.
    {
        let data = to.try_borrow_data()?;
        let user = Pubkey::new_from_array(
            data.get(32..64)
                .ok_or(ProgramError::InvalidAccountData)?
                .try_into()
                .map_err(|_| ProgramError::InvalidAccountData)?,
        );
        balance(to, mint.key, &user, token_program.key)?;
    }
    let mut data = vec![12];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![
            AccountMeta::new(*from.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new(*to.key, false),
            AccountMeta::new_readonly(*owner.key, true),
        ],
        data,
    };
    signed(
        &ix,
        &[
            from.clone(),
            mint.clone(),
            to.clone(),
            owner.clone(),
            token_program.clone(),
        ],
        seeds,
    )
}
pub fn close_empty<'a>(
    account: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    recipient: &AccountInfo<'a>,
    owner: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    seeds: &[&[u8]],
) -> ProgramResult {
    validate_mint(mint, token_program)?;
    if account.key == recipient.key || account.owner != token_program.key {
        return Err(ProgramError::InvalidArgument);
    }
    let (amount, withheld) = account_data(
        &account.try_borrow_data()?,
        mint.key,
        owner.key,
        token_program.key,
    )?;
    if amount != 0 {
        return Err(err(CustodyError::NotEmpty));
    }
    if withheld != 0 {
        if !mint.is_writable {
            return Err(ProgramError::InvalidArgument);
        }
        let ix = Instruction {
            program_id: *token_program.key,
            accounts: vec![
                AccountMeta::new(*mint.key, false),
                AccountMeta::new(*account.key, false),
            ],
            data: vec![26, 4],
        };
        invoke(&ix, &[mint.clone(), account.clone(), token_program.clone()])?;
        if account_data(
            &account.try_borrow_data()?,
            mint.key,
            owner.key,
            token_program.key,
        )?
        .1 != 0
        {
            return Err(err(CustodyError::CloseFailed));
        }
    }
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![
            AccountMeta::new(*account.key, false),
            AccountMeta::new(*recipient.key, false),
            AccountMeta::new_readonly(*owner.key, true),
        ],
        data: vec![9],
    };
    signed(
        &ix,
        &[
            account.clone(),
            recipient.clone(),
            owner.clone(),
            token_program.clone(),
        ],
        seeds,
    )?;
    if account.lamports() != 0 {
        return Err(err(CustodyError::CloseFailed));
    }
    Ok(())
}
pub fn sync_native<'a>(
    account: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
) -> ProgramResult {
    executable(token_program)?;
    if account.owner != token_program.key {
        return Err(ProgramError::IllegalOwner);
    }
    invoke(
        &Instruction {
            program_id: *token_program.key,
            accounts: vec![AccountMeta::new(*account.key, false)],
            data: vec![17],
        },
        &[account.clone(), token_program.clone()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mint() -> Vec<u8> {
        let mut b = vec![0; 82];
        b[44] = 9;
        b[45] = 1;
        b
    }
    fn ext(b: &mut Vec<u8>, kind: u8, tag: u16, value: &[u8]) {
        if b.len() < 166 {
            b.resize(166, 0);
            b[165] = kind;
        }
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&(value.len() as u16).to_le_bytes());
        b.extend_from_slice(value);
    }
    #[test]
    fn scaled_permanent_delegate_pausable_fee_are_raw_atoms() {
        let key = Pubkey::new_unique();
        let mut b = mint();
        let mut scaled = vec![0; 56];
        scaled[32..40].copy_from_slice(&2f64.to_le_bytes());
        scaled[48..56].copy_from_slice(&3f64.to_le_bytes());
        ext(&mut b, 1, 25, &scaled);
        ext(&mut b, 1, 12, &[7; 32]);
        ext(&mut b, 1, 26, &[0; 33]);
        ext(&mut b, 1, 1, &[0; 108]);
        assert_eq!(mint_data(&b, &key, &TOKEN22), Ok(9));
        assert!(mint_data(&b, &key, &SPL).is_err());
        let user = Pubkey::new_unique();
        let mut a = vec![0; 165];
        a[..32].copy_from_slice(key.as_ref());
        a[32..64].copy_from_slice(user.as_ref());
        a[64..72].copy_from_slice(&123u64.to_le_bytes());
        a[108] = 1;
        ext(&mut a, 2, 2, &17u64.to_le_bytes());
        ext(&mut a, 2, 27, &[]);
        assert_eq!(account_data(&a, &key, &user, &TOKEN22), Ok((123, 17)));
        a[72] = 1;
        assert!(account_data(&a, &key, &user, &TOKEN22).is_err());
    }
    #[test]
    fn rejects_paused_hook_confidential_unknown_duplicate_and_truncated() {
        let key = Pubkey::new_unique();
        for (tag, value) in [
            (26, vec![1; 33]),
            (14, vec![1; 64]),
            (4, vec![0; 65]),
            (9, vec![]),
            (65535, vec![]),
        ] {
            let mut b = mint();
            ext(&mut b, 1, tag, &value);
            assert!(mint_data(&b, &key, &TOKEN22).is_err());
        }
        let mut b = mint();
        ext(&mut b, 1, 12, &[0; 32]);
        ext(&mut b, 1, 12, &[0; 32]);
        assert!(mint_data(&b, &key, &TOKEN22).is_err());
        b.truncate(b.len() - 1);
        assert!(mint_data(&b, &key, &TOKEN22).is_err());
    }
    #[test]
    fn rejects_invalid_scaled_bits_fee_and_frozen_accounts() {
        let key = Pubkey::new_unique();
        for bits in [
            0,
            (-1f64).to_bits(),
            f64::INFINITY.to_bits(),
            f64::NAN.to_bits(),
        ] {
            let mut b = mint();
            let mut v = [0; 56];
            v[32..40].copy_from_slice(&bits.to_le_bytes());
            v[48..56].copy_from_slice(&1f64.to_le_bytes());
            ext(&mut b, 1, 25, &v);
            assert!(mint_data(&b, &key, &TOKEN22).is_err());
        }
        let mut b = mint();
        let mut fee = [0; 108];
        fee[106..108].copy_from_slice(&10001u16.to_le_bytes());
        ext(&mut b, 1, 1, &fee);
        assert!(mint_data(&b, &key, &TOKEN22).is_err());
        let user = Pubkey::new_unique();
        let mut a = vec![0; 165];
        a[..32].copy_from_slice(key.as_ref());
        a[32..64].copy_from_slice(user.as_ref());
        a[108] = 2;
        assert!(account_data(&a, &key, &user, &SPL).is_err());
    }
}
