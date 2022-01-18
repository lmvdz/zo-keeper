use anchor_lang::{
    prelude::{AccountInfo, AccountLoader},
    Owner, ZeroCopy,
};

use anchor_client::{ClientError::SolanaClientError, RequestBuilder};

use solana_account_decoder::{UiAccountEncoding};
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
    rpc_request::RpcError,
};
use solana_sdk::{
    account::Account, commitment_config::CommitmentConfig, pubkey::Pubkey,
    signature::Signature,
};

use std::ops::Deref;

use tracing::error;

use zo_abi::{Cache, OpenOrdersInfo, OracleCache, Symbol, MAX_MARKETS};

use crate::liquidator::error::ErrorCode;

pub fn get_account_info<'a>(
    key: &'a Pubkey,
    account: &'a mut Account,
) -> AccountInfo<'a> {
    let account_info: AccountInfo<'_> = (key, account).into();
    account_info
}

#[tracing::instrument(
    skip_all,
    level = "error",
    fields(key = %key, ty = %std::any::type_name::<T>())
)]
pub fn get_type_from_account<T>(key: &Pubkey, account: &mut Account) -> T
where
    T: ZeroCopy + Owner,
{
    let account_info: AccountInfo<'_> = get_account_info(key, account);
    let loader: AccountLoader<'_, T> =
        AccountLoader::try_from(&account_info).unwrap();
    let value = loader.load();
    match value {
        Ok(x) => *x.deref(),
        Err(e) => {
            error!("Failed to get type from {}: {:?}.", key, e);
            panic!()
        }
    }
}

pub fn load_program_accounts<T>(
    client: &RpcClient,
    program_address: &Pubkey,
) -> Result<Vec<(Pubkey, T)>, ErrorCode>
where
    T: ZeroCopy + Owner,
{
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::DataSize((8 + std::mem::size_of::<T>()) as u64),
            RpcFilterType::Memcmp(Memcmp {
                offset: 0,
                bytes: MemcmpEncodedBytes::Bytes(T::discriminator().into()),
                encoding: None,
            }),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: None,
            commitment: Some(CommitmentConfig::finalized()),
        },
        with_context: Some(false),
    };

    client
        .get_program_accounts_with_config(program_address, config)
        .map(|v| {
            v.into_iter()
                .map(|(k, mut a)| (k, get_type_from_account::<T>(&k, &mut a)))
                .collect()
        })
        .map_err(|_| ErrorCode::FetchAccountFailure)
}

fn get_oracle_index(cache: &Cache, s: &Symbol) -> Option<usize> {
    if s.is_nil() {
        return None;
    }

    (&cache.oracles)
        .binary_search_by_key(s, |&x| x.symbol)
        .ok()
}

pub fn get_oracle<'a>(
    cache: &'a Cache,
    s: &Symbol,
) -> Option<&'a OracleCache> {
    Some(&cache.oracles[get_oracle_index(cache, s)?])
}

pub fn get_oo_keys(
    agg: &[OpenOrdersInfo; MAX_MARKETS as usize],
) -> [Pubkey; MAX_MARKETS as usize] {
    let mut keys: [Pubkey; MAX_MARKETS as usize] =
        [Pubkey::default(); MAX_MARKETS as usize];

    for (i, oo) in agg.iter().enumerate() {
        keys[i] = oo.key;
    }

    keys
}

pub fn is_right_remainder(key: &Pubkey, modulus: u8, remainder: u8) -> bool {
    /*
     * This should be used strictly for control accounts.
     * For margin accounts, check it on the control field.
     */

    // Convert the key to a number
    // The hash which actually does the conversion is bad.
    // The hash which just does the sum is good
    // Convert key to bytes and sum?
    let bytes = key.to_bytes();
    let mut sum = 0;
    for byte in bytes {
        sum += byte % modulus;
    }

    sum % modulus == remainder
}

pub fn array_to_le_bytes(array: &[u64; 4]) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (i, x) in array.iter().enumerate() {
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&x.to_le_bytes());
    }
    bytes
}

pub fn array_to_pubkey(array: &[u64; 4]) -> Pubkey {
    Pubkey::new(&array_to_le_bytes(array))
}

#[tracing::instrument(skip_all, level = "error")]
pub fn retry_send<'a>(
    make_builder: impl Fn() -> RequestBuilder<'a>,
    retries: usize,
) -> Result<Signature, ErrorCode> {
    let mut last_error: Option<_> = None;

    for _i in 0..retries {
        let request_builder = make_builder();

        match request_builder.send() {
            Ok(response) => {
                return Ok(response);
            }
            Err(e) => {
                last_error = Some(e);
            }
        };
    }

    let ix = make_builder().instructions().unwrap();

    if let Some(e) = last_error {
        if let SolanaClientError(ClientError {
            request: _,
            kind:
                ClientErrorKind::RpcError(RpcError::RpcResponseError {
                    code: _,
                    message: _error_msg,
                    data: d,
                }),
        }) = e
        {
            error!("Failed to send request. {:?}", d);
        } else {
            error!("Failed to send request {:?}", e);
        }
    } else {
        error!("Failed to send request {:?}", ix);
    }

    Err(ErrorCode::TimeoutExceeded)
}
