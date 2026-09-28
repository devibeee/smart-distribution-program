//! Atomic multi-quote custody. Venue CPIs use exact input; final net deltas are
//! the authority for slippage. A closed route has no persistent nonce history.
use super::*;
use solana_program::program::invoke;

mod clmm;
mod cpmm;
mod launchlab;
mod pump_amm;
mod pump_curve;
mod token;

const TAG: u8 = 24;
const PREFIX: usize = 18;
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const SPL: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

#[derive(Debug)]
struct Args {
    venue: u8,
    native: bool,
    nonce: [u8; 32],
    expiry: u64,
    amount: u64,
    min_sell: u64,
    min_buy: u64,
    max_buy: u64,
    seed: u64,
    buyer_rent: u64,
    sell_count: usize,
    buy_count: usize,
}

fn parse(d: &[u8]) -> Result<Args, ProgramError> {
    if d.len() != 94 || d[0] != TAG || d[1] != 1 || d[2] > 4 || d[3] > 1 {
        return Err(SettlementError::InvalidInstruction.into());
    }
    let mut nonce = [0; 32];
    nonce.copy_from_slice(&d[4..36]);
    let a = Args {
        venue: d[2],
        native: d[3] == 1,
        nonce,
        expiry: read_u64_le(d, 36)?,
        amount: read_u64_le(d, 44)?,
        min_sell: read_u64_le(d, 52)?,
        min_buy: read_u64_le(d, 60)?,
        max_buy: read_u64_le(d, 68)?,
        seed: read_u64_le(d, 76)?,
        buyer_rent: read_u64_le(d, 84)?,
        sell_count: d[92] as usize,
        buy_count: d[93] as usize,
    };
    if a.nonce == [0; 32]
        || [a.expiry, a.amount, a.min_sell, a.min_buy, a.max_buy, a.seed].contains(&0)
        || a.buyer_rent > a.seed
        || a.sell_count == 0
        || a.buy_count == 0
        || (a.native && a.venue != 0)
    {
        return Err(SettlementError::InvalidInstruction.into());
    }
    Ok(a)
}

fn validate_leg(
    a: &[AccountInfo],
    venue: u8,
    user: &Pubkey,
    base: &Pubkey,
    quote: &Pubkey,
    btp: &Pubkey,
    qtp: &Pubkey,
    buy: bool,
) -> ProgramResult {
    match venue {
        0 => pump_curve::validate(a, user, base, quote, btp, qtp, buy),
        1 => pump_amm::validate(a, user, base, quote, btp, qtp, buy),
        2 => launchlab::validate(a, user, base, quote, btp, qtp, buy),
        3 => cpmm::validate(a, user, base, quote, btp, qtp, buy),
        4 => clmm::validate(a, user, base, quote, btp, qtp, buy),
        _ => Err(SettlementError::InvalidRoute.into()),
    }
}
fn swap<'a>(
    a: &[AccountInfo<'a>],
    venue: u8,
    buy: bool,
    amount: u64,
    minimum: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    match venue {
        0 => pump_curve::swap(a, buy, amount, minimum, seeds),
        1 => pump_amm::swap(a, buy, amount, minimum, seeds),
        2 => launchlab::swap(a, buy, amount, minimum, seeds),
        3 => cpmm::swap(a, buy, amount, minimum, seeds),
        4 => clmm::swap(a, buy, amount, minimum, seeds),
        _ => Err(SettlementError::InvalidRoute.into()),
    }
}
fn volume_exists(a: &[AccountInfo], venue: u8, buy: bool) -> Result<bool, ProgramError> {
    match venue {
        0 => pump_curve::volume_exists(a, buy),
        1 => pump_amm::volume_exists(a, buy),
        _ => Ok(false),
    }
}
fn cleanup<'a>(
    a: &[AccountInfo<'a>],
    args: &Args,
    buy: bool,
    existed: bool,
    seeds: &[&[u8]],
) -> ProgramResult {
    match args.venue {
        0 => pump_curve::cleanup(a, buy, existed, args.native, seeds),
        1 => pump_amm::cleanup(a, buy, existed, false, seeds),
        _ => Ok(()),
    }
}
fn signed(ix: &Instruction, infos: &[AccountInfo], seeds: &[&[u8]]) -> ProgramResult {
    if seeds.is_empty() {
        invoke(ix, infos)
    } else {
        invoke_signed(ix, infos, &[seeds])
    }
}
fn sol<'a>(
    from: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    system: &AccountInfo<'a>,
    amount: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    signed(
        &system_instruction::transfer(from.key, to.key, amount),
        &[from.clone(), to.clone(), system.clone()],
        seeds,
    )
}
fn unwrap<'a>(
    account: &AccountInfo<'a>,
    recipient: &AccountInfo<'a>,
    owner: &AccountInfo<'a>,
    program: &AccountInfo<'a>,
    seeds: &[&[u8]],
) -> ProgramResult {
    if account.lamports() == 0 {
        return Ok(());
    }
    require_key(program.key, &SPL)?;
    token::balance(account, &WSOL, owner.key, &SPL)?;
    let d = account.try_borrow_data()?;
    if d.get(109..113) != Some(&[1, 0, 0, 0]) {
        return Err(SettlementError::InvalidRoute.into());
    }
    drop(d);
    signed(
        &Instruction {
            program_id: SPL,
            data: vec![9],
            accounts: vec![
                AccountMeta::new(*account.key, false),
                AccountMeta::new(*recipient.key, false),
                AccountMeta::new_readonly(*owner.key, true),
            ],
        },
        &[
            account.clone(),
            recipient.clone(),
            owner.clone(),
            program.clone(),
        ],
        seeds,
    )?;
    if account.lamports() != 0 {
        return Err(SettlementError::CleanupMismatch.into());
    }
    Ok(())
}

