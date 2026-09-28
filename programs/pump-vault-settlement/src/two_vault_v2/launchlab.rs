//! LaunchLab buy_exact_in / sell_exact_in, zero share fee. Sources:
//! raydium-sdk-V2/src/raydium/launchpad/{layout,pda,instrument}.ts.
//! SDK revision: cc33ec28a8921a35609e83293e9e07ad830b0779. Its 18-account
//! instruction extends the published IDL's 15 core accounts with fee vaults.
use super::cpmm::{
    executable, instruction, invoke, key, mint, pda, privileges, require, state, token, POOL,
    TOKEN, TOKEN22,
};
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, pubkey, pubkey::Pubkey,
};
const MAIN: Pubkey = pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
const DEV: Pubkey = pubkey!("DRay6fNdQ5J82H7xV6uq2aV3mNrUZ1J4PgSKsWgptcm6");
const BUY: [u8; 8] = [250, 234, 13, 123, 213, 156, 19, 236];
const SELL: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26];
const WRITABLE: &[usize] = &[0, 4, 5, 6, 7, 8, 16, 17];
fn data(buy: bool, amount: u64, min_out: u64) -> Vec<u8> {
    let mut d = if buy { BUY } else { SELL }.to_vec();
    for n in [amount, min_out, 0] {
        d.extend_from_slice(&n.to_le_bytes());
    }
    d
}
/// Official 15 core accounts plus system, platform-fee and creator-fee PDAs.
/// LaunchLab's base/quote orientation is fixed by its bonding curve.
pub fn validate(
    a: &[AccountInfo],
    user: &Pubkey,
    base: &Pubkey,
    quote: &Pubkey,
    base_tp: &Pubkey,
    quote_tp: &Pubkey,
    _buy: bool,
) -> ProgramResult {
    require(a.len() == 18 && base != quote)?;
    executable(&a[14], &[MAIN, DEV])?;
    let program = a[14].key;
    privileges(a, WRITABLE)?;
    require(a[0].key == user)?;
    state(&a[4], program, 429, &POOL)?;
    state(&a[2], program, 371, &[149, 8, 156, 202, 160, 252, 176, 217])?;
    require(
        a[3].owner == program
            && a[3].data_len() >= 944
            && a[3].try_borrow_data()?.get(..8)
                == Some([160, 78, 128, 0, 248, 83, 230, 160].as_slice()),
    )?;
    let d = a[4].try_borrow_data()?;
    require(d[17] == 0 && d[365] <= 3 && key(&d, 141)? == *a[2].key && key(&d, 173)? == *a[3].key)?;
    require(
        key(&d, 205)? == *base
            && key(&d, 237)? == *quote
            && key(&d, 269)? == *a[7].key
            && key(&d, 301)? == *a[8].key,
    )?;
    require(*base_tp == if d[365] & 1 == 0 { TOKEN } else { TOKEN22 })?;
    require(*quote_tp == if d[365] & 2 == 0 { TOKEN } else { TOKEN22 })?;
    let c = a[2].try_borrow_data()?;
    require(key(&c, 83)? == *quote && c[16] <= 2)?;
    pda(
        a[2].key,
        program,
        &[
            b"global_config",
            quote.as_ref(),
            &c[16..17],
            &u16::from_le_bytes([c[17], c[18]]).to_be_bytes(),
        ],
    )?;
    pda(a[4].key, program, &[b"pool", base.as_ref(), quote.as_ref()])?;
    pda(a[1].key, program, &[b"vault_auth_seed"])?;
    pda(
        a[7].key,
        program,
        &[b"pool_vault", a[4].key.as_ref(), base.as_ref()],
    )?;
    pda(
        a[8].key,
        program,
        &[b"pool_vault", a[4].key.as_ref(), quote.as_ref()],
    )?;
    mint(&a[9], base, base_tp)?;
    mint(&a[10], quote, quote_tp)?;
    executable(&a[11], &[*base_tp])?;
    executable(&a[12], &[*quote_tp])?;
    token(&a[5], base, user, base_tp, true)?;
    token(&a[6], quote, user, quote_tp, true)?;
    token(&a[7], base, a[1].key, base_tp, false)?;
    token(&a[8], quote, a[1].key, quote_tp, false)?;
    pda(a[13].key, program, &[b"__event_authority"])?;
    executable(&a[15], &[Pubkey::default()])?;
    pda(a[16].key, program, &[a[3].key.as_ref(), quote.as_ref()])?;
    pda(
        a[17].key,
        program,
        &[key(&d, 333)?.as_ref(), quote.as_ref()],
    )?;
    // Fee vaults can be lazily created by the venue. They are platform/creator
    // scoped, not user/PDA-B scoped. Existing vault authority is venue-owned.
    for (i, seed) in [
        (16, b"platform_fee_vault_auth_seed".as_slice()),
        (17, b"creator_fee_vault_auth_seed".as_slice()),
    ] {
        if a[i].data_is_empty() {
            require(a[i].owner == &Pubkey::default())?;
        } else {
            token(
                &a[i],
                quote,
                &Pubkey::find_program_address(&[seed], program).0,
                quote_tp,
                false,
            )?;
        }
    }
    Ok(())
}
pub fn swap<'a>(
    a: &[AccountInfo<'a>],
    buy: bool,
    amount: u64,
    min_out: u64,
    seeds: &[&[u8]],
) -> ProgramResult {
    require(a.len() == 18 && amount > 0)?;
    executable(&a[14], &[MAIN, DEV])?;
    invoke(
        &instruction(a, 14, WRITABLE, data(buy, amount, min_out), true),
        a,
        seeds,
    )
}
#[cfg(test)]
mod tests {
    use super::super::cpmm::test_support::*;
    use super::*;
    fn fixture(p: Pubkey, qt: Pubkey) -> (Vec<AccountInfo<'static>>, Pubkey, Pubkey, Pubkey) {
        let user = Pubkey::new_unique();
        let base = Pubkey::new_unique();
        let quote = Pubkey::new_unique();
        let platform = Pubkey::new_unique();
        let creator = Pubkey::new_unique();
        let conf = derive(p, &[b"global_config", quote.as_ref(), &[0], &[0, 0]]);
        let pool = derive(p, &[b"pool", base.as_ref(), quote.as_ref()]);
        let auth = derive(p, &[b"vault_auth_seed"]);
        let v0 = derive(p, &[b"pool_vault", pool.as_ref(), base.as_ref()]);
        let v1 = derive(p, &[b"pool_vault", pool.as_ref(), quote.as_ref()]);
        let mut d = bytes(429, POOL);
        for (o, k) in [
            (141, conf),
            (173, platform),
            (205, base),
            (237, quote),
            (269, v0),
            (301, v1),
            (333, creator),
        ] {
            put(&mut d, o, k);
        }
        d[365] = if qt == TOKEN22 { 3 } else { 1 };
        let mut c = bytes(371, [149, 8, 156, 202, 160, 252, 176, 217]);
        put(&mut c, 83, quote);
        let a = vec![
            plain(user),
            plain(auth),
            account(conf, p, c, false),
            account(
                platform,
                p,
                bytes(944, [160, 78, 128, 0, 248, 83, 230, 160]),
                false,
            ),
            account(pool, p, d, false),
            user_token(base, user, TOKEN22),
            user_token(quote, user, qt),
            token_account(v0, base, auth, TOKEN22),
            token_account(v1, quote, auth, qt),
            mint_account(base, TOKEN22),
            mint_account(quote, qt),
            program(TOKEN22),
            program(qt),
            plain(derive(p, &[b"__event_authority"])),
            program(p),
            program(Pubkey::default()),
            plain(derive(p, &[platform.as_ref(), quote.as_ref()])),
            plain(derive(p, &[creator.as_ref(), quote.as_ref()])),
        ];
        (a, user, base, quote)
    }
    #[test]
    fn validates_custom_legacy_and_token22_quote() {
        for p in [MAIN, DEV] {
            for qt in [TOKEN, TOKEN22] {
                for buy in [false, true] {
                    let (a, u, b, q) = fixture(p, qt);
                    assert_eq!(validate(&a, &u, &b, &q, &TOKEN22, &qt, buy), Ok(()));
                    let ix = instruction(&a, 14, WRITABLE, data(buy, 1, 1), true);
                    assert_eq!(ix.accounts.len(), 18);
                    assert_eq!(ix.accounts.iter().filter(|m| m.is_signer).count(), 1);
                }
            }
        }
    }
    #[test]
    fn rejects_wrong_config_creator_fee_pda_and_program_flag() {
        let (a, u, b, q) = fixture(MAIN, TOKEN);
        a[2].try_borrow_mut_data().unwrap()[83] ^= 1;
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (mut a, u, b, q) = fixture(MAIN, TOKEN);
        a[17] = plain(Pubkey::new_unique());
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
        let (a, u, b, q) = fixture(MAIN, TOKEN);
        a[4].try_borrow_mut_data().unwrap()[365] = 3;
        assert!(validate(&a, &u, &b, &q, &TOKEN22, &TOKEN, false).is_err());
    }
    #[test]
    fn exact_input_no_share_fee() {
        for (buy, disc) in [(true, BUY), (false, SELL)] {
            let d = data(buy, 91, 73);
            assert_eq!(&d[..8], &disc);
            assert_eq!(d.len(), 32);
            assert_eq!(&d[8..16], &91u64.to_le_bytes());
            assert_eq!(&d[16..24], &73u64.to_le_bytes());
            assert_eq!(&d[24..], &[0; 8]);
        }
    }
    #[test]
    fn rejects_truncated_accounts() {
        let k = Pubkey::new_unique();
        assert!(validate(&[], &k, &k, &k, &TOKEN, &TOKEN, true).is_err());
        assert!(swap(&[], true, 1, 1, &[]).is_err());
    }
}
