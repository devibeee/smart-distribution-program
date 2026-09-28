//! Raydium CLMM swap_v2 with pool-bound dynamic tick arrays/bitmap extension.
//! Sources: raydium-clmm/programs/amm/src/{states,instructions/swap_v2.rs}.
//! Source revision: ed7c84a54ced59c55981780546adb0b4583dcf85.
use super::cpmm::{
    executable, instruction, invoke, key, mint, pda, privileges, require, state, token, CONFIG,
    OBSERVATION, POOL, TOKEN, TOKEN22,
};
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError, pubkey,
    pubkey::Pubkey,
};
const MAIN: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
const DEV: Pubkey = pubkey!("DRayAUgENGQBKVaX8owNhgzkEDyoHTGVEGHVJT1E9pfH");
const MEMO: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const TICK: [u8; 8] = [192, 155, 85, 205, 49, 249, 129, 42];
const BITMAP: [u8; 8] = [60, 150, 36, 219, 97, 128, 139, 153];
const SWAP: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
fn data(amount: u64, min_out: u64) -> Vec<u8> {
    let mut d = SWAP.to_vec();
    d.extend_from_slice(&amount.to_le_bytes());
    d.extend_from_slice(&min_out.to_le_bytes());
    // Zero delegates directional extreme-price limits to the venue, which
    // requires the complete exact-input amount when no custom limit is given.
    d.extend_from_slice(&0u128.to_le_bytes());
    d.push(1);
    d
}
fn writable(len: usize) -> Vec<usize> {
    (2..8).chain(13..len - 1).collect()
}
fn remaining(
    a: &[AccountInfo],
    program: &Pubkey,
    pool: &Pubkey,
    span: i32,
    zero: bool,
) -> ProgramResult {
    let mut previous: Option<i32> = None;
    let mut bitmap = false;
    let mut count = 0;
    for info in a {
        require(info.owner == program && info.is_writable)?;
        if info.data_len() == 1832 {
            require(!bitmap)?;
            bitmap = true;
            state(info, program, 1832, &BITMAP)?;
            require(key(&info.try_borrow_data()?, 8)? == *pool)?;
            pda(
                info.key,
                program,
                &[b"pool_tick_array_bitmap_extension", pool.as_ref()],
            )?;
        } else {
            state(info, program, 10240, &TICK)?;
            let d = info.try_borrow_data()?;
            require(key(&d, 8)? == *pool)?;
            let start = i32::from_le_bytes(
                d[40..44]
                    .try_into()
                    .map_err(|_| ProgramError::InvalidAccountData)?,
            );
            require(
                span > 0
                    && start % span == 0
                    && start >= (-443636i32).div_euclid(span) * span
                    && start <= 443636,
            )?;
            if let Some(prev) = previous {
                require(if zero { start < prev } else { start > prev })?;
            }
            pda(
                info.key,
                program,
                &[b"tick_array", pool.as_ref(), &start.to_be_bytes()],
            )?;
            previous = Some(start);
            count += 1;
        }
    }
    require(count > 0)
}
/// Official 13 swap_v2 accounts, any required ordered tick arrays and optional
/// bitmap extension, then executable venue. No arbitrary remaining accounts.
pub fn validate(
    a: &[AccountInfo],
    user: &Pubkey,
    base: &Pubkey,
    quote: &Pubkey,
    base_tp: &Pubkey,
    quote_tp: &Pubkey,
    buy: bool,
) -> ProgramResult {
    require(a.len() >= 15 && base != quote)?;
    let program = a[a.len() - 1].key;
    executable(&a[a.len() - 1], &[MAIN, DEV])?;
    require(a[0].key == user)?;
    privileges(a, &writable(a.len()))?;
    state(&a[2], program, 1544, &POOL)?;
    state(&a[1], program, 117, &CONFIG)?;
    let d = a[2].try_borrow_data()?;
    let c = a[1].try_borrow_data()?;
    let m0 = key(&d, 73)?;
    let m1 = key(&d, 105)?;
    require(m0 < m1 && key(&d, 9)? == *a[1].key && d[389] & 16 == 0)?;
    pda(
        a[2].key,
        program,
        &[
            b"pool",
            a[1].key.as_ref(),
            m0.as_ref(),
            m1.as_ref(),
            if d[391..393] == [0, 0] {
                &[]
            } else {
                &d[391..393]
            },
        ],
    )?;
    pda(
        a[1].key,
        program,
        &[
            b"amm_config",
            &u16::from_le_bytes([c[9], c[10]]).to_be_bytes(),
        ],
    )?;
    let spacing = u16::from_le_bytes([d[235], d[236]]);
    require(spacing > 0 && c[51..53] == d[235..237])?;
    let (input, output, itp, otp) = if buy {
        (quote, base, quote_tp, base_tp)
    } else {
        (base, quote, base_tp, quote_tp)
    };
    require((*input == m0 && *output == m1) || (*input == m1 && *output == m0))?;
    let zero = *input == m0;
    require(
        key(&d, if zero { 137 } else { 169 })? == *a[5].key
            && key(&d, if zero { 169 } else { 137 })? == *a[6].key,
    )?;
    mint(&a[11], input, itp)?;
    mint(&a[12], output, otp)?;
    executable(&a[8], &[TOKEN])?;
    executable(&a[9], &[TOKEN22])?;
    executable(&a[10], &[MEMO])?;
    token(&a[3], input, user, itp, true)?;
    token(&a[4], output, user, otp, true)?;
    token(&a[5], input, a[2].key, itp, false)?;
    token(&a[6], output, a[2].key, otp, false)?;
    pda(
        a[5].key,
        program,
        &[b"pool_vault", a[2].key.as_ref(), input.as_ref()],
    )?;
    pda(
        a[6].key,
        program,
        &[b"pool_vault", a[2].key.as_ref(), output.as_ref()],
    )?;
    require(key(&d, 201)? == *a[7].key)?;
    pda(a[7].key, program, &[b"observation", a[2].key.as_ref()])?;
    state(&a[7], program, 4483, &OBSERVATION)?;
    require(key(&a[7].try_borrow_data()?, 19)? == *a[2].key)?;
    // Pool-owned runtime handles current liquidity, dynamic fees, bitmap
    // traversal and initialized-tick semantics; we constrain account identity.
    remaining(
        &a[13..a.len() - 1],
        program,
        a[2].key,
        i32::from(spacing) * 60,
        zero,
    )
}
pub fn swap<'a>(
    a: &[AccountInfo<'a>],
    _buy: bool,
    amount: u64,
    min_out: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    require(a.len() >= 15 && amount > 0)?;
    let last = a.len() - 1;
    executable(&a[last], &[MAIN, DEV])?;
    invoke(
        &instruction(a, last, &writable(a.len()), data(amount, min_out), false),
        a,
        seeds,
    )
}
#[cfg(test)]
mod tests {
    use super::super::cpmm::test_support::*;
    use super::*;
    fn tick(p: Pubkey, pool: Pubkey, start: i32) -> AccountInfo<'static> {
        let mut d = bytes(10240, TICK);
        put(&mut d, 8, pool);
        d[40..44].copy_from_slice(&start.to_le_bytes());
        account(
            derive(p, &[b"tick_array", pool.as_ref(), &start.to_be_bytes()]),
            p,
            d,
            false,
        )
    }
    fn extension(p: Pubkey, pool: Pubkey) -> AccountInfo<'static> {
        let mut d = bytes(1832, BITMAP);
        put(&mut d, 8, pool);
        account(
            derive(p, &[b"pool_tick_array_bitmap_extension", pool.as_ref()]),
            p,
            d,
            false,
        )
    }
    fn fixture(p: Pubkey, buy: bool) -> (Vec<AccountInfo<'static>>, Pubkey, Pubkey, Pubkey) {
        indexed_fixture(p, buy, [0, 0])
    }
    fn indexed_fixture(
        p: Pubkey,
        buy: bool,
        index: [u8; 2],
    ) -> (Vec<AccountInfo<'static>>, Pubkey, Pubkey, Pubkey) {
        let user = Pubkey::new_unique();
        let mut ms = [Pubkey::new_unique(), Pubkey::new_unique()];
        ms.sort();
        let [base, quote] = ms;
        let conf = derive(p, &[b"amm_config", &[0, 0]]);
        let pool = derive(
            p,
            &[
                b"pool",
                conf.as_ref(),
                base.as_ref(),
                quote.as_ref(),
                if index == [0, 0] { &[] } else { &index },
            ],
        );
        let v0 = derive(p, &[b"pool_vault", pool.as_ref(), base.as_ref()]);
        let v1 = derive(p, &[b"pool_vault", pool.as_ref(), quote.as_ref()]);
        let obs = derive(p, &[b"observation", pool.as_ref()]);
        let mut d = bytes(1544, POOL);
        for (o, k) in [
            (9, conf),
            (73, base),
            (105, quote),
            (137, v0),
            (169, v1),
            (201, obs),
        ] {
            put(&mut d, o, k);
        }
        d[235] = 1;
        d[391..393].copy_from_slice(&index);
        let mut c = bytes(117, CONFIG);
        c[51] = 1;
        let mut ob = bytes(4483, OBSERVATION);
        put(&mut ob, 19, pool);
        let (im, om, it, ot, iv, ov) = if buy {
            (quote, base, TOKEN, TOKEN22, v1, v0)
        } else {
            (base, quote, TOKEN22, TOKEN, v0, v1)
        };
        let mut a = vec![
            plain(user),
            account(conf, p, c, false),
            account(pool, p, d, false),
            user_token(im, user, it),
            user_token(om, user, ot),
            token_account(iv, im, pool, it),
            token_account(ov, om, pool, ot),
            account(obs, p, ob, false),
            program(TOKEN),
            program(TOKEN22),
            program(MEMO),
            mint_account(im, it),
            mint_account(om, ot),
            extension(p, pool),
        ];
        for i in 0..8 {
            a.push(tick(p, pool, if buy { i * 60 } else { -i * 60 }));
        }
        a.push(program(p));
        (a, user, base, quote)
    }
    #[test]
    fn binds_permissioned_pool_seed_index() {
        for p in [MAIN, DEV] {
            for buy in [false, true] {
                let (a, u, b, q) = indexed_fixture(p, buy, [0x12, 0x13]);
                assert_eq!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, buy), Ok(()));
                a[2].try_borrow_mut_data().unwrap()[391] ^= 1;
                assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, buy).is_err());
            }
        }
    }
    #[test]
    fn supports_eight_arrays_extension_and_both_directions() {
        for p in [MAIN, DEV] {
            for buy in [false, true] {
                let (a, u, b, q) = fixture(p, buy);
                assert_eq!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, buy), Ok(()));
                let ix = instruction(&a, a.len() - 1, &writable(a.len()), data(1, 1), false);
                assert_eq!(ix.accounts.iter().filter(|m| m.is_signer).count(), 1);
                assert_eq!(ix.accounts.len(), a.len() - 1);
            }
        }
    }
    #[test]
    fn rejects_foreign_bitmap_tick_and_non_monotonic_arrays() {
        let (mut a, u, b, q) = fixture(MAIN, false);
        a[13] = extension(MAIN, Pubkey::new_unique());
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (mut a, u, b, q) = fixture(MAIN, false);
        a[14] = tick(MAIN, Pubkey::new_unique(), 0);
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (mut a, u, b, q) = fixture(MAIN, false);
        a.swap(14, 15);
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (mut a, u, b, q) = fixture(MAIN, false);
        a[15] = a[14].clone();
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
    }
    #[test]
    fn exact_input_zero_custom_limit() {
        let d = data(22, 17);
        assert_eq!(d.len(), 41);
        assert_eq!(&d[..8], &SWAP);
        assert_eq!(&d[8..16], &22u64.to_le_bytes());
        assert_eq!(&d[16..24], &17u64.to_le_bytes());
        assert_eq!(&d[24..40], &[0; 16]);
        assert_eq!(d[40], 1);
    }
    #[test]
    fn rejects_short_accounts() {
        let k = Pubkey::new_unique();
        assert!(validate(&[], &k, &k, &k, &TOKEN, &TOKEN, false).is_err());
        assert!(swap(&[], false, 1, 1, &[]).is_err());
    }
}
