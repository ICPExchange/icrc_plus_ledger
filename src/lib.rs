pub mod cdk_runtime;

#[cfg(test)]
mod tests;

use crate::cdk_runtime::CdkRuntime;
use candid::{
    types::number::{Int, Nat},
    CandidType, Principal,
};
use ic_canister_log::{log, Sink};
use ic_crypto_tree_hash::{Label, MixedHashTree};
use ic_icrc1::blocks::encoded_block_to_generic_block;
use ic_icrc1::{Block, LedgerBalances, Transaction};
pub use ic_ledger_canister_core::archive::ArchiveOptions;
use ic_ledger_canister_core::{
    archive::ArchiveCanisterWasm,
    blockchain::Blockchain,
    ledger::{apply_transaction, block_locations, LedgerContext, LedgerData, TransactionInfo},
    range_utils,
};
use ic_ledger_core::{
    approvals::AllowanceTable,
    balances::Balances,
    block::{BlockIndex, BlockType, EncodedBlock, FeeCollector},
    timestamp::TimeStamp,
    tokens::TokensType,
};
use ic_ledger_hash_of::HashOf;
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc3::transactions::Transaction as Tx;
use icrc_ledger_types::icrc3::{blocks::GetBlocksResponse, transactions::GetTransactionsResponse};
use icrc_ledger_types::{
    icrc::generic_metadata_value::MetadataValue as Value,
    icrc3::archive::{ArchivedRange, QueryBlockArchiveFn, QueryTxArchiveFn},
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

const TRANSACTION_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ACCOUNTS: usize = 28_000_000;
/// The maximum number of transactions the ledger should return for a single
/// get_transactions request.
const MAX_TRANSACTIONS_PER_REQUEST: usize = 2_000;
const ACCOUNTS_OVERFLOW_TRIM_QUANTITY: usize = 100_000;
const MAX_TRANSACTIONS_IN_WINDOW: usize = 3_000_000;
const MAX_TRANSACTIONS_TO_PURGE: usize = 100_000;

const DEFAULT_MAX_MEMO_LENGTH: u16 = 32;

#[derive(Debug, Clone)]
pub struct Icrc1ArchiveWasm;

impl ArchiveCanisterWasm for Icrc1ArchiveWasm {
    fn archive_wasm() -> Cow<'static, [u8]> {
        Cow::Borrowed(include_bytes!(env!("IC_ICRC1_ARCHIVE_WASM_PATH")))
    }
}

#[derive(Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct FeeInfo{
    pub transfer_fee:Nat,
    pub burn_fee:Nat,
    pub decimals:Nat,
}

/// Like [endpoints::Value], but can be serialized to CBOR.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub enum StoredValue {
    NatBytes(ByteBuf),
    IntBytes(ByteBuf),
    Text(String),
    Blob(ByteBuf),
}

impl From<StoredValue> for Value {
    fn from(v: StoredValue) -> Self {
        match v {
            StoredValue::NatBytes(num_bytes) => Self::Nat(
                Nat::decode(&mut &num_bytes[..])
                    .unwrap_or_else(|e| panic!("bug: invalid Nat encoding {:?}: {}", num_bytes, e)),
            ),
            StoredValue::IntBytes(int_bytes) => Self::Int(
                Int::decode(&mut &int_bytes[..])
                    .unwrap_or_else(|e| panic!("bug: invalid Int encoding {:?}: {}", int_bytes, e)),
            ),
            StoredValue::Text(text) => Self::Text(text),
            StoredValue::Blob(bytes) => Self::Blob(bytes),
        }
    }
}

impl From<Value> for StoredValue {
    fn from(v: Value) -> Self {
        match v {
            Value::Nat(num) => {
                let mut buf = vec![];
                num.encode(&mut buf).expect("bug: failed to encode nat");
                Self::NatBytes(ByteBuf::from(buf))
            }
            Value::Int(int) => {
                let mut buf = vec![];
                int.encode(&mut buf).expect("bug: failed to encode nat");
                Self::IntBytes(ByteBuf::from(buf))
            }
            Value::Text(text) => Self::Text(text),
            Value::Blob(bytes) => Self::Blob(bytes),
        }
    }
}

#[derive(Clone, Debug)]
pub struct InitArgsBuilder(InitArgs);

impl InitArgsBuilder {
    pub fn with_symbol_and_name(symbol: impl ToString, name: impl ToString) -> Self {
        let mut args = Self::for_tests();
        args.0.token_symbol = symbol.to_string();
        args.0.token_name = name.to_string();
        args
    }

