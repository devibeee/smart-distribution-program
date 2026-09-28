//! Atomic ephemeral two-vault curve route. No persistent replay tombstone exists:
//! callers must journal issued nonces. Venue 0 only; PumpSwap is not supported.
use super::*;
use solana_program::program::invoke;
const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const FEE: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
pub const TAG: u8 = 23;
fn signed(ix: &Instruction, accounts: &[AccountInfo], seeds: &[&[u8]]) -> ProgramResult {
    if seeds.is_empty() {
        invoke(ix, accounts)
    } else {
        invoke_signed(ix, accounts, &[seeds])
    }
}
#[derive(Debug, PartialEq)]
struct Args {
    nonce: [u8; 32],
    expiry: u64,
    amount: u64,
    min_sell: u64,
    min_buy: u64,
    max_buy: u64,
    seed: u64,
}
fn parse(data: &[u8]) -> Result<Args, ProgramError> {
    if data.len() != 83 || data[..3] != [TAG, 1, 0] {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut nonce = [0; 32];
    nonce.copy_from_slice(&data[3..35]);
    let args = Args {
        nonce,
        expiry: read_u64_le(data, 35)?,
        amount: read_u64_le(data, 43)?,
        min_sell: read_u64_le(data, 51)?,
        min_buy: read_u64_le(data, 59)?,
        max_buy: read_u64_le(data, 67)?,
        seed: read_u64_le(data, 75)?,
    };
    if nonce == [0; 32]
        || [
            args.expiry,
            args.amount,
            args.min_sell,
            args.min_buy,
            args.max_buy,
            args.seed,
        ]
        .contains(&0)
    {
        return Err(SettlementError::InvalidInstruction.into());
    }
    Ok(args)
}
fn absent(a: &AccountInfo) -> ProgramResult {
    if a.owner != &solana_system_interface::program::id() || a.lamports() != 0 || !a.data_is_empty()
    {
        return Err(SettlementError::InvalidStatus.into());
    }
    Ok(())
}
fn ata(user: &Pubkey, mint: &Pubkey, token: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[user.as_ref(), token.as_ref(), mint.as_ref()], &ATA).0
}
fn token(
    a: &AccountInfo,
    mint: &Pubkey,
    user: &Pubkey,
    program: &Pubkey,
) -> Result<u64, ProgramError> {
    require_key(a.owner, program)?;
    let d = a.try_borrow_data()?;
    if d.len() < 165
        || d[..32] != mint.to_bytes()
        || d[32..64] != user.to_bytes()
        || d[108] != 1
        || d[72..76] != [0; 4]
        || d[129..133] != [0; 4]
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    read_u64_le(&d, 64)
}
fn mint_ok(d: &[u8], program: &Pubkey) -> ProgramResult {
    if ![TOKEN, TOKEN22].contains(program) || d.len() < 82 || d[45] != 1 {
        return Err(SettlementError::InvalidRoute.into());
    }
    if d.len() == 82 {
        return Ok(());
    }
    if program != &TOKEN22
        || d.len() < 166
        || d.len() > 16384
        || d.len() == 355
        || d[165] != 1
        || d[82..165].iter().any(|b| *b != 0)
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    let mut p = 166;
    let mut seen = Vec::new();
    while p < d.len() {
        if d[p..].iter().all(|b| *b == 0) {
            break;
        }
        if p + 4 > d.len() {
            return Err(SettlementError::InvalidRoute.into());
        }
        let kind = u16::from_le_bytes([d[p], d[p + 1]]);
        let n = u16::from_le_bytes([d[p + 2], d[p + 3]]) as usize;
        p += 4;
        if p + n > d.len()
            || seen.contains(&kind)
            || !match kind {
                3 => n == 32,
                18 => n == 64,
                19 => n >= 80,
                _ => false,
            }
        {
            return Err(SettlementError::InvalidRoute.into());
        }
        seen.push(kind);
        p += n;
    }
    Ok(())
}
fn create_ata<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    user: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    token: &AccountInfo<'a>,
    program: &AccountInfo<'a>,
) -> ProgramResult {
    let ix = Instruction {
        program_id: ATA,
        data: vec![0],
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*account.key, false),
            AccountMeta::new_readonly(*user.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new_readonly(*system.key, false),
            AccountMeta::new_readonly(*token.key, false),
        ],
    };
    invoke(
        &ix,
        &[
            payer.clone(),
            account.clone(),
            user.clone(),
            mint.clone(),
            system.clone(),
            token.clone(),
            program.clone(),
        ],
    )
}
fn transfer<'a>(
    from: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    owner: &AccountInfo<'a>,
    program: &AccountInfo<'a>,
    amount: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    let mut data = vec![3];
    data.extend_from_slice(&amount.to_le_bytes());
    signed(
        &Instruction {
            program_id: *program.key,
            data,
            accounts: vec![
                AccountMeta::new(*from.key, false),
                AccountMeta::new(*to.key, false),
                AccountMeta::new_readonly(*owner.key, true),
            ],
        },
        &[from.clone(), to.clone(), owner.clone(), program.clone()],
        seeds,
    )
}
fn sol<'a>(
    from: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    amount: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    signed(
        &system_instruction::transfer(from.key, to.key, amount),
        &[from.clone(), to.clone(), system.clone()],
        seeds,
    )
}
fn close_quote<'a>(
    a: &AccountInfo<'a>,
    user: &AccountInfo<'a>,
    program: &AccountInfo<'a>,
    seeds: &[&[u8]],
) -> ProgramResult {
    if a.lamports() == 0 {
        return absent(a);
    }
    token(a, &WSOL, user.key, &TOKEN)?;
    signed(
        &Instruction {
            program_id: *program.key,
            data: vec![9],
            accounts: vec![
                AccountMeta::new(*a.key, false),
                AccountMeta::new(*user.key, false),
                AccountMeta::new_readonly(*user.key, true),
            ],
        },
        &[a.clone(), user.clone(), program.clone()],
        seeds,
    )
}
fn close_volume<'a>(
    user: &AccountInfo<'a>,
    volume: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    event: &AccountInfo<'a>,
    pump: &AccountInfo<'a>,
    seeds: &[&[u8]],
) -> ProgramResult {
    if volume.lamports() == 0 {
        return absent(volume);
    }
    require_key(volume.owner, &PUMP_PROGRAM_ID)?;
    let accounts = vec![
        AccountMeta::new(*user.key, false),
        AccountMeta::new(*volume.key, false),
        AccountMeta::new_readonly(*system.key, false),
        AccountMeta::new_readonly(*event.key, false),
        AccountMeta::new_readonly(*pump.key, false),
    ];
    signed(
        &Instruction {
            program_id: PUMP_PROGRAM_ID,
            accounts,
            data: vec![37, 58, 35, 126, 190, 53, 228, 197],
        },
        &[
            user.clone(),
            volume.clone(),
            system.clone(),
            event.clone(),
            pump.clone(),
        ],
        seeds,
    )?;
    signed(
        &Instruction {
            program_id: PUMP_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*user.key, true),
                AccountMeta::new(*volume.key, false),
                AccountMeta::new_readonly(*event.key, false),
                AccountMeta::new_readonly(*pump.key, false),
            ],
            data: vec![249, 69, 164, 218, 150, 103, 84, 138],
        },
        &[user.clone(), volume.clone(), event.clone(), pump.clone()],
        seeds,
    )?;
    if volume.lamports() != 0 {
        return Err(SettlementError::CleanupMismatch.into());
    }
    Ok(())
}
pub fn process(program_id: &Pubkey, a: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let args = parse(data)?;
    if a.len() != 66 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let (source, buyer, mint, source_token, va, ta, vb, tb, buyer_token, system, tp, ap, clock) = (
        &a[0], &a[1], &a[2], &a[3], &a[4], &a[5], &a[6], &a[7], &a[8], &a[9], &a[10], &a[11],
        &a[12],
    );
    require_signer(source)?;
    require_signer(buyer)?;
    require_key(system.key, &solana_system_interface::program::id())?;
    require_key(ap.key, &ATA)?;
    if !system.executable || !tp.executable || !ap.executable {
        return Err(ProgramError::IncorrectProgramId);
    }
    for user in [source, buyer] {
        require_key(user.owner, system.key)?;
        if !user.data_is_empty() {
            return Err(SettlementError::InvalidRoute.into());
        }
    }
    for i in 0..9 {
        for j in i + 1..9 {
            if a[i].key == a[j].key {
                return Err(SettlementError::AccountMismatch.into());
            }
        }
    }
    if Clock::from_account_info(clock)?.slot > args.expiry {
        return Err(SettlementError::RouteExpired.into());
    }
    require_key(mint.owner, tp.key)?;
    mint_ok(&mint.try_borrow_data()?, tp.key)?;
    let source_before = token(source_token, mint.key, source.key, tp.key)?;
    if source_before < args.amount {
        return Err(ProgramError::InsufficientFunds);
    }
    for (account, user) in [(ta, va), (tb, vb), (buyer_token, buyer)] {
        require_key(account.key, &ata(user.key, mint.key, tp.key))?;
    }
    for account in [va, ta, vb, tb] {
        absent(account)?;
    }
    let (ka, ba) = Pubkey::find_program_address(
        &[
            b"two-vault-a",
            source.key.as_ref(),
            buyer.key.as_ref(),
            mint.key.as_ref(),
            &args.nonce,
        ],
        program_id,
    );
    let (kb, bb) = Pubkey::find_program_address(
        &[
            b"two-vault-b",
            source.key.as_ref(),
            buyer.key.as_ref(),
            mint.key.as_ref(),
            &args.nonce,
        ],
        program_id,
    );
    require_key(va.key, &ka)?;
    require_key(vb.key, &kb)?;
    let ab = [ba];
    let bbum = [bb];
    let sa: &[&[u8]] = &[
        b"two-vault-a",
        source.key.as_ref(),
        buyer.key.as_ref(),
        mint.key.as_ref(),
        &args.nonce,
        &ab,
    ];
    let sb: &[&[u8]] = &[
        b"two-vault-b",
        source.key.as_ref(),
        buyer.key.as_ref(),
        mint.key.as_ref(),
        &args.nonce,
        &bbum,
    ];
    let sell = &a[13..39];
    let buy = &a[39..66];
    for index in (0..13).chain(16..19) {
        require_key(sell[index].key, buy[index].key)?;
    }
    require_key(sell[21].key, buy[22].key)?;
    require_key(sell[22].key, buy[23].key)?;
    super::require_pump_sell_accounts_v2(sell, vb.key, mint.key)?;
    super::require_pump_buy_accounts_v2(buy, buyer.key, mint.key)?;
    for (p, user, base, vi, sysi, ei, pi) in [
        (sell, vb, tb, 19, 23, 24, 25),
        (buy, buyer, buyer_token, 20, 24, 25, 26),
    ] {
        for (idx, key) in [
            (2, &WSOL),
            (3, tp.key),
            (4, &TOKEN),
            (5, &ATA),
            (14, base.key),
            (sysi, system.key),
        ] {
            require_key(p[idx].key, key)?;
        }
        require_key(p[15].key, &ata(user.key, &WSOL, &TOKEN))?;
        absent(&p[15])?;
        require_key(
            p[vi].key,
            &Pubkey::find_program_address(
                &[b"user_volume_accumulator", user.key.as_ref()],
                &PUMP_PROGRAM_ID,
            )
            .0,
        )?;
        require_key(p[vi + 1].key, &ata(p[vi].key, &WSOL, &TOKEN))?;
        absent(&p[vi + 1])?;
        require_key(
            p[ei].key,
            &Pubkey::find_program_address(&[b"__event_authority"], &PUMP_PROGRAM_ID).0,
        )?;
        if !p[pi].executable {
            return Err(ProgramError::IncorrectProgramId);
        }
        require_key(p[0].owner, &PUMP_PROGRAM_ID)?;
        require_key(
            p[0].key,
            &Pubkey::find_program_address(&[b"global"], &PUMP_PROGRAM_ID).0,
        )?;
        require_key(p[vi + 3].key, &FEE)?;
        require_key(
            p[vi + 2].key,
            &Pubkey::find_program_address(&[b"fee_config", PUMP_PROGRAM_ID.as_ref()], &FEE).0,
        )?;
        if !p[vi + 2].data_is_empty() {
            require_key(p[vi + 2].owner, &FEE)?;
        }
        require_key(
            p[18].key,
            &Pubkey::find_program_address(&[b"sharing-config", mint.key.as_ref()], &FEE).0,
        )?;
        require_key(p[10].owner, &PUMP_PROGRAM_ID)?;
        require_key(
            p[10].key,
            &Pubkey::find_program_address(&[b"bonding-curve", mint.key.as_ref()], &PUMP_PROGRAM_ID)
                .0,
        )?;
    }
    let global = sell[0].try_borrow_data()?;
    let curve = sell[10].try_borrow_data()?;
    if global.get(..8) != Some(&[167, 232, 232, 177, 200, 108, 114, 127])
        || curve.get(..8) != Some(&[23, 183, 248, 55, 96, 216, 172, 96])
        || curve.len() < 83
        || curve[48] != 0
        || (curve.len() >= 115 && curve[83..115] != [0; 32] && curve[83..115] != WSOL.to_bytes())
    {
        return Err(SettlementError::InvalidRoute.into());
    }
    drop(global);
    drop(curve);
    absent(&sell[19])?;
    let new_buyer_volume = buy[20].lamports() == 0;
    if new_buyer_volume {
        absent(&buy[20])?;
    } else {
        require_key(buy[20].owner, &PUMP_PROGRAM_ID)?;
    }
    create_ata(source, ta, va, mint, system, tp, ap)?;
    create_ata(source, tb, vb, mint, system, tp, ap)?;
    let buyer_before = if buyer_token.lamports() == 0 {
        absent(buyer_token)?;
        create_ata(source, buyer_token, buyer, mint, system, tp, ap)?;
        0
    } else {
        token(buyer_token, mint.key, buyer.key, tp.key)?
    };
    sol(source, va, system, args.seed, &[])?;
    transfer(source_token, ta, source, tp, args.amount, &[])?;
    if token(ta, mint.key, va.key, tp.key)? != args.amount
        || token(source_token, mint.key, source.key, tp.key)? != source_before - args.amount
    {
        return Err(SettlementError::BaselineMismatch.into());
    }
    sol(va, vb, system, args.seed, sa)?;
    transfer(ta, tb, va, tp, args.amount, sa)?;
    if token(ta, mint.key, va.key, tp.key)? != 0
        || token(tb, mint.key, vb.key, tp.key)? != args.amount
    {
        return Err(SettlementError::BaselineMismatch.into());
    }
    let b_before = vb.lamports();
    invoke_signed(
        &pump_sell_v2_instruction(sell, args.amount, args.min_sell)?,
        sell,
        &[sb],
    )?;
    if token(tb, mint.key, vb.key, tp.key)? != 0 {
        return Err(SettlementError::CustodyNotEmpty.into());
    }
    close_quote(&sell[15], vb, &sell[4], sb)?;
    close_volume(vb, &sell[19], system, &sell[24], &sell[25], sb)?;
    absent(&sell[20])?;
    let proceeds = vb
        .lamports()
        .checked_sub(b_before)
        .ok_or(SettlementError::BaselineMismatch)?;
    if proceeds < args.min_sell {
        return Err(SettlementError::SlippageExceeded.into());
    }
    let funding = proceeds.min(args.max_buy);
    let baseline = buyer.lamports();
    sol(vb, buyer, system, funding, sb)?;
    let quote = dynamic_buy_quote(
        &buy[10].try_borrow_data()?,
        &buy[0].try_borrow_data()?,
        &buy[22].try_borrow_data()?,
        funding,
    )?;
    if quote.amount_raw < args.min_buy {
        return Err(SettlementError::SlippageExceeded.into());
    }
    invoke(
        &pump_buy_v2_instruction(buy, quote.amount_raw, funding)?,
        buy,
    )?;
    if token(buyer_token, mint.key, buyer.key, tp.key)?.checked_sub(buyer_before)
        != Some(quote.amount_raw)
        || buyer.lamports() < baseline
    {
        return Err(SettlementError::BaselineMismatch.into());
    }
    close_quote(&buy[15], buyer, &buy[4], &[])?;
    if new_buyer_volume {
        close_volume(buyer, &buy[20], system, &buy[25], &buy[26], &[])?;
    }
    absent(&buy[21])?;
    let residual = buyer
        .lamports()
        .checked_sub(baseline)
        .ok_or(SettlementError::BaselineMismatch)?;
    sol(buyer, source, system, residual, &[])?;
    for (vault, tokens, seeds) in [(va, ta, sa), (vb, tb, sb)] {
        super::close_token_account_via_pda(tokens, source, vault, tp, seeds)?;
        invoke_signed(
            &system_instruction::assign(vault.key, program_id),
            &[vault.clone(), system.clone()],
            &[seeds],
        )?;
        transfer_from_program_owned(vault, source, vault.lamports())?;
        if vault.lamports() != 0 || tokens.lamports() != 0 {
            return Err(SettlementError::CleanupMismatch.into());
        }
    }
    if buyer.lamports() != baseline {
        return Err(SettlementError::BaselineMismatch.into());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn data() -> Vec<u8> {
        let mut d = vec![23, 1, 0];
        d.extend_from_slice(&[1; 32]);
        for n in [100u64, 20, 3, 4, 50, 1000] {
            d.extend_from_slice(&n.to_le_bytes());
        }
        d
    }
    #[test]
    fn strict_wire_and_positive_bounds() {
        let d = data();
        assert_eq!(parse(&d).unwrap().amount, 20);
        for n in 0..d.len() {
            assert!(parse(&d[..n]).is_err());
        }
        let mut extra = d.clone();
        extra.push(0);
        assert!(parse(&extra).is_err());
        for offset in [35, 43, 51, 59, 67, 75] {
            let mut bad = d.clone();
            bad[offset..offset + 8].fill(0);
            assert!(parse(&bad).is_err());
        }
        for (offset, value) in [(0, 22), (1, 2), (2, 1)] {
            let mut bad = d.clone();
            bad[offset] = value;
            assert!(parse(&bad).is_err());
        }
    }
    #[test]
    fn mint_extensions_fail_closed() {
        let mut d = vec![0; 82];
        d[45] = 1;
        assert!(mint_ok(&d, &TOKEN).is_ok());
        d.resize(170, 0);
        d[165] = 1;
        d[166..168].copy_from_slice(&1u16.to_le_bytes());
        assert!(mint_ok(&d, &TOKEN22).is_err());
        assert!(mint_ok(&d, &TOKEN).is_err());
    }
    #[test]
    fn rejects_prefunded_or_allocated_temporary_accounts() {
        let key = Pubkey::new_unique();
        let owner = solana_system_interface::program::id();
        let mut balance = 1;
        let mut empty = [];
        let account = AccountInfo::new(&key, false, true, &mut balance, &mut empty, &owner, false);
        assert!(absent(&account).is_err());
        **account.try_borrow_mut_lamports().unwrap() = 0;
        assert!(absent(&account).is_ok());
        let mut allocated = [0];
        let mut zero = 0;
        let account = AccountInfo::new(&key, false, true, &mut zero, &mut allocated, &owner, false);
        assert!(absent(&account).is_err());
    }
    #[test]
    fn token_identity_state_and_authorities_are_bound() {
        let key = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let user = Pubkey::new_unique();
        let mut balance = 1;
        let mut bytes = [0u8; 165];
        bytes[..32].copy_from_slice(mint.as_ref());
        bytes[32..64].copy_from_slice(user.as_ref());
        bytes[64..72].copy_from_slice(&42u64.to_le_bytes());
        bytes[108] = 1;
        let account = AccountInfo::new(&key, false, true, &mut balance, &mut bytes, &TOKEN, false);
        assert_eq!(token(&account, &mint, &user, &TOKEN).unwrap(), 42);
        assert!(token(&account, &Pubkey::new_unique(), &user, &TOKEN).is_err());
        assert!(token(&account, &mint, &Pubkey::new_unique(), &TOKEN).is_err());
        assert!(token(&account, &mint, &user, &TOKEN22).is_err());
        for offset in [72, 129] {
            account.try_borrow_mut_data().unwrap()[offset] = 1;
            assert!(token(&account, &mint, &user, &TOKEN).is_err());
            account.try_borrow_mut_data().unwrap()[offset] = 0;
        }
        account.try_borrow_mut_data().unwrap()[108] = 2;
        assert!(token(&account, &mint, &user, &TOKEN).is_err());
    }
    #[test]
    fn nonce_and_participant_bind_every_vault() {
        let p = Pubkey::new_unique();
        let s = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let m = Pubkey::new_unique();
        let derive = |seed: &[u8], nonce: &[u8]| {
            Pubkey::find_program_address(&[seed, s.as_ref(), b.as_ref(), m.as_ref(), nonce], &p).0
        };
        assert_ne!(
            derive(b"two-vault-a", &[1; 32]),
            derive(b"two-vault-b", &[1; 32])
        );
        assert_ne!(
            derive(b"two-vault-a", &[1; 32]),
            derive(b"two-vault-a", &[2; 32])
        );
    }
}