pub fn process(program_id: &Pubkey, a: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let args = parse(data)?;
    if a.len() != PREFIX + args.sell_count + args.buy_count {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let (
        source,
        buyer,
        base,
        quote,
        source_base,
        source_quote,
        va,
        ta,
        vb,
        tb,
        bq,
        buyer_base,
        buyer_quote,
        system,
        btp,
        qtp,
        ata_program,
        clock,
    ) = (
        &a[0], &a[1], &a[2], &a[3], &a[4], &a[5], &a[6], &a[7], &a[8], &a[9], &a[10], &a[11],
        &a[12], &a[13], &a[14], &a[15], &a[16], &a[17],
    );
    require_signer(source)?;
    require_signer(buyer)?;
    require_key(system.key, &solana_system_interface::program::id())?;
    require_key(ata_program.key, &ATA)?;
    if !system.executable || !ata_program.executable {
        return Err(ProgramError::IncorrectProgramId);
    }
    for user in [source, buyer] {
        require_key(user.owner, system.key)?;
        if !user.data_is_empty() {
            return Err(SettlementError::InvalidRoute.into());
        }
    }
    for i in 0..13 {
        if !a[i].is_writable {
            return Err(ProgramError::InvalidAccountData);
        }
        for j in i + 1..13 {
            if a[i].key == a[j].key {
                return Err(SettlementError::AccountMismatch.into());
            }
        }
    }
    if Clock::from_account_info(clock)?.slot > args.expiry {
        return Err(SettlementError::RouteExpired.into());
    }
    if args.native && (*quote.key != WSOL || *qtp.key != SPL) {
        return Err(SettlementError::InvalidRoute.into());
    }
    // Pump's WSOL mint identifies the native-lamport branch. Other venues use SPL WSOL.
    if args.venue == 0 && (*quote.key == WSOL) != args.native {
        return Err(SettlementError::InvalidRoute.into());
    }
    let bd = token::validate_mint(base, btp)?;
    let qd = token::validate_mint(quote, qtp)?;
    for (account, owner, mint, program) in [
        (source_base, source, base, btp),
        (source_quote, source, quote, qtp),
        (ta, va, base, btp),
        (tb, vb, base, btp),
        (bq, vb, quote, qtp),
        (buyer_base, buyer, base, btp),
        (buyer_quote, buyer, quote, qtp),
    ] {
        require_key(
            account.key,
            &token::associated(owner.key, mint.key, program.key),
        )?;
    }
    let (ka, bump_a) = Pubkey::find_program_address(
        &[
            b"two-vault-v2-a",
            source.key.as_ref(),
            buyer.key.as_ref(),
            base.key.as_ref(),
            quote.key.as_ref(),
            &args.nonce,
        ],
        program_id,
    );
    let (kb, bump_b) = Pubkey::find_program_address(
        &[
            b"two-vault-v2-b",
            source.key.as_ref(),
            buyer.key.as_ref(),
            base.key.as_ref(),
            quote.key.as_ref(),
            &args.nonce,
        ],
        program_id,
    );
    require_key(va.key, &ka)?;
    require_key(vb.key, &kb)?;
    let ab = [bump_a];
    let bb = [bump_b];
    let sa: &[&[u8]] = &[
        b"two-vault-v2-a",
        source.key.as_ref(),
        buyer.key.as_ref(),
        base.key.as_ref(),
        quote.key.as_ref(),
        &args.nonce,
        &ab,
    ];
    let sb: &[&[u8]] = &[
        b"two-vault-v2-b",
        source.key.as_ref(),
        buyer.key.as_ref(),
        base.key.as_ref(),
        quote.key.as_ref(),
        &args.nonce,
        &bb,
    ];
    for account in [va, ta, vb, tb, bq] {
        token::absent(account)?;
    }
    let source_before = token::balance(source_base, base.key, source.key, btp.key)?;
    if source_before < args.amount {
        return Err(ProgramError::InsufficientFunds);
    }
    let sell = &a[PREFIX..PREFIX + args.sell_count];
    let buy = &a[PREFIX + args.sell_count..];
    validate_leg(
        sell, args.venue, vb.key, base.key, quote.key, btp.key, qtp.key, false,
    )?;
    validate_leg(
        buy, args.venue, buyer.key, base.key, quote.key, btp.key, qtp.key, true,
    )?;
    let pool_index = [10, 0, 4, 3, 2][args.venue as usize];
    require_key(sell[pool_index].key, buy[pool_index].key)?;
    if volume_exists(sell, args.venue, false)? {
        return Err(SettlementError::InvalidStatus.into());
    }
    let buyer_volume_existed = volume_exists(buy, args.venue, true)?;
    let buyer_sol_before = buyer.lamports();
    let buyer_base_before = if buyer_base.lamports() == 0 {
        token::create_ata(source, buyer_base, buyer, base, system, btp, ata_program)?;
        0
    } else {
        token::balance(buyer_base, base.key, buyer.key, btp.key)?
    };
    token::create_ata(source, ta, va, base, system, btp, ata_program)?;
    token::create_ata(source, tb, vb, base, system, btp, ata_program)?;
    let source_quote_new = !args.native && source_quote.lamports() == 0;
    let buyer_quote_new = buyer_quote.lamports() == 0;
    let buyer_quote_before = if args.native {
        token::absent(buyer_quote)?;
        0
    } else {
        token::create_ata(source, bq, vb, quote, system, qtp, ata_program)?;
        if source_quote_new {
            token::create_ata(
                source,
                source_quote,
                source,
                quote,
                system,
                qtp,
                ata_program,
            )?;
        }
        token::balance(source_quote, quote.key, source.key, qtp.key)?;
        if buyer_quote_new {
            token::create_ata(source, buyer_quote, buyer, quote, system, qtp, ata_program)?;
        }
        token::balance(buyer_quote, quote.key, buyer.key, qtp.key)?
    };
    sol(source, va, system, args.seed, &[])?;
    token::transfer(source_base, base, ta, source, btp, args.amount, bd, &[])?;
    if token::balance(source_base, base.key, source.key, btp.key)? != source_before - args.amount {
        return Err(SettlementError::BaselineMismatch.into());
    }
    let a_amount = token::balance(ta, base.key, va.key, btp.key)?;
    if a_amount == 0 || a_amount > args.amount {
        return Err(SettlementError::BaselineMismatch.into());
    }
    sol(va, vb, system, args.seed, sa)?;
    token::transfer(ta, base, tb, va, btp, a_amount, bd, sa)?;
    let b_amount = token::balance(tb, base.key, vb.key, btp.key)?;
    if token::balance(ta, base.key, va.key, btp.key)? != 0 || b_amount == 0 || b_amount > a_amount {
        return Err(SettlementError::BaselineMismatch.into());
    }
    let b_sol_before = vb.lamports();
    swap(sell, args.venue, false, b_amount, args.min_sell, sb)?;
    if token::balance(tb, base.key, vb.key, btp.key)? != 0 {
        return Err(SettlementError::CustodyNotEmpty.into());
    }
    cleanup(sell, &args, false, false, sb)?;
    if args.native {
        unwrap(bq, vb, vb, qtp, sb)?;
    }
    let proceeds = if args.native {
        vb.lamports()
            .checked_sub(b_sol_before)
            .ok_or(SettlementError::BaselineMismatch)?
    } else {
        token::balance(bq, quote.key, vb.key, qtp.key)?
    };
    if proceeds < args.min_sell {
        return Err(SettlementError::SlippageExceeded.into());
    }
    let funding = proceeds.min(args.max_buy);
    sol(vb, buyer, system, args.buyer_rent, sb)?;
    let buy_input = if args.native {
        sol(vb, buyer, system, funding, sb)?;
        funding
    } else {
        // A legitimate fee-recipient alias may have received quote during the sell.
        // Only this funding transfer is authorized as input to the buy.
        let quote_before_funding = token::balance(buyer_quote, quote.key, buyer.key, qtp.key)?;
        token::transfer(bq, quote, buyer_quote, vb, qtp, funding, qd, sb)?;
        token::balance(buyer_quote, quote.key, buyer.key, qtp.key)?
            .checked_sub(quote_before_funding)
            .ok_or(SettlementError::BaselineMismatch)?
    };
    if buy_input == 0 {
        return Err(SettlementError::SlippageExceeded.into());
    }
    swap(buy, args.venue, true, buy_input, args.min_buy, &[])?;
    let acquired = token::balance(buyer_base, base.key, buyer.key, btp.key)?
        .checked_sub(buyer_base_before)
        .ok_or(SettlementError::BaselineMismatch)?;
    if acquired < args.min_buy || buyer.lamports() < buyer_sol_before {
        return Err(SettlementError::SlippageExceeded.into());
    }
    if !args.native
        && token::balance(buyer_quote, quote.key, buyer.key, qtp.key)? < buyer_quote_before
    {
        return Err(SettlementError::BaselineMismatch.into());
    }
    cleanup(buy, &args, true, buyer_volume_existed, &[])?;
    if args.native {
        unwrap(buyer_quote, buyer, buyer, qtp, &[])?;
    } else {
        let residual = token::balance(buyer_quote, quote.key, buyer.key, qtp.key)?
            .checked_sub(buyer_quote_before)
            .ok_or(SettlementError::BaselineMismatch)?;
        if residual > 0 {
            token::transfer(
                buyer_quote,
                quote,
                source_quote,
                buyer,
                qtp,
                residual,
                qd,
                &[],
            )?;
        }
        let excess = token::balance(bq, quote.key, vb.key, qtp.key)?;
        if excess > 0 {
            token::transfer(bq, quote, source_quote, vb, qtp, excess, qd, sb)?;
        }
        if token::balance(buyer_quote, quote.key, buyer.key, qtp.key)? != buyer_quote_before {
            return Err(SettlementError::BaselineMismatch.into());
        }
        token::close_empty(bq, quote, source, vb, qtp, sb)?;
        if buyer_quote_new {
            token::close_empty(buyer_quote, quote, source, buyer, qtp, &[])?;
        }
        if source_quote_new {
            if *quote.key == WSOL && *qtp.key == SPL {
                unwrap(source_quote, source, source, qtp, &[])?;
            } else if token::balance(source_quote, quote.key, source.key, qtp.key)? == 0 {
                token::close_empty(source_quote, quote, source, source, qtp, &[])?;
            }
        }
    }
    let sol_refund = buyer
        .lamports()
        .checked_sub(buyer_sol_before)
        .ok_or(SettlementError::BaselineMismatch)?;
    sol(buyer, source, system, sol_refund, &[])?;
    for (vault, tokens, seeds) in [(va, ta, sa), (vb, tb, sb)] {
        token::close_empty(tokens, base, source, vault, btp, seeds)?;
        signed(
            &system_instruction::assign(vault.key, program_id),
            &[vault.clone(), system.clone()],
            seeds,
        )?;
        transfer_from_program_owned(vault, source, vault.lamports())?;
        if vault.lamports() != 0 || tokens.lamports() != 0 {
            return Err(SettlementError::CleanupMismatch.into());
        }
    }
    if buyer.lamports() != buyer_sol_before
        || bq.lamports() != 0
        || (buyer_quote_new && buyer_quote.lamports() != 0)
    {
        return Err(SettlementError::CleanupMismatch.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wire() -> Vec<u8> {
        let mut d = vec![TAG, 1, 0, 1];
        d.extend([7; 32]);
        for n in [100u64, 50, 1, 1, 90, 20, 10] {
            d.extend(n.to_le_bytes());
        }
        d.extend([26, 27]);
        d
    }
    #[test]
    fn wire_is_bounded_and_rent_never_exceeds_reviewed_seed() {
        let d = wire();
        assert_eq!(parse(&d).unwrap().amount, 50);
        for n in 0..d.len() {
            assert!(parse(&d[..n]).is_err());
        }
        let mut bad = d.clone();
        bad.push(0);
        assert!(parse(&bad).is_err());
        let mut bad = d.clone();
        bad[84..92].copy_from_slice(&21u64.to_le_bytes());
        assert!(parse(&bad).is_err());
        let mut bad = d.clone();
        bad[2] = 1;
        assert!(parse(&bad).is_err());
        let mut bad = d.clone();
        bad[4..36].fill(0);
        assert!(parse(&bad).is_err());
        for at in [36, 44, 52, 60, 68, 76] {
            let mut bad = d.clone();
            bad[at..at + 8].fill(0);
            assert!(parse(&bad).is_err());
        }
    }
}