    pub fn for_tests() -> Self {
        let default_owner = Principal::anonymous();
        Self(InitArgs {
            minting_account: Account {
                owner: default_owner,
                subaccount: None,
            },
            fee_collector_account: None,
            initial_balances: vec![],
            transfer_fee: 10_000_u32.into(),
            decimals: None,
            token_name: "Test Token".to_string(),
            token_symbol: "XTK".to_string(),
            metadata: vec![],
            archive_options: ArchiveOptions {
                trigger_threshold: 1000,
                num_blocks_to_archive: 1000,
                node_max_memory_size_bytes: None,
                max_message_size_bytes: None,
                controller_id: default_owner.into(),
                more_controller_ids: None,
                cycles_for_archive_creation: None,
                max_transactions_per_response: None,
            },
            max_memo_length: None,
            feature_flags: None,
            maximum_number_of_accounts: None,
            accounts_overflow_trim_quantity: None,

            burn_fee: 10_000_u32.into(),
            transfer_fee_rate:0u32.into(),
            burn_fee_rate:0u32.into(),
        })
    }

    pub fn with_minting_account(mut self, account: impl Into<Account>) -> Self {
        self.0.minting_account = account.into();
        self
    }

    pub fn with_fee_collector_account(mut self, account: impl Into<Account>) -> Self {
        self.0.fee_collector_account = Some(account.into());
        self
    }

    pub fn with_transfer_fee(mut self, fee: impl Into<Nat>) -> Self {
        self.0.transfer_fee = fee.into();
        self
    }

    pub fn with_decimals(mut self, decimals: u8) -> Self {
        self.0.decimals = Some(decimals);
        self
    }

    pub fn with_archive_options(mut self, options: ArchiveOptions) -> Self {
        self.0.archive_options = options;
        self
    }

    pub fn with_token_symbol(mut self, symbol: impl ToString) -> Self {
        self.0.token_symbol = symbol.to_string();
        self
    }

    pub fn with_token_name(mut self, name: impl ToString) -> Self {
        self.0.token_name = name.to_string();
        self
    }

    pub fn with_metadata_entry(mut self, name: impl ToString, value: impl Into<Value>) -> Self {
        self.0.metadata.push((name.to_string(), value.into()));
        self
    }

    pub fn with_initial_balance(
        mut self,
        account: impl Into<Account>,
        amount: impl Into<Nat>,
    ) -> Self {
        self.0
            .initial_balances
            .push((account.into(), amount.into()));
        self
    }

    pub fn with_max_memo_length(mut self, limit: u16) -> Self {
        self.0.max_memo_length = Some(limit);
        self
    }

    pub fn with_feature_flags(mut self, flags: FeatureFlags) -> Self {
        self.0.feature_flags = Some(flags);
        self
    }

    pub fn build(self) -> InitArgs {
        self.0
    }
}

#[derive(Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct InitArgs {
    pub minting_account: Account,
    pub fee_collector_account: Option<Account>,
    pub initial_balances: Vec<(Account, Nat)>,
    pub transfer_fee: Nat,
    pub decimals: Option<u8>,
    pub token_name: String,
    pub token_symbol: String,
    pub metadata: Vec<(String, Value)>,
    pub archive_options: ArchiveOptions,
    pub max_memo_length: Option<u16>,
    pub feature_flags: Option<FeatureFlags>,
    pub maximum_number_of_accounts: Option<u64>,
    pub accounts_overflow_trim_quantity: Option<u64>,

    pub burn_fee: Nat,
    pub transfer_fee_rate: Nat,
    pub burn_fee_rate: Nat,
}

#[derive(Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub enum ChangeFeeCollector {
    Unset,
    SetTo(Account),
}

impl From<ChangeFeeCollector> for Option<FeeCollector<Account>> {
    fn from(value: ChangeFeeCollector) -> Self {
        match value {
            ChangeFeeCollector::Unset => None,
            ChangeFeeCollector::SetTo(account) => Some(FeeCollector::from(account)),
        }
    }
}

#[derive(Default, Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
pub struct UpgradeArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Vec<(String, Value)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_fee: Option<Nat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_fee: Option<Nat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_fee_rate: Option<Nat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_fee_rate: Option<Nat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_fee_collector: Option<ChangeFeeCollector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memo_length: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_flags: Option<FeatureFlags>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_number_of_accounts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts_overflow_trim_quantity: Option<u64>,
}

