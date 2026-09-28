//! Exact-input Raydium CPMM CPI. Layout and account order follow
//! raydium-io/raydium-cp-swap (states/{pool,config,oracle}.rs, swap_base_input.rs).
//! Source revision: 59fb845a9e5bb569c8b2f3415f13b0c0ebcc6b92.
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
};

pub(super) const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub(super) const TOKEN22: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub(super) const POOL: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
pub(super) const CONFIG: [u8; 8] = [218, 244, 33, 104, 203, 203, 43, 111];
pub(super) const OBSERVATION: [u8; 8] = [122, 174, 197, 53, 129, 9, 165, 132];
const MAIN: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const DEV: Pubkey = pubkey!("DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb");
const SWAP: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];

pub(super) fn require(ok: bool) -> ProgramResult {
    if ok {
        Ok(())
    } else {
        Err(ProgramError::InvalidAccountData)
    }
}
pub(super) fn key(d: &[u8], offset: usize) -> Result<Pubkey, ProgramError> {
    let b: [u8; 32] = d
        .get(offset..offset + 32)
        .ok_or(ProgramError::InvalidAccountData)?
        .try_into()
        .map_err(|_| ProgramError::InvalidAccountData)?;
    Ok(Pubkey::new_from_array(b))
}
pub(super) fn pda(a: &Pubkey, program: &Pubkey, seeds: &[&[u8]]) -> ProgramResult {
    require(*a == Pubkey::find_program_address(seeds, program).0)
}
pub(super) fn state(
    a: &AccountInfo,
    program: &Pubkey,
    len: usize,
    disc: &[u8; 8],
) -> ProgramResult {
    require(
        a.owner == program
            && a.data_len() == len
            && a.try_borrow_data()?.get(..8) == Some(disc.as_slice()),
    )
}
pub(super) fn executable(a: &AccountInfo, ids: &[Pubkey]) -> ProgramResult {
    require(a.executable && ids.contains(a.key))
}
pub(super) fn token(
    a: &AccountInfo,
    mint: &Pubkey,
    owner: &Pubkey,
    tp: &Pubkey,
    ata: bool,
) -> ProgramResult {
    require(*tp == TOKEN || *tp == TOKEN22)?;
    if ata {
        pda(
            a.key,
            &pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
            &[owner.as_ref(), tp.as_ref(), mint.as_ref()],
        )?;
        // The kernel validates venue identities before creating its fresh ATAs.
        // Only the exact canonical, unallocated user account may be absent.
        if a.data_is_empty() {
            return super::token::absent(a);
        }
    }
    super::token::balance(a, mint, owner, tp)?;
    Ok(())
}
pub(super) fn mint(a: &AccountInfo, expected: &Pubkey, tp: &Pubkey) -> ProgramResult {
    require(
        (*tp == TOKEN || *tp == TOKEN22)
            && a.key == expected
            && a.owner == tp
            && a.data_len() >= 82
            && a.try_borrow_data()?[45] == 1,
    )
}
pub(super) fn privileges(a: &[AccountInfo], writable: &[usize]) -> ProgramResult {
    for &i in writable {
        require(a.get(i).is_some_and(|a| a.is_writable))?;
    }
    // Transaction-level privileges are coalesced; CPI metas below deliberately
    // grant signer authority to the validated user alone.
    Ok(())
}
pub(super) fn instruction(
    a: &[AccountInfo],
    program_index: usize,
    writable: &[usize],
    data: Vec<u8>,
    include_program: bool,
) -> Instruction {
    Instruction {
        program_id: *a[program_index].key,
        accounts: a
            .iter()
            .enumerate()
            .filter(|(i, _)| include_program || *i != program_index)
            .map(|(i, a)| {
                if writable.contains(&i) {
                    AccountMeta::new(*a.key, i == 0)
                } else {
                    AccountMeta::new_readonly(*a.key, i == 0)
                }
            })
            .collect(),
        data,
    }
}
pub(super) fn invoke<'a>(
    ix: &Instruction,
    a: &[AccountInfo<'a>],
    seeds: &[&[u8]],
) -> ProgramResult {
    if seeds.is_empty() {
        invoke_signed(ix, a, &[])
    } else {
        invoke_signed(ix, a, &[seeds])
    }
}
fn data(amount: u64, min_out: u64) -> Vec<u8> {
    let mut d = SWAP.to_vec();
    d.extend_from_slice(&amount.to_le_bytes());
    d.extend_from_slice(&min_out.to_le_bytes());
    d
}