#[derive(Deserialize, CandidType, Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum LedgerArgument {
    Init(InitArgs),
    Upgrade(Option<UpgradeArgs>),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(bound = "")]
pub struct Ledger<Tokens: TokensType> {
    balances: LedgerBalances<Tokens>,
    #[serde(default)]
    approvals: AllowanceTable<ApprovalKey, Account, Tokens>,
    blockchain: Blockchain<CdkRuntime, Icrc1ArchiveWasm>,

    minting_account: Account,
    fee_collector: Option<FeeCollector<Account>>,

    transactions_by_hash: BTreeMap<HashOf<Transaction<Tokens>>, BlockIndex>,
    transactions_by_height: VecDeque<TransactionInfo<Transaction<Tokens>>>,
    transfer_fee: Tokens,
    burn_fee: Tokens,
    //fee rate，exp:120_000，then fee rate is div 10_000 12%
    burn_fee_rate:Tokens,
    transfer_fee_rate:Tokens,
    token_symbol: String,
    token_name: String,
    metadata: Vec<(String, StoredValue)>,
    #[serde(default = "default_max_memo_length")]
    max_memo_length: u16,

    #[serde(default = "default_decimals")]
    decimals: u8,

    #[serde(default)]
    feature_flags: FeatureFlags,

    #[serde(default = "default_maximum_number_of_accounts")]
    maximum_number_of_accounts: usize,
    #[serde(default = "default_accounts_overflow_trim_quantity")]
    accounts_overflow_trim_quantity: usize,
}

fn default_maximum_number_of_accounts() -> usize {
    MAX_ACCOUNTS
}

fn default_accounts_overflow_trim_quantity() -> usize {
    ACCOUNTS_OVERFLOW_TRIM_QUANTITY
}

#[derive(CandidType, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct FeatureFlags {
    pub icrc2: bool,
}

impl FeatureFlags {
    const fn const_default() -> Self {
        Self { icrc2: true }
    }
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self::const_default()
    }
}

fn default_max_memo_length() -> u16 {
    DEFAULT_MAX_MEMO_LENGTH
}

fn default_decimals() -> u8 {
    ic_ledger_core::tokens::DECIMAL_PLACES as u8
}