/// 13 official accounts in input/output order, followed by executable program.
pub fn validate(
    a: &[AccountInfo],
    user: &Pubkey,
    base: &Pubkey,
    quote: &Pubkey,
    base_tp: &Pubkey,
    quote_tp: &Pubkey,
    buy: bool,
) -> ProgramResult {
    require(a.len() == 14 && base != quote)?;
    executable(&a[13], &[MAIN, DEV])?;
    let program = a[13].key;
    require(a[0].key == user)?;
    privileges(a, &[3, 4, 5, 6, 7, 12])?;
    state(&a[3], program, 637, &POOL)?;
    state(&a[2], program, 236, &CONFIG)?;
    let d = a[3].try_borrow_data()?;
    let m0 = key(&d, 168)?;
    let m1 = key(&d, 200)?;
    require(m0 < m1 && key(&d, 8)? == *a[2].key && d[329] & 4 == 0)?;
    let c = a[2].try_borrow_data()?;
    pda(
        a[2].key,
        program,
        &[
            b"amm_config",
            &u16::from_le_bytes([c[10], c[11]]).to_be_bytes(),
        ],
    )?;
    pda(a[1].key, program, &[b"vault_and_lp_mint_auth_seed"])?;
    let (input, output, itp, otp) = if buy {
        (quote, base, quote_tp, base_tp)
    } else {
        (base, quote, base_tp, quote_tp)
    };
    require((*input == m0 && *output == m1) || (*input == m1 && *output == m0))?;
    let zero = *input == m0;
    require(
        key(&d, if zero { 232 } else { 264 })? == *itp
            && key(&d, if zero { 264 } else { 232 })? == *otp,
    )?;
    require(
        key(&d, if zero { 72 } else { 104 })? == *a[6].key
            && key(&d, if zero { 104 } else { 72 })? == *a[7].key,
    )?;
    executable(&a[8], &[*itp])?;
    executable(&a[9], &[*otp])?;
    mint(&a[10], input, itp)?;
    mint(&a[11], output, otp)?;
    token(&a[4], input, user, itp, true)?;
    token(&a[5], output, user, otp, true)?;
    token(&a[6], input, a[1].key, itp, false)?;
    token(&a[7], output, a[1].key, otp, false)?;
    pda(
        a[6].key,
        program,
        &[b"pool_vault", a[3].key.as_ref(), input.as_ref()],
    )?;
    pda(
        a[7].key,
        program,
        &[b"pool_vault", a[3].key.as_ref(), output.as_ref()],
    )?;
    // initialize.rs explicitly supports both a canonical PDA and a random
    // signer-created pool. Its program-owned state and vault PDAs bind either.
    require(key(&d, 296)? == *a[12].key)?;
    pda(a[12].key, program, &[b"observation", a[3].key.as_ref()])?;
    state(&a[12], program, 4075, &OBSERVATION)?;
    require(key(&a[12].try_borrow_data()?, 11)? == *a[3].key)
}
pub fn swap<'a>(
    a: &[AccountInfo<'a>],
    _buy: bool,
    amount: u64,
    min_out: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    require(a.len() == 14 && amount > 0)?;
    executable(&a[13], &[MAIN, DEV])?;
    invoke(
        &instruction(a, 13, &[3, 4, 5, 6, 7, 12], data(amount, min_out), false),
        a,
        seeds,
    )
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    pub fn account(k: Pubkey, owner: Pubkey, d: Vec<u8>, exec: bool) -> AccountInfo<'static> {
        AccountInfo::new(
            Box::leak(Box::new(k)),
            false,
            true,
            Box::leak(Box::new(1_000_000)),
            Box::leak(d.into_boxed_slice()),
            Box::leak(Box::new(owner)),
            exec,
        )
    }
    pub fn bytes(size: usize, disc: [u8; 8]) -> Vec<u8> {
        let mut d = vec![0; size];
        d[..8].copy_from_slice(&disc);
        d
    }
    pub fn put(d: &mut [u8], offset: usize, k: Pubkey) {
        d[offset..offset + 32].copy_from_slice(k.as_ref());
    }
    pub fn derive(p: Pubkey, seeds: &[&[u8]]) -> Pubkey {
        Pubkey::find_program_address(seeds, &p).0
    }
    pub fn token_account(k: Pubkey, m: Pubkey, o: Pubkey, tp: Pubkey) -> AccountInfo<'static> {
        let mut d = vec![0; 165];
        put(&mut d, 0, m);
        put(&mut d, 32, o);
        d[108] = 1;
        account(k, tp, d, false)
    }
    pub fn mint_account(k: Pubkey, tp: Pubkey) -> AccountInfo<'static> {
        let mut d = vec![0; 82];
        d[45] = 1;
        account(k, tp, d, false)
    }
    pub fn user_token(m: Pubkey, o: Pubkey, tp: Pubkey) -> AccountInfo<'static> {
        token_account(
            derive(
                pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"),
                &[o.as_ref(), tp.as_ref(), m.as_ref()],
            ),
            m,
            o,
            tp,
        )
    }
    pub fn plain(k: Pubkey) -> AccountInfo<'static> {
        account(k, Pubkey::default(), vec![], false)
    }
    pub fn program(k: Pubkey) -> AccountInfo<'static> {
        account(k, Pubkey::default(), vec![], true)
    }
}
#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    fn fixture(p: Pubkey, buy: bool) -> (Vec<AccountInfo<'static>>, Pubkey, Pubkey, Pubkey) {
        let user = Pubkey::new_unique();
        let mut ms = [Pubkey::new_unique(), Pubkey::new_unique()];
        ms.sort();
        let [base, quote] = ms;
        // Deliberately random, valid program-owned pool: not every CPMM pool is a PDA.
        let pool = Pubkey::new_unique();
        let conf = derive(p, &[b"amm_config", &[0, 0]]);
        let auth = derive(p, &[b"vault_and_lp_mint_auth_seed"]);
        let v0 = derive(p, &[b"pool_vault", pool.as_ref(), base.as_ref()]);
        let v1 = derive(p, &[b"pool_vault", pool.as_ref(), quote.as_ref()]);
        let obs = derive(p, &[b"observation", pool.as_ref()]);
        let mut d = bytes(637, POOL);
        for (o, k) in [
            (8, conf),
            (72, v0),
            (104, v1),
            (168, base),
            (200, quote),
            (232, TOKEN22),
            (264, TOKEN),
            (296, obs),
        ] {
            put(&mut d, o, k);
        }
        let mut ob = bytes(4075, OBSERVATION);
        put(&mut ob, 11, pool);
        let (im, om, it, ot, iv, ov) = if buy {
            (quote, base, TOKEN, TOKEN22, v1, v0)
        } else {
            (base, quote, TOKEN22, TOKEN, v0, v1)
        };
        (
            vec![
                plain(user),
                plain(auth),
                account(conf, p, bytes(236, CONFIG), false),
                account(pool, p, d, false),
                user_token(im, user, it),
                user_token(om, user, ot),
                token_account(iv, im, auth, it),
                token_account(ov, om, auth, ot),
                program(it),
                program(ot),
                mint_account(im, it),
                mint_account(om, ot),
                account(obs, p, ob, false),
                program(p),
            ],
            user,
            base,
            quote,
        )
    }
    #[test]
    fn custom_quote_both_directions_and_clusters() {
        for p in [MAIN, DEV] {
            for buy in [false, true] {
                let (a, u, b, q) = fixture(p, buy);
                assert_eq!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, buy), Ok(()));
                let ix = instruction(&a, 13, &[3, 4, 5, 6, 7, 12], data(1, 1), false);
                assert_eq!(ix.accounts.len(), 13);
                assert_eq!(ix.accounts.iter().filter(|a| a.is_signer).count(), 1);
                assert_eq!(ix.accounts[0].pubkey, u);
            }
        }
    }
    #[test]
    fn rejects_wrong_pool_mint_owner_and_vault() {
        let (mut a, u, b, q) = fixture(MAIN, false);
        a[6].try_borrow_mut_data().unwrap()[0] ^= 1;
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (fresh, _, _, _) = fixture(MAIN, false);
        a[6] = fresh[6].clone();
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (mut a, u, b, q) = fixture(MAIN, false);
        a[13] = program(Pubkey::new_unique());
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (a, u, b, q) = fixture(MAIN, false);
        a[3].try_borrow_mut_data().unwrap()[0] ^= 1;
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
    }
    #[test]
    fn canonical_absent_user_ata_allowed_but_not_absent_vault() {
        let (mut a, u, b, q) = fixture(MAIN, false);
        let missing = account(*a[4].key, Pubkey::default(), vec![], false);
        **missing.try_borrow_mut_lamports().unwrap() = 0;
        a[4] = missing;
        assert_eq!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false), Ok(()));
        let missing = account(*a[6].key, Pubkey::default(), vec![], false);
        **missing.try_borrow_mut_lamports().unwrap() = 0;
        a[6] = missing;
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
    }
    #[test]
    fn wire_exact_input_and_boundaries() {
        let d = data(u64::MAX, 42);
        assert_eq!(&d[..8], &SWAP);
        assert_eq!(d.len(), 24);
        assert_eq!(&d[8..16], &u64::MAX.to_le_bytes());
        assert_eq!(&d[16..], &42u64.to_le_bytes());
    }
    #[test]
    fn short_input_is_error_not_panic() {
        let k = Pubkey::new_unique();
        assert!(validate(&[], &k, &k, &k, &TOKEN, &TOKEN, false).is_err());
        assert!(key(&[0; 31], 0).is_err());
        assert!(swap(&[], false, 1, 1, &[]).is_err());
    }
}