impl<Tokens: TokensType> Ledger<Tokens> {
    pub fn from_init_args(
        sink: impl Sink + Clone,
        InitArgs {
            minting_account,
            initial_balances,
            transfer_fee,
            token_name,
            token_symbol,
            decimals,
            metadata,
            archive_options,
            fee_collector_account,
            max_memo_length,
            feature_flags,
            maximum_number_of_accounts,
            accounts_overflow_trim_quantity,

            burn_fee,
            transfer_fee_rate,
            burn_fee_rate,
        }: InitArgs,
        now: TimeStamp,
    ) -> Self {
        if feature_flags.as_ref().map(|ff| ff.icrc2) == Some(false) {
            log!(
                sink,
                "[ledger] feature flag icrc2 is deprecated and won't disable ICRC-2 anymore"
            );
        }
        let mut ledger = Self {
            balances: LedgerBalances::default(),
            approvals: Default::default(),
            blockchain: Blockchain::new_with_archive(archive_options),
            transactions_by_hash: BTreeMap::new(),
            transactions_by_height: VecDeque::new(),
            minting_account,
            fee_collector: fee_collector_account.map(FeeCollector::from),
            transfer_fee: Tokens::try_from(transfer_fee.clone()).unwrap_or_else(|e| {
                panic!(
                    "failed to convert transfer fee {} to tokens: {}",
                    transfer_fee, e
                )
            }),
            burn_fee: Tokens::try_from(burn_fee.clone()).unwrap_or_else(|e| {
                panic!(
                    "failed to convert burn fee {} to tokens: {}",
                    burn_fee, e
                )
            }),
            transfer_fee_rate: Tokens::try_from(transfer_fee_rate.clone()).unwrap_or_else(|e| {
                panic!(
                    "failed to convert transfer fee rate {} to tokens: {}",
                    transfer_fee_rate, e
                )
            }),
            burn_fee_rate: Tokens::try_from(burn_fee_rate.clone()).unwrap_or_else(|e| {
                panic!(
                    "failed to convert burn fee rate {} to tokens: {}",
                    burn_fee_rate, e
                )
            }),
            token_symbol,
            token_name,
            decimals: decimals.unwrap_or_else(default_decimals),
            metadata: metadata
                .into_iter()
                .map(|(k, v)| (k, StoredValue::from(v)))
                .collect(),
            max_memo_length: max_memo_length.unwrap_or(DEFAULT_MAX_MEMO_LENGTH),
            feature_flags: feature_flags.unwrap_or_default(),
            maximum_number_of_accounts: maximum_number_of_accounts
                .unwrap_or_else(|| MAX_ACCOUNTS.try_into().unwrap())
                .try_into()
                .unwrap(),
            accounts_overflow_trim_quantity: accounts_overflow_trim_quantity
                .unwrap_or_else(|| ACCOUNTS_OVERFLOW_TRIM_QUANTITY.try_into().unwrap())
                .try_into()
                .unwrap(),
        };

        for (account, balance) in initial_balances.into_iter() {
            let amount = Tokens::try_from(balance.clone()).unwrap_or_else(|e| {
                panic!(
                    "failed to convert initial balance {} to tokens: {}",
                    balance, e
                )
            });
            let mint = Transaction::mint(account, amount, Some(now), None);
            apply_transaction(&mut ledger, mint, now, Tokens::zero()).unwrap_or_else(|err| {
                panic!(
                    "failed to mint {} tokens to {}: {:?}",
                    balance, account, err
                )
            });
        }

        ledger
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct ApprovalKey(Account, Account);

impl From<(&Account, &Account)> for ApprovalKey {
    fn from((account, spender): (&Account, &Account)) -> Self {
        Self(*account, *spender)
    }
}

impl From<ApprovalKey> for (Account, Account) {
    fn from(key: ApprovalKey) -> Self {
        (key.0, key.1)
    }
}

impl<Tokens: TokensType> LedgerContext for Ledger<Tokens> {
    type AccountId = Account;
    type Approvals = AllowanceTable<ApprovalKey, Account, Tokens>;
    type BalancesStore = BTreeMap<Self::AccountId, Tokens>;
    type Tokens = Tokens;

    fn balances(&self) -> &Balances<Self::BalancesStore> {
        &self.balances
    }

    fn balances_mut(&mut self) -> &mut Balances<Self::BalancesStore> {
        &mut self.balances
    }

    fn approvals(&self) -> &Self::Approvals {
        &self.approvals
    }

    fn approvals_mut(&mut self) -> &mut Self::Approvals {
        &mut self.approvals
    }

    fn fee_collector(&self) -> Option<&FeeCollector<Self::AccountId>> {
        self.fee_collector.as_ref()
    }
}

impl<Tokens: TokensType> LedgerData for Ledger<Tokens> {
    type Runtime = CdkRuntime;
    type ArchiveWasm = Icrc1ArchiveWasm;
    type Transaction = Transaction<Tokens>;
    type Block = Block<Tokens>;

    fn transaction_window(&self) -> Duration {
        TRANSACTION_WINDOW
    }

    fn max_transactions_in_window(&self) -> usize {
        MAX_TRANSACTIONS_IN_WINDOW
    }

    fn max_transactions_to_purge(&self) -> usize {
        MAX_TRANSACTIONS_TO_PURGE
    }

    fn max_number_of_accounts(&self) -> usize {
        self.maximum_number_of_accounts
    }

    fn accounts_overflow_trim_quantity(&self) -> usize {
        self.accounts_overflow_trim_quantity
    }

    fn token_name(&self) -> &str {
        &self.token_name
    }

    fn token_symbol(&self) -> &str {
        &self.token_symbol
    }

    fn blockchain(&self) -> &Blockchain<Self::Runtime, Self::ArchiveWasm> {
        &self.blockchain
    }

    fn blockchain_mut(&mut self) -> &mut Blockchain<Self::Runtime, Self::ArchiveWasm> {
        &mut self.blockchain
    }

    fn transactions_by_hash(&self) -> &BTreeMap<HashOf<Self::Transaction>, BlockIndex> {
        &self.transactions_by_hash
    }

    fn transactions_by_hash_mut(&mut self) -> &mut BTreeMap<HashOf<Self::Transaction>, BlockIndex> {
        &mut self.transactions_by_hash
    }

    fn transactions_by_height(&self) -> &VecDeque<TransactionInfo<Self::Transaction>> {
        &self.transactions_by_height
    }

    fn transactions_by_height_mut(&mut self) -> &mut VecDeque<TransactionInfo<Self::Transaction>> {
        &mut self.transactions_by_height
    }

    fn on_purged_transaction(&mut self, _height: BlockIndex) {}

    fn fee_collector_mut(&mut self) -> Option<&mut FeeCollector<Self::AccountId>> {
        self.fee_collector.as_mut()
    }
}

impl<Tokens: TokensType> Ledger<Tokens> {
    pub fn minting_account(&self) -> &Account {
        &self.minting_account
    }

    pub fn transfer_fee(&self) -> Tokens {
        self.transfer_fee.clone()
    }

    pub fn burn_fee(&self) -> Tokens {
        self.burn_fee.clone()
    }

    pub fn transfer_fee_rate(&self) -> Tokens {
        self.transfer_fee_rate.clone()
    }

    pub fn burn_fee_rate(&self) -> Tokens {
        self.burn_fee_rate.clone()
    }

    pub fn max_memo_length(&self) -> u16 {
        self.max_memo_length
    }

    pub fn decimals(&self) -> u8 {
        self.decimals
    }

    pub fn metadata(&self) -> Vec<(String, Value)> {
        let mut records: Vec<(String, Value)> = self
            .metadata
            .clone()
            .into_iter()
            .map(|(k, v)| (k, StoredValue::into(v)))
            .collect();
        records.push(Value::entry("icrc1:decimals", self.decimals() as u64));
        records.push(Value::entry("icrc1:name", self.token_name()));
        records.push(Value::entry("icrc1:symbol", self.token_symbol()));
        records.push(Value::entry("icrc1:fee", self.transfer_fee().into()));
        records.push(Value::entry("icrc1:burn_fee", self.burn_fee().into()));
        records.push(Value::entry("icrc1:transfer_fee_rate", self.transfer_fee_rate().into()));
        records.push(Value::entry("icrc1:burn_fee_rate", self.burn_fee_rate().into()));
        records.push(Value::entry(
            "icrc1:max_memo_length",
            self.max_memo_length() as u64,
        ));
        records
    }

    pub fn feature_flags(&self) -> &FeatureFlags {
        &self.feature_flags
    }

    pub fn up_minting_account(&mut self,arg:Account){
        self.minting_account=arg;
    }

    pub fn upgrade(&mut self, sink: impl Sink + Clone, args: UpgradeArgs) {
        if let Some(upgrade_metadata_args) = args.metadata {
            self.metadata = upgrade_metadata_args
                .into_iter()
                .map(|(k, v)| (k, StoredValue::from(v)))
                .collect();
        }
        if let Some(token_name) = args.token_name {
            self.token_name = token_name;
        }
        if let Some(token_symbol) = args.token_symbol {
            self.token_symbol = token_symbol;
        }
        if let Some(burn_fee) = args.burn_fee {
            self.burn_fee = Tokens::try_from(burn_fee.clone()).unwrap_or_else(|e| {
                ic_cdk::trap(&format!(
                    "failed to convert burn fee {} to tokens: {}",
                    burn_fee, e
                ))
            });
        }
        if let Some(transfer_fee_rate) = args.transfer_fee_rate {
            self.transfer_fee_rate = Tokens::try_from(transfer_fee_rate.clone()).unwrap_or_else(|e| {
                ic_cdk::trap(&format!(
                    "failed to convert transfer fee rate {} to tokens: {}",
                    transfer_fee_rate, e
                ))
            });
        }
        if let Some(burn_fee_rate) = args.burn_fee_rate {
            self.burn_fee_rate = Tokens::try_from(burn_fee_rate.clone()).unwrap_or_else(|e| {
                ic_cdk::trap(&format!(
                    "failed to convert burn fee rate {} to tokens: {}",
                    burn_fee_rate, e
                ))
            });
        }
        if let Some(transfer_fee) = args.transfer_fee {
            self.transfer_fee = Tokens::try_from(transfer_fee.clone()).unwrap_or_else(|e| {
                ic_cdk::trap(&format!(
                    "failed to convert transfer fee {} to tokens: {}",
                    transfer_fee, e
                ))
            });
        }
        if let Some(max_memo_length) = args.max_memo_length {
            if self.max_memo_length > max_memo_length {
                ic_cdk::trap(&format!("The max len of the memo can be changed only to be bigger or equal than the current size. Current size: {}", self.max_memo_length));
            }
            self.max_memo_length = max_memo_length;
        }
        if let Some(change_fee_collector) = args.change_fee_collector {
            self.fee_collector = change_fee_collector.into();
            if self.fee_collector.as_ref().map(|fc| fc.fee_collector) == Some(self.minting_account)
            {
                ic_cdk::trap(
                    "The fee collector account cannot be the same account as the minting account",
                );
            }
        }
        if let Some(feature_flags) = args.feature_flags {
            if !feature_flags.icrc2 {
                log!(
                    sink,
                    "[ledger] feature flag icrc2 is deprecated and won't disable ICRC-2 anymore"
                );
            }
            self.feature_flags = feature_flags;
        }
        if let Some(maximum_number_of_accounts) = args.maximum_number_of_accounts {
            self.maximum_number_of_accounts = maximum_number_of_accounts.try_into().unwrap();
        }
        if let Some(accounts_overflow_trim_quantity) = args.accounts_overflow_trim_quantity {
            self.accounts_overflow_trim_quantity =
                accounts_overflow_trim_quantity.try_into().unwrap();
        }
    }

    /// Returns the root hash of the certified ledger state.
    /// The canister code must call set_certified_data with the value this function returns after
    /// each successful modification of the ledger.
    pub fn root_hash(&self) -> [u8; 32] {
        self.construct_hash_tree().digest().0
    }

    pub fn construct_hash_tree(&self) -> MixedHashTree {
        match self.blockchain().last_hash {
            Some(hash) => {
                let last_block_index = self.blockchain().chain_length().checked_sub(1).unwrap();
                MixedHashTree::Fork(Box::new((
                    MixedHashTree::Labeled(
                        Label::from("last_block_index"),
                        Box::new(MixedHashTree::Leaf(last_block_index.to_be_bytes().to_vec())),
                    ),
                    MixedHashTree::Labeled(
                        Label::from("tip_hash"),
                        Box::new(MixedHashTree::Leaf(hash.as_slice().to_vec())),
                    ),
                )))
            }
            None => MixedHashTree::Empty,
        }
    }

    fn query_blocks<ArchiveFn, B>(
        &self,
        start: BlockIndex,
        length: usize,
        decode: impl Fn(&EncodedBlock) -> B,
        make_callback: impl Fn(Principal) -> ArchiveFn,
    ) -> (u64, Vec<B>, Vec<ArchivedRange<ArchiveFn>>) {
        let locations = block_locations(self, start, length);

        let local_blocks_range =
            range_utils::take(&locations.local_blocks, MAX_TRANSACTIONS_PER_REQUEST);

        let local_blocks: Vec<B> = self
            .blockchain
            .block_slice(local_blocks_range)
            .iter()
            .map(decode)
            .collect();

        let archived_blocks = locations
            .archived_blocks
            .into_iter()
            .map(|(canister_id, slice)| ArchivedRange {
                start: Nat::from(slice.start),
                length: Nat::from(range_utils::range_len(&slice)),
                callback: make_callback(canister_id.get().0),
            })
            .collect();

        (locations.local_blocks.start, local_blocks, archived_blocks)
    }

    /// Returns transactions in the specified range.
    pub fn get_transactions(&self, start: BlockIndex, length: usize) -> GetTransactionsResponse {
        let (first_index, local_transactions, archived_transactions) = self.query_blocks(
            start,
            length,
            |enc_block| -> Tx {
                let decoded_block: Block<Tokens> =
                    Block::decode(enc_block.clone()).expect("bug: failed to decode encoded block");
                decoded_block.into()
            },
            |canister_id| QueryTxArchiveFn::new(canister_id, "get_transactions"),
        );

        GetTransactionsResponse {
            first_index: Nat::from(first_index),
            log_length: Nat::from(self.blockchain.chain_length()),
            transactions: local_transactions,
            archived_transactions,
        }
    }

    /// Returns blocks in the specified range.
    pub fn get_blocks(&self, start: BlockIndex, length: usize) -> GetBlocksResponse {
        let (first_index, local_blocks, archived_blocks) = self.query_blocks(
            start,
            length,
            encoded_block_to_generic_block,
            |canister_id| QueryBlockArchiveFn::new(canister_id, "get_blocks"),
        );

        GetBlocksResponse {
            first_index: Nat::from(first_index),
            chain_length: self.blockchain.chain_length(),
            certificate: ic_cdk::api::data_certificate().map(serde_bytes::ByteBuf::from),
            blocks: local_blocks,
            archived_blocks,
        }
    }
}
